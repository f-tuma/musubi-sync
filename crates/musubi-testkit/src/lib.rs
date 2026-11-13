//! Test kit for musubi-sync. Dev-dependency only (`publish = false`).
//!
//! [`FakeProvider`] is an in-memory [`Provider`] and [`PushSource`] with
//! paging, resumable cursors and a delta log, scripted failures
//! ([`FakeProvider::fail`]), gates holding full reads
//! ([`FakeProvider::hold_full_reads`]) and other calls ([`FakeProvider::hold`]), notifications and call counters
//! ([`FakeProvider::calls`]). Engine tests also drive it with
//! [`FakeProvider::unlist_collection`], [`FakeProvider::hide_from_reads`],
//! [`FakeProvider::hide_from_fetch`], [`FakeProvider::expire_cursors`],
//! [`FakeProvider::full_on_next_delta`], [`FakeProvider::set_unauthorized`],
//! [`FakeProvider::set_endpoint`] and [`FakeProvider::without_oauth`], read
//! [`FakeProvider::unsubscribed`], and use [`ManualClock`],
//! [`MemoryVault`], [`CanonicalCodec`] and [`event_object`].
//!
//! [`google`] is the in-process fake Google Calendar HTTP server (K3) that
//! the Google adapter's transport tests run against. [`harness`] runs the
//! whole `musubi-syncd` binary over it (crash tests, the `bench` binary).
//!
//! [`dav`] is the in-process fake CalDAV/CardDAV server, [`radicale`] runs a
//! local Radicale, and [`conformance`] holds the scenario format and the
//! `Target` trait of the conformance runner (`tests/conformance.rs`).

pub mod conformance;
pub mod dav;
pub mod google;
pub mod harness;
pub mod radicale;

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::future::Future;
use std::num::NonZeroU32;
use std::ops::Bound;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll};

use async_trait::async_trait;
use futures_core::Stream;
use http::{HeaderMap, HeaderValue, Uri};
use jiff::{SignedDuration, Timestamp};
use musubi_model::capability::{
    DeltaSupport, Fidelity, KindCapabilities, Limits, ProviderId, PushSupport, RecurrenceSupport,
    ZoneSupport,
};
use musubi_model::control::Credentials;
use musubi_model::provider::ChannelId;
use musubi_model::provider::Decoded;
use musubi_model::provider::{
    AccessToken, BoxStream, Clock, Credential, Discovery, Endpoint, FetchHint, Limiter, Pass,
    PushHint, RemoteCollection, Start, Stub, Subscription, TokenRefresh,
};
use musubi_model::remote::MediaType;
use musubi_model::remote::RemoteRef;
use musubi_model::{
    AccountCx, AccountId, CalendarEvent, ChangePage, Codec, CollectionRef, Cursor, EntityKind,
    Event, Provider, ProviderDescriptor, PushRejected, PushSource, Raw, RemoteKey, RemoteObject,
    StoreError, SyncError, TimeWindow, TokenVault, Validator,
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
    /// Keys left out of every page but still returned by `fetch`.
    hidden: BTreeSet<RemoteKey>,
    /// Keys still listed (stubs) but missing from `fetch`.
    unfetchable: BTreeSet<RemoteKey>,
}

/// A provider call that [`FakeProvider::fail`] can make fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Op {
    /// [`Provider::discover`].
    Discover,
    /// One page request of [`Provider::changes`] (any pass, any stream).
    Page,
    /// [`Provider::fetch`].
    Fetch,
    /// [`TokenRefresh::refresh`].
    Refresh,
    /// [`PushSource::subscribe`].
    Subscribe,
    /// [`PushSource::unsubscribe`].
    Unsubscribe,
}

/// How many times each call reached the provider (failed ones included).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Calls {
    /// `discover` calls.
    pub discover: u32,
    /// Streams started by `changes` from scratch for a full read.
    pub full_reads: u32,
    /// Streams started by `changes` from scratch for a window read.
    pub window_reads: u32,
    /// Streams started by `changes` with a cursor of a finished pass: delta requests.
    pub deltas: u32,
    /// Pages requested over all streams.
    pub pages: u32,
    /// `fetch` calls.
    pub fetch: u32,
    /// `refresh` calls.
    pub refresh: u32,
    /// `subscribe` calls.
    pub subscribe: u32,
    /// `unsubscribe` calls.
    pub unsubscribe: u32,
    /// Most `changes` streams open at the same time on one collection.
    pub max_open_reads: u32,
}

