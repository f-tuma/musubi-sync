//! Test kit for musubi-sync. Dev-dependency only (`publish = false`).
//!
//! K0 contains the skeleton of [`FakeProvider`]: an in-memory [`Provider`] and
//! [`PushSource`] with paging, resumable cursors and a delta log. Scripted
//! failures, notifications and call counters follow in K5; the fake Google
//! server in K3.

use std::collections::BTreeMap;
use std::num::NonZeroU32;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll};

use async_trait::async_trait;
use futures_core::Stream;
use http::{HeaderMap, HeaderValue, Uri};
use jiff::{SignedDuration, Timestamp};
use musubi_model::capability::{
    ConditionalWrite, DeltaSupport, Fidelity, InviteSuppression, KindCapabilities, Limits,
    ProviderId, PushSupport, RecurrenceSupport, ReminderCapabilities, SchedulingCapabilities,
    SchedulingMode, ZoneSupport,
};
use musubi_model::provider::ChannelId;
use musubi_model::provider::{
    AccessToken, BoxStream, Clock, Discovery, FetchHint, Limiter, Pass, PushHint, RemoteCollection,
    Start, Subscription,
};
use musubi_model::remote::RemoteRef;
use musubi_model::{
    AccountCx, ChangePage, CollectionRef, Cursor, EntityKind, Provider, ProviderDescriptor,
    PushRejected, PushSource, RemoteKey, RemoteObject, SyncError, TimeWindow,
};
use secrecy::{ExposeSecret, SecretString};

/// Header carrying the channel id in fake notifications.
pub const CHANNEL_ID_HEADER: &str = "x-fake-channel-id";
/// Header carrying the channel token in fake notifications.
pub const CHANNEL_TOKEN_HEADER: &str = "x-fake-channel-token";
/// Header carrying the resource state (`sync` for the handshake) in fake notifications.
pub const RESOURCE_STATE_HEADER: &str = "x-fake-resource-state";

/// An object stored in the fake provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakeObject {
    /// The object as a page delivers it.
    pub object: RemoteObject,
    /// Time span used by window reads; `None` never matches a window.
    pub span: Option<(Timestamp, Timestamp)>,
}

#[derive(Debug, Default)]
struct FakeCollection {
    meta: Option<RemoteCollection>,
    objects: BTreeMap<RemoteKey, FakeObject>,
    /// Version of the last change per key, deleted keys included.
    log: BTreeMap<RemoteKey, u64>,
}

#[derive(Debug, Default)]
struct State {
    version: u64,
    /// Delta cursors older than this are expired.
    oldest_cursor: u64,
    collections: BTreeMap<CollectionRef, FakeCollection>,
    channels: BTreeMap<ChannelId, CollectionRef>,
    next_channel: u64,
}

/// In-memory provider. Cheap to clone; clones share state.
#[derive(Debug, Clone)]
pub struct FakeProvider {
    state: Arc<Mutex<State>>,
    page_size: u32,
    empty_page_after_first: bool,
}

impl Default for FakeProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeProvider {
    /// An empty provider with page size 2.
    pub fn new() -> Self {
        Self {
            state: Arc::default(),
            page_size: 2,
            empty_page_after_first: false,
        }
    }

    /// Default page size (a hint's `page_size` overrides it).
    pub fn with_page_size(mut self, n: NonZeroU32) -> Self {
        self.page_size = n.get();
        self
    }

    /// Emit an empty, non-last page after the first page of every pass, as
    /// Google does with an empty page that still has `nextPageToken`.
    pub fn with_empty_page_after_first(mut self) -> Self {
        self.empty_page_after_first = true;
        self
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        // A panicking test thread cannot leave the maps half-updated (every
        // mutation is one insert/remove), so a poisoned lock is still usable.
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Adds or replaces a collection listed by `discover`.
    pub fn add_collection(&self, meta: RemoteCollection) {
        let mut s = self.lock();
        let key = meta.remote.clone();
        s.collections.entry(key).or_default().meta = Some(meta);
    }

    /// Inserts or replaces an object (creating the collection if needed, unlisted).
    pub fn put(&self, col: &CollectionRef, obj: FakeObject) {
        let mut s = self.lock();
        s.version += 1;
        let v = s.version;
        let c = s.collections.entry(col.clone()).or_default();
        c.log.insert(obj.object.key.clone(), v);
        c.objects.insert(obj.object.key.clone(), obj);
    }

    /// Deletes an object; a later delta reports it in `removals`.
    pub fn delete(&self, col: &CollectionRef, key: &RemoteKey) {
        let mut s = self.lock();
        s.version += 1;
        let v = s.version;
        let c = s.collections.entry(col.clone()).or_default();
        if c.objects.remove(key).is_some() {
            c.log.insert(key.clone(), v);
        }
    }

    /// Makes every delta cursor issued so far expire ([`SyncError::CursorExpired`]).
    pub fn expire_cursors(&self) {
        let mut s = self.lock();
        s.oldest_cursor = s.version + 1;
    }

    /// Headers of a notification for `sub`, as the provider would send them.
    pub fn notification(sub: &Subscription, handshake: bool) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (name, value) in [
            (CHANNEL_ID_HEADER, sub.channel.0.as_str()),
            (CHANNEL_TOKEN_HEADER, sub.token.expose_secret()),
            (
                RESOURCE_STATE_HEADER,
                if handshake { "sync" } else { "exists" },
            ),
        ] {
            if let Ok(v) = HeaderValue::from_str(value) {
                h.insert(name, v);
            }
        }
        h
    }

    /// Computes one page. `pos` is the decoded cursor or the start position.
    fn page(
        &self,
        col: &CollectionRef,
        pos: Pos,
        size: usize,
        empty_next: bool,
    ) -> Result<(ChangePage, Pos), SyncError> {
        let s = self.lock();
        let c = s
            .collections
            .get(col)
            .ok_or_else(|| SyncError::Permanent("unknown collection".into()))?;
        match pos {
            Pos::Full { .. } | Pos::Window { .. } if empty_next => {
                let full = matches!(pos, Pos::Full { .. });
                let page = ChangePage {
                    pass: if full { Pass::Full } else { Pass::Window },
                    upserts: Vec::new(),
                    removals: Vec::new(),
                    cursor: full.then(|| pos.encode()),
                    last: false,
                };
                Ok((page, pos))
            }
            Pos::Full { snapshot, offset } => {
                let upserts: Vec<_> = c
                    .objects
                    .values()
                    .skip(offset)
                    .take(size)
                    .map(|o| o.object.clone())
                    .collect();
                let next = offset + upserts.len();
                let last = next >= c.objects.len();
                let pos = if last {
                    Pos::Delta {
                        since: snapshot,
                        until: snapshot,
                        offset: 0,
                    }
                } else {
                    Pos::Full {
                        snapshot,
                        offset: next,
                    }
                };
                Ok((
                    ChangePage {
                        pass: Pass::Full,
                        upserts,
                        removals: Vec::new(),
                        cursor: Some(pos.encode()),
                        last,
                    },
                    pos,
                ))
            }
            Pos::Window { window, offset } => {
                let hits: Vec<_> = c
                    .objects
                    .values()
                    .filter(|o| o.span.is_some_and(|(a, b)| window.overlaps(a, b)))
                    .collect();
                let upserts: Vec<_> = hits
                    .iter()
                    .skip(offset)
                    .take(size)
                    .map(|o| o.object.clone())
                    .collect();
                let next = offset + upserts.len();
                let last = next >= hits.len();
                let page = ChangePage {
                    pass: Pass::Window,
                    upserts,
                    removals: Vec::new(),
                    cursor: None,
                    last,
                };
                Ok((
                    page,
                    Pos::Window {
                        window,
                        offset: next,
                    },
                ))
            }
            Pos::Delta {
                since,
                until,
                offset,
            } => {
                if since < s.oldest_cursor {
                    return Err(SyncError::CursorExpired);
                }
                // `until` == `since` marks a fresh delta: it covers changes up to now.
                let until = if until == since { s.version } else { until };
                let changed: Vec<_> = c
                    .log
                    .iter()
                    .filter(|(_, v)| **v > since && **v <= until)
                    .map(|(k, _)| k)
                    .collect();
                let mut upserts = Vec::new();
                let mut removals = Vec::new();
                for key in changed.iter().skip(offset).take(size) {
                    match c.objects.get(*key) {
                        Some(o) => upserts.push(o.object.clone()),
                        None => removals.push((*key).clone()),
                    }
                }
                let next = offset + upserts.len() + removals.len();
                let last = next >= changed.len();
                let pos = if last {
                    Pos::Delta {
                        since: until,
                        until,
                        offset: 0,
                    }
                } else {
                    Pos::Delta {
                        since,
                        until,
                        offset: next,
                    }
                };
                Ok((
                    ChangePage {
                        pass: Pass::Delta,
                        upserts,
                        removals,
                        cursor: Some(pos.encode()),
                        last,
                    },
                    pos,
                ))
            }
        }
    }
}