#[derive(Debug, Default)]
struct State {
    /// Scripted failures, consumed one per call of the op.
    failures: BTreeMap<Op, VecDeque<SyncError>>,
    calls: Calls,
    /// While set, full-read pages wait for a permit.
    full_gate: Option<Arc<tokio::sync::Semaphore>>,
    /// Held calls per op (see [`FakeProvider::hold`]).
    gates: BTreeMap<Op, Arc<tokio::sync::Semaphore>>,
    version: u64,
    /// Delta cursors older than this are expired.
    oldest_cursor: u64,
    collections: BTreeMap<CollectionRef, FakeCollection>,
    channels: BTreeMap<ChannelId, CollectionRef>,
    /// Scripted lost `subscribe` replies (see [`FakeProvider::lose_subscribe_reply`]).
    lost_subscribe: VecDeque<SyncError>,
    /// `changes` streams not yet dropped, per collection.
    open_reads: BTreeMap<CollectionRef, u32>,
    /// Kinds `discover` reports as unauthorised.
    unauthorized: Vec<EntityKind>,
    /// Endpoint update `discover` returns.
    endpoint: Option<Endpoint>,
    /// The next delta request is answered with a full listing.
    full_on_delta: bool,
    /// Every `unsubscribe` call: channel and resource id.
    unsubscribed: Vec<(ChannelId, String)>,
    /// The account context's endpoint of every `changes` call.
    read_endpoints: Vec<Option<Endpoint>>,
}