/// Decoded cursor: where a pass continues.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pos {
    /// Full read at `offset` in key order; ends with a delta cursor at `snapshot`.
    Full { snapshot: u64, offset: usize },
    /// Window read (never encoded: window pages carry no cursor).
    Window { window: TimeWindow, offset: usize },
    /// Changes with version in `(since, until]`, at `offset` in key order.
    Delta {
        since: u64,
        until: u64,
        offset: usize,
    },
}

impl Pos {
    fn encode(self) -> Cursor {
        let (tag, a, b, c) = match self {
            Pos::Full { snapshot, offset } => (b'F', snapshot, 0, offset as u64),
            Pos::Delta {
                since,
                until,
                offset,
            } => (b'D', since, until, offset as u64),
            Pos::Window { .. } => unreachable!("window pages carry no cursor"),
        };
        let mut v = vec![tag];
        for n in [a, b, c] {
            v.extend_from_slice(&n.to_be_bytes());
        }
        Cursor(v)
    }

    fn decode(c: &Cursor) -> Result<Self, SyncError> {
        let bad = || SyncError::Permanent("malformed fake cursor".into());
        let (tag, rest) = c.0.split_first().ok_or_else(bad)?;
        let n: Vec<u64> = rest
            .chunks(8)
            .map(|ch| ch.try_into().map(u64::from_be_bytes).map_err(|_| bad()))
            .collect::<Result<_, _>>()?;
        match (tag, n.as_slice()) {
            (b'F', [s, 0, o]) => Ok(Pos::Full {
                snapshot: *s,
                offset: usize::try_from(*o).map_err(|_| bad())?,
            }),
            (b'D', [s, u, o]) => Ok(Pos::Delta {
                since: *s,
                until: *u,
                offset: usize::try_from(*o).map_err(|_| bad())?,
            }),
            _ => Err(bad()),
        }
    }
}

/// Stream of pages; computes one page per poll, never pending.
struct Pages {
    provider: FakeProvider,
    col: CollectionRef,
    pos: Result<Pos, Option<SyncError>>,
    size: usize,
    emitted: usize,
    done: bool,
}

impl Stream for Pages {
    type Item = Result<ChangePage, SyncError>;

    fn poll_next(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.done {
            return Poll::Ready(None);
        }
        let pos = match &mut this.pos {
            Ok(p) => *p,
            Err(e) => {
                this.done = true;
                return Poll::Ready(e.take().map(Err));
            }
        };
        let empty = this.emitted == 1
            && this.provider.empty_page_after_first
            && !matches!(pos, Pos::Delta { .. });
        let item = this.provider.page(&this.col, pos, this.size, empty);
        this.emitted += 1;
        match &item {
            Ok((page, next)) => {
                this.done = page.last;
                this.pos = Ok(*next);
            }
            Err(_) => this.done = true,
        }
        Poll::Ready(Some(item.map(|(page, _)| page)))
    }
}

#[async_trait]
impl Provider for FakeProvider {
    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor {
            id: ProviderId("fake".into()),
            kinds: vec![KindCapabilities {
                kind: EntityKind::Event,
                read: true,
                write: false,
                conditional_update: ConditionalWrite::None,
                conditional_delete: ConditionalWrite::None,
                client_id: None,
            }],
            delta: DeltaSupport::SyncToken,
            push: PushSupport::Webhook,
            recurrence: RecurrenceSupport::Full,
            zones: vec![ZoneSupport::Iana],
            scheduling: SchedulingCapabilities {
                mode: SchedulingMode::None,
                invite_suppression: InviteSuppression::None,
                organizer_writes_reply: false,
            },
            field_fidelity: BTreeMap::from([("title".to_owned(), Fidelity::Exact)]),
            hidden_marker: false,
            reminders: ReminderCapabilities {
                relative: false,
                absolute: false,
                actions: Vec::new(),
                max_per_item: None,
            },
            tasks: None,
            limits: Limits {
                max_page_size: self.page_size,
                requests_per_minute: None,
                max_batch: None,
            },
        }
    }

    async fn discover(&self, _cx: &AccountCx) -> Result<Discovery, SyncError> {
        let s = self.lock();
        Ok(Discovery {
            collections: s
                .collections
                .values()
                .filter_map(|c| c.meta.clone())
                .collect(),
        })
    }

    fn changes<'a>(
        &'a self,
        _cx: &'a AccountCx,
        col: &'a CollectionRef,
        cursor: Option<Cursor>,
        hint: FetchHint,
    ) -> BoxStream<'a, Result<ChangePage, SyncError>> {
        let snapshot = self.lock().version;
        let pos = match (&cursor, hint.start) {
            (Some(c), _) => Pos::decode(c).map_err(Some),
            (None, Start::Full) => Ok(Pos::Full {
                snapshot,
                offset: 0,
            }),
            (None, Start::Window(window)) => Ok(Pos::Window { window, offset: 0 }),
        };
        let size = hint.page_size.map_or(self.page_size, NonZeroU32::get) as usize;
        Box::pin(Pages {
            provider: self.clone(),
            col: col.clone(),
            pos,
            size,
            emitted: 0,
            done: false,
        })
    }

    async fn fetch(
        &self,
        _cx: &AccountCx,
        refs: &[RemoteRef],
    ) -> Result<Vec<RemoteObject>, SyncError> {
        let s = self.lock();
        Ok(refs
            .iter()
            .filter_map(|r| s.collections.get(&r.collection)?.objects.get(&r.key))
            .map(|o| o.object.clone())
            .collect())
    }

    async fn refresh(&self, _refresh_token: &SecretString) -> Result<AccessToken, SyncError> {
        Ok(AccessToken {
            access: SecretString::from("fake-access-token"),
            expires_in: std::time::Duration::from_secs(3600),
            rotated_refresh: None,
        })
    }

    fn push(&self) -> Option<&dyn PushSource> {
        Some(self)
    }
}