/// In-memory provider. Cheap to clone; clones share state.
///
/// The delta mechanism of a collection is its [`RemoteCollection::delta`], or
/// else its kind's [`KindCapabilities::delta`] in the descriptor. A collection
/// with [`DeltaSupport::CtagEtag`] lists the objects of a full read and of
/// a delta as [`ChangePage::stubs`] (key and validator, no body), as a DAV
/// server does; `fetch` returns the bodies. Every other collection sends bodies in
/// `upserts`.
#[derive(Debug, Clone)]
pub struct FakeProvider {
    state: Arc<Mutex<State>>,
    page_size: u32,
    empty_page_after_first: bool,
    descriptor: Option<Arc<ProviderDescriptor>>,
    oauth: bool,
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
            descriptor: None,
            oauth: true,
        }
    }

    /// Replaces the default descriptor (one kind, events, with sync-token
    /// delta and webhook push). Its `limits.max_page_size` is ignored: the
    /// page size stays [`FakeProvider::with_page_size`].
    pub fn with_descriptor(mut self, d: ProviderDescriptor) -> Self {
        self.descriptor = Some(Arc::new(d));
        self
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

    /// No [`TokenRefresh`]: `oauth()` returns `None`, as for a DAV account
    /// with a password.
    pub fn without_oauth(mut self) -> Self {
        self.oauth = false;
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

    /// The next call of `op` fails with `err` instead of running (queued in order).
    pub fn fail(&self, op: Op, err: SyncError) {
        self.lock().failures.entry(op).or_default().push_back(err);
    }

    /// The next `subscribe` opens its channel, then fails with `err`: the reply
    /// is lost, as after a timeout or a crash before the engine stores it.
    pub fn lose_subscribe_reply(&self, err: SyncError) {
        self.lock().lost_subscribe.push_back(err);
    }

    /// Call counters so far.
    pub fn calls(&self) -> Calls {
        self.lock().calls
    }

    /// Full-read pages wait from now on until [`FakeProvider::release_full_reads`].
    pub fn hold_full_reads(&self) {
        self.lock().full_gate = Some(Arc::new(tokio::sync::Semaphore::new(0)));
    }

    /// Lets held full reads continue and stops holding new ones.
    pub fn release_full_reads(&self) {
        if let Some(g) = self.lock().full_gate.take() {
            g.close();
        }
    }

    /// Calls of `op` (`Discover`, `Subscribe` or `Unsubscribe`) from now on are counted, then
    /// wait until [`FakeProvider::release`].
    pub fn hold(&self, op: Op) {
        self.lock()
            .gates
            .insert(op, Arc::new(tokio::sync::Semaphore::new(0)));
    }

    /// Lets held calls of `op` continue and stops holding new ones.
    pub fn release(&self, op: Op) {
        if let Some(g) = self.lock().gates.remove(&op) {
            g.close();
        }
    }

    /// Waits while `op` is held.
    async fn gate(&self, op: Op) {
        let gate = self.lock().gates.get(&op).cloned();
        if let Some(g) = gate {
            // Closed by `release`; no permit is ever added.
            let _ = g.acquire().await;
        }
    }

    /// Removes a collection from `discover` (its objects stay readable).
    pub fn unlist_collection(&self, col: &CollectionRef) {
        if let Some(c) = self.lock().collections.get_mut(col) {
            c.meta = None;
        }
    }

    /// Leaves `key` out of every page from now on (full, window and delta reads),
    /// while `fetch` still returns it: a listing that misses an object that exists.
    pub fn hide_from_reads(&self, col: &CollectionRef, key: &RemoteKey) {
        let mut s = self.lock();
        s.collections
            .entry(col.clone())
            .or_default()
            .hidden
            .insert(key.clone());
    }

    /// `fetch` leaves `key` out from now on, while pages still list it: an
    /// object deleted between the listing and the fetch.
    pub fn hide_from_fetch(&self, col: &CollectionRef, key: &RemoteKey) {
        let mut s = self.lock();
        s.collections
            .entry(col.clone())
            .or_default()
            .unfetchable
            .insert(key.clone());
    }

    /// `discover` reports these kinds as unauthorised from now on (and leaves
    /// their collections out of the listing).
    pub fn set_unauthorized(&self, kinds: Vec<EntityKind>) {
        self.lock().unauthorized = kinds;
    }

    /// `discover` returns this endpoint update from now on.
    pub fn set_endpoint(&self, endpoint: Option<Endpoint>) {
        self.lock().endpoint = endpoint;
    }

    /// The next delta request (a `changes` call with a delta cursor) is
    /// answered with a full listing (`Pass::Full` pages), as a server does
    /// when it no longer knows the sync token.
    pub fn full_on_next_delta(&self) {
        self.lock().full_on_delta = true;
    }

    /// Channel and resource id of every `unsubscribe` call so far.
    pub fn unsubscribed(&self) -> Vec<(ChannelId, String)> {
        self.lock().unsubscribed.clone()
    }

    /// The [`AccountCx::endpoint`] of every `changes` call so far.
    pub fn read_endpoints(&self) -> Vec<Option<Endpoint>> {
        self.lock().read_endpoints.clone()
    }

    /// Open watch channels.
    pub fn channels(&self) -> Vec<ChannelId> {
        self.lock().channels.keys().cloned().collect()
    }

    /// Counts a call of `op`; returns its scripted failure, if any.
    fn enter(&self, op: Op) -> Result<(), SyncError> {
        let mut s = self.lock();
        let c = &mut s.calls;
        match op {
            Op::Discover => c.discover += 1,
            Op::Page => c.pages += 1,
            Op::Fetch => c.fetch += 1,
            Op::Refresh => c.refresh += 1,
            Op::Subscribe => c.subscribe += 1,
            Op::Unsubscribe => c.unsubscribe += 1,
        }
        match s.failures.get_mut(&op).and_then(VecDeque::pop_front) {
            Some(e) => Err(e),
            None => Ok(()),
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
                let pass = if full { Pass::Full } else { Pass::Window };
                let page = ChangePage::new(pass, full.then(|| pos.encode()), false);
                Ok((page, pos))
            }
            Pos::Full { snapshot, after } => {
                // Keyset paging: keys inserted behind `after` during the read are
                // newer than `snapshot`, so the following delta delivers them.
                let mut rest = c
                    .objects
                    .range(past(&after))
                    .filter(|(k, _)| !c.hidden.contains(*k))
                    .map(|(_, o)| &o.object);
                let mut upserts: Vec<_> = rest.by_ref().take(size).cloned().collect();
                let last = rest.next().is_none();
                let after_key = upserts.last().map(|o| o.key.clone());
                let stubs = if self.delta_of(c) == Some(DeltaSupport::CtagEtag) {
                    std::mem::take(&mut upserts)
                        .into_iter()
                        .map(|o| Stub::new(o.key, Some(o.validator)))
                        .collect()
                } else {
                    Vec::new()
                };
                let pos = if last {
                    Pos::Delta {
                        since: snapshot,
                        until: snapshot,
                        after: None,
                    }
                } else {
                    Pos::Full {
                        snapshot,
                        after: after_key,
                    }
                };
                let mut page = ChangePage::new(Pass::Full, Some(pos.encode()), last);
                page.upserts = upserts;
                page.stubs = stubs;
                Ok((page, pos))
            }
            Pos::Window { window, after } => {
                let mut rest = c
                    .objects
                    .range(past(&after))
                    .filter(|(k, _)| !c.hidden.contains(*k))
                    .map(|(_, o)| o)
                    .filter(|o| o.span.is_some_and(|(a, b)| window.overlaps(a, b)))
                    .map(|o| &o.object);
                let upserts: Vec<_> = rest.by_ref().take(size).cloned().collect();
                let last = rest.next().is_none();
                let after = upserts.last().map(|o| o.key.clone()).or(after);
                let mut page = ChangePage::new(Pass::Window, None, last);
                page.upserts = upserts;
                Ok((page, Pos::Window { window, after }))
            }
            Pos::Delta {
                since,
                until,
                after,
            } => {
                if since < s.oldest_cursor {
                    return Err(SyncError::CursorExpired);
                }
                // `until` == `since` marks a fresh delta: it covers changes up to now.
                let until = if until == since { s.version } else { until };
                // A key changed again after `until` drops out here; the next delta
                // (from `until`) delivers it.
                let mut rest = c
                    .log
                    .range(past(&after))
                    .filter(|(k, v)| **v > since && **v <= until && !c.hidden.contains(*k))
                    .map(|(k, _)| k);
                let changed: Vec<_> = rest.by_ref().take(size).collect();
                let last = rest.next().is_none();
                let mut upserts = Vec::new();
                let mut removals = Vec::new();
                for key in &changed {
                    match c.objects.get(*key) {
                        Some(o) => upserts.push(o.object.clone()),
                        None => removals.push((*key).clone()),
                    }
                }
                let pos = if last {
                    Pos::Delta {
                        since: until,
                        until,
                        after: None,
                    }
                } else {
                    Pos::Delta {
                        since,
                        until,
                        after: changed.last().map(|k| (*k).clone()),
                    }
                };
                let mut page = ChangePage::new(Pass::Delta, Some(pos.encode()), last);
                if self.delta_of(c) == Some(DeltaSupport::CtagEtag) {
                    page.stubs = upserts
                        .into_iter()
                        .map(|o| Stub::new(o.key, Some(o.validator)))
                        .collect();
                } else {
                    page.upserts = upserts;
                }
                page.removals = removals;
                Ok((page, pos))
            }
        }
    }
}

impl FakeProvider {
    /// The collection's delta mechanism; `None` for an unlisted collection.
    fn delta_of(&self, c: &FakeCollection) -> Option<DeltaSupport> {
        let meta = c.meta.as_ref()?;
        meta.delta.or_else(|| {
            self.describe()
                .kinds
                .iter()
                .find(|k| k.kind == meta.kind)
                .map(|k| k.delta)
        })
    }

    fn describe(&self) -> Arc<ProviderDescriptor> {
        self.descriptor.clone().unwrap_or_else(|| {
            let events = KindCapabilities::new(
                EntityKind::Event,
                DeltaSupport::SyncToken,
                PushSupport::Webhook,
            );
            let mut d = ProviderDescriptor::new(
                ProviderId("fake".into()),
                vec![events],
                Limits::new(self.page_size),
            );
            d.recurrence = RecurrenceSupport::Full;
            d.zones = vec![ZoneSupport::Iana];
            d.field_fidelity = BTreeMap::from([("title".to_owned(), Fidelity::Exact)]);
            Arc::new(d)
        })
    }
}

/// Key range strictly after `after` (all keys for `None`).
fn past(after: &Option<RemoteKey>) -> (Bound<&RemoteKey>, Bound<&RemoteKey>) {
    (
        after.as_ref().map_or(Bound::Unbounded, Bound::Excluded),
        Bound::Unbounded,
    )
}