#[async_trait]
impl PushSource for FakeProvider {
    async fn subscribe(
        &self,
        cx: &AccountCx,
        col: &CollectionRef,
        _callback: &Uri,
    ) -> Result<Subscription, SyncError> {
        let mut s = self.lock();
        s.next_channel += 1;
        let channel = ChannelId(format!("fake-channel-{}", s.next_channel));
        s.channels.insert(channel.clone(), col.clone());
        Ok(Subscription {
            token: SecretString::from(format!("token-{}", channel.0)),
            channel,
            collection: col.clone(),
            resource: format!("resource-{}", col.0),
            expiration: cx.clock.now() + SignedDuration::from_hours(24),
        })
    }

    async fn unsubscribe(&self, _cx: &AccountCx, sub: &Subscription) -> Result<(), SyncError> {
        self.lock().channels.remove(&sub.channel);
        Ok(())
    }

    fn channel_of(&self, headers: &HeaderMap) -> Option<ChannelId> {
        Some(ChannelId(
            headers.get(CHANNEL_ID_HEADER)?.to_str().ok()?.to_owned(),
        ))
    }

    fn verify(&self, headers: &HeaderMap, known: &Subscription) -> Result<PushHint, PushRejected> {
        let get = |name| {
            headers
                .get(name)
                .and_then(|v: &HeaderValue| v.to_str().ok())
                .ok_or_else(|| PushRejected::Malformed(format!("missing {name}")))
        };
        if get(CHANNEL_ID_HEADER)? != known.channel.0 {
            return Err(PushRejected::UnknownChannel);
        }
        if get(CHANNEL_TOKEN_HEADER)? != known.token.expose_secret() {
            return Err(PushRejected::BadToken);
        }
        Ok(if get(RESOURCE_STATE_HEADER)? == "sync" {
            PushHint::Handshake
        } else {
            PushHint::Changed
        })
    }
}

/// A limiter that never waits.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoLimit;

#[async_trait]
impl Limiter for NoLimit {
    async fn acquire(&self) {}
}

/// A clock standing still at a given instant.
#[derive(Debug, Clone, Copy)]
pub struct FixedClock(pub Timestamp);

impl Clock for FixedClock {
    fn now(&self) -> Timestamp {
        self.0
    }
}

/// Account context for tests: no rate limit, clock fixed at `now`.
pub fn account_cx(account: musubi_model::AccountId, now: Timestamp) -> AccountCx {
    AccountCx {
        account,
        token: SecretString::from("fake-access-token"),
        limiter: Arc::new(NoLimit),
        clock: Arc::new(FixedClock(now)),
    }
}

/// Polls a future that never returns `Pending` (everything in this crate).
///
/// shortcut: busy-polls with a no-op waker; use a real executor for futures that wait.
pub fn block_on<F: std::future::Future>(f: F) -> F::Output {
    let mut f = std::pin::pin!(f);
    let mut cx = Context::from_waker(std::task::Waker::noop());
    loop {
        if let Poll::Ready(v) = f.as_mut().poll(&mut cx) {
            return v;
        }
    }
}

/// Collects a stream of ready items (see [`block_on`]).
pub fn collect<S: Stream + Unpin>(mut s: S) -> Vec<S::Item> {
    let mut cx = Context::from_waker(std::task::Waker::noop());
    let mut out = Vec::new();
    while let Poll::Ready(Some(item)) = Pin::new(&mut s).poll_next(&mut cx) {
        out.push(item);
    }
    out
}