/// Decoded cursor: where a pass continues. Pages continue after the last key
/// delivered (keyset), so concurrent inserts and deletes never shift a page.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Pos {
    /// Full read in key order; ends with a delta cursor at `snapshot`.
    Full {
        snapshot: u64,
        after: Option<RemoteKey>,
    },
    /// Window read (never encoded: window pages carry no cursor).
    Window {
        window: TimeWindow,
        after: Option<RemoteKey>,
    },
    /// Changes with version in `(since, until]`, in key order.
    Delta {
        since: u64,
        until: u64,
        after: Option<RemoteKey>,
    },
}

impl Pos {
    /// Tag, two big-endian `u64`s, then `1` and the key, or `0`.
    fn encode(&self) -> Cursor {
        let (tag, a, b, after) = match self {
            Pos::Full { snapshot, after } => (b'F', *snapshot, 0, after),
            Pos::Delta {
                since,
                until,
                after,
            } => (b'D', *since, *until, after),
            Pos::Window { .. } => unreachable!("window pages carry no cursor"),
        };
        let mut v = vec![tag];
        v.extend_from_slice(&a.to_be_bytes());
        v.extend_from_slice(&b.to_be_bytes());
        match after {
            Some(k) => {
                v.push(1);
                v.extend_from_slice(k.0.as_bytes());
            }
            None => v.push(0),
        }
        Cursor(v)
    }

    fn decode(c: &Cursor) -> Result<Self, SyncError> {
        let bad = || SyncError::Permanent("malformed fake cursor".into());
        let (tag, rest) = c.0.split_first().ok_or_else(bad)?;
        if rest.len() < 17 {
            return Err(bad());
        }
        let (nums, rest) = rest.split_at(16);
        let a = u64::from_be_bytes(nums[..8].try_into().map_err(|_| bad())?);
        let b = u64::from_be_bytes(nums[8..].try_into().map_err(|_| bad())?);
        let after = match rest.split_first() {
            Some((0, [])) => None,
            Some((1, key)) => Some(RemoteKey(
                String::from_utf8(key.to_vec()).map_err(|_| bad())?,
            )),
            _ => return Err(bad()),
        };
        match tag {
            b'F' if b == 0 => Ok(Pos::Full { snapshot: a, after }),
            b'D' => Ok(Pos::Delta {
                since: a,
                until: b,
                after,
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
    /// Wait of a held full-read page (see [`FakeProvider::hold_full_reads`]).
    held: Option<Pin<Box<dyn Future<Output = ()> + Send>>>,
}

impl Drop for Pages {
    fn drop(&mut self) {
        if let Some(n) = self.provider.lock().open_reads.get_mut(&self.col) {
            *n -= 1;
        }
    }
}

impl Stream for Pages {
    type Item = Result<ChangePage, SyncError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.done {
            return Poll::Ready(None);
        }
        if matches!(this.pos, Ok(Pos::Full { .. }))
            && this.held.is_none()
            && let Some(g) = this.provider.lock().full_gate.clone()
        {
            // Closed by `release_full_reads`; no permit is ever added.
            this.held = Some(Box::pin(async move {
                let _ = g.acquire().await;
            }));
        }
        if let Some(h) = &mut this.held
            && h.as_mut().poll(cx).is_pending()
        {
            return Poll::Pending;
        }
        this.held = None;
        if let Err(e) = this.provider.enter(Op::Page) {
            this.done = true;
            return Poll::Ready(Some(Err(e)));
        }
        let pos = match &mut this.pos {
            Ok(p) => p.clone(),
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
                this.pos = Ok(next.clone());
            }
            Err(_) => this.done = true,
        }
        Poll::Ready(Some(item.map(|(page, _)| page)))
    }
}

#[async_trait]
impl Provider for FakeProvider {
    fn descriptor(&self) -> ProviderDescriptor {
        let mut d = (*self.describe()).clone();
        d.limits.max_page_size = self.page_size;
        d
    }

    async fn discover(&self, _cx: &AccountCx) -> Result<Discovery, SyncError> {
        self.enter(Op::Discover)?;
        self.gate(Op::Discover).await;
        let s = self.lock();
        let mut d = Discovery::new(
            s.collections
                .values()
                .filter_map(|c| c.meta.clone())
                .filter(|m| !s.unauthorized.contains(&m.kind))
                .collect(),
        );
        d.unauthorized = s.unauthorized.clone();
        d.endpoint = s.endpoint.clone();
        Ok(d)
    }

    fn changes<'a>(
        &'a self,
        cx: &'a AccountCx,
        col: &'a CollectionRef,
        cursor: Option<Cursor>,
        hint: FetchHint,
    ) -> BoxStream<'a, Result<ChangePage, SyncError>> {
        let mut s = self.lock();
        s.read_endpoints.push(cx.endpoint.clone());
        let snapshot = s.version;
        let restarted = cursor.is_some() && s.full_on_delta;
        let pos = match (&cursor, hint.start) {
            (Some(c), _) if restarted => {
                s.full_on_delta = false;
                s.calls.deltas += 1;
                Pos::decode(c).map_err(Some).map(|_| Pos::Full {
                    snapshot,
                    after: None,
                })
            }
            (Some(c), _) => Pos::decode(c).map_err(Some),
            (None, Start::Full) => Ok(Pos::Full {
                snapshot,
                after: None,
            }),
            (None, Start::Window(window)) => Ok(Pos::Window {
                window,
                after: None,
            }),
            (None, _) => Err(Some(SyncError::Permanent("unsupported start".into()))),
        };
        match &pos {
            _ if restarted => {}
            Ok(Pos::Full { after: None, .. }) => s.calls.full_reads += 1,
            Ok(Pos::Window { .. }) => s.calls.window_reads += 1,
            Ok(Pos::Delta { after: None, .. }) | Err(_) => s.calls.deltas += 1,
            Ok(_) => {}
        }
        let open = s.open_reads.entry(col.clone()).or_default();
        *open += 1;
        let open = *open;
        s.calls.max_open_reads = s.calls.max_open_reads.max(open);
        drop(s);
        let size = hint.page_size.map_or(self.page_size, NonZeroU32::get) as usize;
        Box::pin(Pages {
            provider: self.clone(),
            col: col.clone(),
            pos,
            size,
            emitted: 0,
            done: false,
            held: None,
        })
    }

    async fn fetch(
        &self,
        _cx: &AccountCx,
        refs: &[RemoteRef],
    ) -> Result<Vec<RemoteObject>, SyncError> {
        self.enter(Op::Fetch)?;
        let s = self.lock();
        Ok(refs
            .iter()
            .filter_map(|r| {
                let c = s.collections.get(&r.collection)?;
                c.objects
                    .get(&r.key)
                    .filter(|_| !c.unfetchable.contains(&r.key))
            })
            .map(|o| o.object.clone())
            .collect())
    }

    fn oauth(&self) -> Option<&dyn TokenRefresh> {
        self.oauth.then_some(self as &dyn TokenRefresh)
    }

    fn push(&self) -> Option<&dyn PushSource> {
        Some(self)
    }
}

#[async_trait]
impl TokenRefresh for FakeProvider {
    async fn refresh(&self, _refresh_token: &SecretString) -> Result<AccessToken, SyncError> {
        self.enter(Op::Refresh)?;
        Ok(AccessToken::new(
            SecretString::from("fake-access-token"),
            std::time::Duration::from_secs(3600),
        ))
    }
}

#[async_trait]
impl PushSource for FakeProvider {
    async fn subscribe(
        &self,
        cx: &AccountCx,
        col: &CollectionRef,
        _callback: &Uri,
        channel: &ChannelId,
    ) -> Result<Subscription, SyncError> {
        self.enter(Op::Subscribe)?;
        self.gate(Op::Subscribe).await;
        let mut s = self.lock();
        s.channels.insert(channel.clone(), col.clone());
        if let Some(e) = s.lost_subscribe.pop_front() {
            return Err(e);
        }
        Ok(Subscription::new(
            channel.clone(),
            col.clone(),
            fake_resource(col),
            SecretString::from(format!("token-{}", channel.0)),
            cx.clock.now() + SignedDuration::from_hours(24),
        ))
    }

    /// Like Google: any resource id of the channel's collection stops it; an
    /// unknown channel or a wrong resource changes nothing and is `Ok`.
    async fn unsubscribe(&self, _cx: &AccountCx, sub: &Subscription) -> Result<(), SyncError> {
        self.enter(Op::Unsubscribe)?;
        self.gate(Op::Unsubscribe).await;
        let mut s = self.lock();
        s.unsubscribed
            .push((sub.channel.clone(), sub.resource.clone()));
        if s.channels.get(&sub.channel).map(fake_resource) == Some(sub.resource.clone()) {
            s.channels.remove(&sub.channel);
        }
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

/// The resource id the fake gives every channel of `col`.
fn fake_resource(col: &CollectionRef) -> String {
    format!("resource-{}", col.0)
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

/// A clock that moves only when told to. Cheap to clone; clones share the time.
#[derive(Debug, Clone)]
pub struct ManualClock(Arc<Mutex<Timestamp>>);

impl ManualClock {
    /// A clock standing at `t`.
    pub fn new(t: Timestamp) -> Self {
        Self(Arc::new(Mutex::new(t)))
    }

    /// Moves the clock forward by `d`.
    pub fn advance(&self, d: SignedDuration) {
        let mut t = self.0.lock().unwrap_or_else(|e| e.into_inner());
        *t += d;
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Timestamp {
        *self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// A [`Codec<Event>`] whose raw body is the canonical [`CalendarEvent`] as JSON:
/// lets engine tests script events without a provider's wire format.
#[derive(Debug, Clone, Copy, Default)]
pub struct CanonicalCodec;

impl Codec<Event> for CanonicalCodec {
    const MAPPING_VERSION: u32 = 1;

    fn decode(raw: &Raw) -> Decoded<CalendarEvent> {
        match serde_json::from_slice(&raw.body) {
            Ok(ev) => Decoded::Canonical(ev),
            Err(e) => Decoded::Unsupported {
                reason: e.to_string(),
            },
        }
    }
}

/// A remote object carrying `ev` for [`CanonicalCodec`]; `master` is the
/// series master's key for a single instance of a series.
pub fn event_object(
    key: &str,
    validator: &str,
    ev: &CalendarEvent,
    master: Option<&str>,
) -> RemoteObject {
    let raw = Raw {
        media_type: MediaType::Json,
        body: serde_json::to_vec(ev).unwrap_or_default(),
    };
    let mut o = RemoteObject::new(
        RemoteKey(key.to_owned()),
        EntityKind::Event,
        Validator(validator.to_owned()),
        raw,
    );
    o.master = master.map(|m| RemoteKey(m.to_owned()));
    o
}

/// Account credentials in memory, any variant.
#[derive(Debug, Default)]
pub struct MemoryVault(Mutex<HashMap<AccountId, Credentials>>);

impl TokenVault for MemoryVault {
    fn load(&self, account: AccountId) -> Result<Option<Credentials>, StoreError> {
        let m = self.0.lock().unwrap_or_else(|e| e.into_inner());
        Ok(m.get(&account).cloned())
    }

    fn store(&self, account: AccountId, c: &Credentials) -> Result<(), StoreError> {
        let mut m = self.0.lock().unwrap_or_else(|e| e.into_inner());
        m.insert(account, c.clone());
        Ok(())
    }
}

/// Account context for tests: no rate limit, clock fixed at `now`.
pub fn account_cx(account: AccountId, now: Timestamp) -> AccountCx {
    AccountCx::new(
        account,
        Credential::Bearer(SecretString::from("fake-access-token")),
        Arc::new(NoLimit),
        Arc::new(FixedClock(now)),
    )
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
