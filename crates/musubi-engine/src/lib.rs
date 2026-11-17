//! Read path of the sync core: account state machine, scheduler over the
//! control store's job queue, one writer per tenant, page ingest with
//! occurrence expansion, push coalescing, channel renewal and the moving
//! occurrence index horizon.
//!
//! The engine works against the traits in `musubi-model` only: it must not
//! depend on `musubi-store-sqlite` or `musubi-google` outside
//! `[dev-dependencies]`, and never on `musubi-google`.
//!
//! Every committed page is reported to the [`CommitSink`] given at
//! construction; the HTTP layer connects it to its change bus.

use std::collections::HashMap;
use std::num::NonZeroU32;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use http::Uri;
use jiff::Timestamp;
use musubi_model::capability::ProviderId;
use musubi_model::control::{
    AccountRecord, AccountState, ChannelRecord, Job, JobKind, JobSpec, Lane,
};
use musubi_model::provider::{Clock, Limiter};
use musubi_model::{
    AccountId, Codec, CollectionId, Contact, ContactGroup, ControlStore, Decoded, Event, Kind,
    Provider, Raw, Seq, Storage, StoreError, SyncError, Task, TenantId, TenantStore, TokenVault,
};
use tokio::sync::{Notify, Semaphore};

mod ingest;
mod jobs;
mod redecode;
mod writer;

use writer::Writer;

/// Called by the tenant writer after every commit with the tenant's new head.
pub type CommitSink = Arc<dyn Fn(TenantId, Seq) + Send + Sync>;

/// How long push notifications for one collection are coalesced into one delta.
pub const COALESCE: Duration = Duration::from_millis(150);

/// [`musubi_model::CollectionError::message`] of a collection whose kind the
/// account has no permission for ([`SyncError::MissingScope`], plan 3.5). It
/// is permanent: the collection is not retried until a complete discovery
/// finds the kind authorised again, which clears exactly this error.
pub const MISSING_SCOPE: &str = "missing_scope";

/// Engine failure.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    /// The store failed.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// The provider failed.
    #[error(transparent)]
    Sync(#[from] SyncError),
    /// The engine or a tenant writer is shutting down.
    #[error("engine stopped")]
    Stopped,
}

/// A typed decoder of one kind: the [`Codec`] of a provider without its type.
pub struct DecodeFn<K: Kind> {
    /// [`Codec::MAPPING_VERSION`].
    pub mapping_version: u32,
    /// [`Codec::decode`].
    pub decode: fn(&Raw) -> Decoded<K::Canonical>,
}

impl<K: Kind> DecodeFn<K> {
    /// The decoder of codec `C`.
    pub fn of<C: Codec<K>>() -> Self {
        Self {
            mapping_version: C::MAPPING_VERSION,
            decode: C::decode,
        }
    }
}

// Manual: a derive would require `K: Clone`.
impl<K: Kind> Clone for DecodeFn<K> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<K: Kind> Copy for DecodeFn<K> {}

/// A provider's decoders by kind. An object of a kind without one is stored
/// raw only (`canonical: None`), as for [`Decoded::Unsupported`].
#[derive(Clone, Copy, Default)]
pub struct Codecs {
    /// Events.
    pub event: Option<DecodeFn<Event>>,
    /// Tasks.
    pub task: Option<DecodeFn<Task>>,
    /// Contacts.
    pub contact: Option<DecodeFn<Contact>>,
    /// Contact groups.
    pub contact_group: Option<DecodeFn<ContactGroup>>,
}

/// The providers an engine serves, by [`ProviderId`]; an account's provider
/// selects its instance (plan 3.6). The engine never branches on the id.
#[derive(Clone, Default)]
pub struct Providers {
    map: HashMap<ProviderId, (Arc<dyn Provider>, Codecs)>,
}

impl Providers {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds `provider` under its descriptor's id, replacing one with that id.
    pub fn with(mut self, provider: Arc<dyn Provider>, codecs: Codecs) -> Self {
        self.map
            .insert(provider.descriptor().id.clone(), (provider, codecs));
        self
    }

    fn get(&self, id: &ProviderId) -> Option<&(Arc<dyn Provider>, Codecs)> {
        self.map.get(id)
    }
}

/// Engine tuning.
#[derive(Debug, Clone)]
pub struct Config {
    /// Window read on connect: this far back from now.
    pub window_back: Duration,
    /// Window read on connect: this far ahead of now.
    pub window_forward: Duration,
    /// Occurrence index horizon: this far ahead of now.
    pub index_forward: Duration,
    /// Most index rows one series may produce in one expansion.
    pub index_row_limit: usize,
    /// Page size hint for the provider.
    pub page_size: Option<NonZeroU32>,
    /// Job lease, extended while the job runs.
    pub lease_for: Duration,
    /// Safety poll of a collection without a push channel.
    pub poll_every: Duration,
    /// Safety poll of a collection with a push channel.
    pub poll_every_push: Duration,
    /// Rediscovery of an account's collections.
    pub rediscover_every: Duration,
    /// Moving the occurrence index horizon.
    pub extend_every: Duration,
    /// Push callback; `None` disables push channels.
    pub callback: Option<Uri>,
    /// More items missing from a full read than this are verified one by one
    /// before they are deleted (mass-deletion guard).
    pub guard_threshold: u64,
    /// Items verified per provider call by the guard.
    pub guard_batch: u32,
    /// Concurrent jobs of the sync lane.
    pub sync_concurrency: usize,
    /// Concurrent jobs of the background lane.
    pub background_concurrency: usize,
    /// How often an idle scheduler looks for due jobs.
    pub tick: Duration,
    /// A tenant writer without a write for this long drops its store handle.
    pub writer_idle: Duration,
    /// Adaptive polling (plan 7.2) of a collection without an open push
    /// channel while its tenant is active (touched in the last
    /// [`Config::active_for`]).
    pub poll_active: PollBounds,
    /// Adaptive polling while the tenant is neither active nor sleeping.
    pub poll_normal: PollBounds,
    /// Adaptive polling of a tenant not touched for [`Config::sleep_after`].
    pub poll_sleeping: PollBounds,
    /// A tenant is active this long after [`Engine::touch`].
    pub active_for: Duration,
    /// A tenant not touched for this long sleeps (counted from the start).
    pub sleep_after: Duration,
}

/// Bounds of an adaptive poll interval: `min · 2^quiet`, at most `max`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PollBounds {
    /// Interval right after a change.
    pub min: Duration,
    /// Interval after many reads without a change.
    pub max: Duration,
}

const DAY: Duration = Duration::from_secs(86_400);

impl Default for Config {
    fn default() -> Self {
        Self {
            window_back: 30 * DAY,
            window_forward: 90 * DAY,
            index_forward: 5 * 365 * DAY,
            index_row_limit: 50_000,
            page_size: None,
            lease_for: Duration::from_secs(60),
            poll_every: Duration::from_secs(300),
            poll_every_push: Duration::from_secs(3600),
            rediscover_every: Duration::from_secs(6 * 3600),
            extend_every: DAY,
            callback: None,
            guard_threshold: 20,
            guard_batch: 50,
            sync_concurrency: 16,
            background_concurrency: 4,
            tick: Duration::from_millis(50),
            writer_idle: Duration::from_secs(30),
            poll_active: PollBounds {
                min: Duration::from_secs(15),
                max: Duration::from_secs(60),
            },
            poll_normal: PollBounds {
                min: Duration::from_secs(120),
                max: Duration::from_secs(900),
            },
            poll_sleeping: PollBounds {
                min: Duration::from_secs(3600),
                max: Duration::from_secs(6 * 3600),
            },
            active_for: Duration::from_secs(120),
            sleep_after: 7 * DAY,
        }
    }
}

/// The sync engine. Cheap to clone; clones share all state.
pub struct Engine<S: Storage> {
    inner: Arc<Inner<S>>,
}

impl<S: Storage> Clone for Engine<S> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

struct Inner<S: Storage> {
    storage: Arc<S>,
    providers: Providers,
    control: Arc<dyn ControlStore>,
    vault: Arc<dyn TokenVault>,
    clock: Arc<dyn Clock>,
    sink: CommitSink,
    config: Config,
    writers: Mutex<HashMap<TenantId, Arc<Writer<S::Tenant>>>>,
    /// Wakes a lane (by [`lane_ix`]) before its next tick.
    wake: [Notify; 2],
    // shortcut: in-process only; two engine processes on one control store could
    // run two jobs of one collection at once. Upgrade to a lease per collection then.
    locks: Mutex<HashMap<CollectionId, Arc<tokio::sync::Mutex<()>>>>,
    // Same shortcut: held by discovery, and by removal while it marks the account.
    account_locks: Mutex<HashMap<AccountId, Arc<tokio::sync::Mutex<()>>>>,
    accounts: Mutex<HashMap<AccountId, AccountRt>>,
    /// Reconnects per account (see [`Engine::account_writes`]); in-process
    /// like the jobs it guards against.
    reconnects: Arc<Mutex<HashMap<AccountId, u64>>>,
    // Same shortcut, and lost on restart: one extra full read or watch request per restart.
    collections: Mutex<HashMap<CollectionId, CollectionRt>>,
    /// The last [`Engine::touch`] of each tenant. Lost on restart by design
    /// (plan 7.3): at most one normal interval.
    interest: Mutex<HashMap<TenantId, Timestamp>>,
    /// Start of this process: interest is counted from it (plan 7.2).
    started: Timestamp,
}

/// In-memory per-account state: request pacing, a rate-limit block and
/// whether the last attempt ended `Unauthorized` despite a fresh token.
#[derive(Clone)]
struct AccountRt {
    limiter: Arc<dyn Limiter>,
    /// `limits.max_concurrency_per_account` (plan 7.4); `None` without a limit.
    running: Option<Arc<Semaphore>>,
    blocked_until: Option<Timestamp>,
    unauthorized: bool,
    /// Discoveries started since the engine started.
    discoveries: u64,
    /// The start number (of `discoveries`) of the last discovery that worked.
    discovered: u64,
}

/// In-memory per-collection state: full reads ended since the last delta
/// that worked, until when the provider is taken to reject the cursor a full
/// read ends with (each sync is then a full read), and until when a refused
/// push channel is not asked for again.
#[derive(Default)]
struct CollectionRt {
    full_reads: u32,
    full_only_until: Option<Timestamp>,
    push_refused_until: Option<Timestamp>,
    /// Reads in a row without a change (plan 7.2).
    quiet: u32,
    /// The running read wrote a change (an upsert or a removal).
    changed: bool,
    /// The provider gives synthetic `musubi:sha256:` validators (plan 7.2).
    hashed: bool,
    /// When a read of it last asked for a rediscovery, with the account's
    /// [`AccountRt::discoveries`] then; cleared by a read that works.
    rediscovered: Option<(Timestamp, u64)>,
}

/// The guard of [`Engine::account_writes`].
pub struct AccountWrites<'a>(std::sync::MutexGuard<'a, HashMap<AccountId, u64>>);

impl AccountWrites<'_> {
    /// Marks `account` as reconnected: its running jobs store no account
    /// field from now on.
    pub fn reconnected(&mut self, account: AccountId) {
        *self.0.entry(account).or_default() += 1;
    }
}

/// Background scheduler started by [`Engine::start`]; stops when dropped.
pub struct Running {
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Drop for Running {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

impl<S: Storage + 'static> Engine<S> {
    /// Creates an engine. Nothing runs until [`Engine::start`] or
    /// [`Engine::run_until_idle`].
    pub fn new(
        storage: Arc<S>,
        providers: Providers,
        control: Arc<dyn ControlStore>,
        vault: Arc<dyn TokenVault>,
        clock: Arc<dyn Clock>,
        sink: CommitSink,
        config: Config,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                started: clock.now(),
                storage,
                providers,
                control,
                vault,
                clock,
                sink,
                config,
                writers: Default::default(),
                wake: Default::default(),
                locks: Default::default(),
                account_locks: Default::default(),
                accounts: Default::default(),
                reconnects: Default::default(),
                collections: Default::default(),
                interest: Default::default(),
            }),
        }
    }

    /// Starts discovery of an account left in `Connecting` by the OAuth
    /// callback. Discovery moves it to `Active` and schedules its collections.
    pub async fn connect(&self, account: AccountId) -> Result<(), EngineError> {
        let rec = self.account(account).await?.ok_or(StoreError::NotFound)?;
        self.enqueue(discover_job(rec.tenant, account, self.now()))
            .await
    }

    /// The lock over account writes, held by a reconnect (`POST
    /// /v1/accounts/dav` with `account`, the OAuth callback) while it stores
    /// the account's tokens, endpoint or state, after
    /// [`AccountWrites::reconnected`]. A job of the account that started
    /// before then stores none of them over the reconnect's.
    pub fn account_writes(&self) -> AccountWrites<'_> {
        AccountWrites(
            self.inner
                .reconnects
                .lock()
                .unwrap_or_else(|e| e.into_inner()),
        )
    }

    /// Records that the tenant's user is looking at its data (`POST
    /// /v1/interest`, plan 7.3): the adaptive poll uses the active bounds for
    /// [`Config::active_for`]. A switch to active queues a sync of every
    /// collection without an open push channel (and not hashed), due now; a
    /// touch while the tenant is active queues nothing.
    pub async fn touch(&self, tenant: TenantId) -> Result<(), EngineError> {
        let now = self.now();
        let prev = self
            .inner
            .interest
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(tenant, now);
        if prev.is_some_and(|t| after(t, self.inner.config.active_for) > now) {
            return Ok(());
        }
        let channels = self.ctl(move |c| c.channels(tenant, None)).await?;
        for col in self.collections(tenant).await? {
            let pushed = channels
                .iter()
                .any(|ch| ch.collection == col.id && ch.open().is_some());
            let scoped = col
                .status
                .error
                .as_ref()
                .is_some_and(|e| e.message == MISSING_SCOPE);
            // A hashed collection downloads every body per read: its poll
            // never speeds up (plan 7.2), so a touch does not read it either.
            let hashed = self.with_col(col.id, |r| r.hashed);
            if col.gone || pushed || scoped || hashed {
                continue;
            }
            self.enqueue(jobs::sync_job(tenant, col.account, col.id, Lane::Sync, now))
                .await?;
        }
        Ok(())
    }

    /// The job a push notification for `collection` enqueues: a delta of the
    /// collection, due after the coalescing delay. Notifications arriving while
    /// it waits merge into it, so a burst costs one delta request.
    pub fn push_job(
        tenant: TenantId,
        account: AccountId,
        collection: CollectionId,
        now: Timestamp,
    ) -> JobSpec {
        JobSpec {
            tenant,
            lane: Lane::Sync,
            kind: JobKind::Sync {
                account,
                collection,
            },
            priority: 0,
            due: after(now, COALESCE),
            dedup: Some(sync_dedup(collection)),
        }
    }

    /// Handles a verified push notification on `channel`.
    pub async fn on_push(&self, channel: &ChannelRecord) -> Result<(), EngineError> {
        let spec = Self::push_job(
            channel.tenant,
            channel.account,
            channel.collection,
            self.now(),
        );
        self.enqueue(spec).await
    }

    /// Removes an account (plan 3.3): marks it `Removing`, purges its
    /// collections, stops its push channels at the provider while its tokens
    /// still exist, then deletes it with its tokens, jobs and channels.
    ///
    /// A process that dies midway continues the removal from the account's
    /// `Discover` job, which a `Removing` account runs as the removal.
    pub async fn remove_account(&self, account: AccountId) -> Result<(), EngineError> {
        let Some(rec) = self.account(account).await? else {
            return Ok(());
        };
        {
            // A running discovery finishes first; later ones see `Removing`.
            let _lock = self.account_lock(account).lock_owned().await;
            self.ctl(move |c| c.set_account_state(account, AccountState::Removing))
                .await?;
            // The resume point; due after this call normally finished and deleted it.
            let due = after(self.now(), self.inner.config.lease_for);
            self.enqueue(discover_job(rec.tenant, account, due)).await?;
        }
        self.removal(&rec).await
    }

    /// The steps of [`Engine::remove_account`] after the state change; each
    /// one can run again, also concurrently with a resumed removal, so what a
    /// step finds already gone counts as done.
    pub(crate) async fn removal(&self, rec: &AccountRecord) -> Result<(), EngineError> {
        let (tenant, account) = (rec.tenant, rec.id);
        let mut cx = None;
        // Under the collection lock: a running job finishes first (including a
        // channel it opens), and later ones find the collection gone. Its
        // channels are stopped before the purge, which takes the stored
        // `push_resource` a lone pending channel is stopped by.
        let cols = self.collections(tenant).await?;
        for col in cols.into_iter().filter(|c| c.account == account) {
            let _lock = self.lock(col.id).lock_owned().await;
            let mut chs = self.ctl(move |c| c.channels(tenant, Some(account))).await?;
            chs.retain(|ch| ch.collection == col.id);
            self.stop_channels(rec, &mut cx, &chs, Some(&col)).await?;
            gone_ok(
                self.write(tenant, move |s| s.purge_collection(col.id).map(drop))
                    .await,
            )?;
        }
        let channels = self.ctl(move |c| c.channels(tenant, Some(account))).await?;
        self.stop_channels(rec, &mut cx, &channels, None).await?;
        gone_ok(self.ctl(move |c| c.delete_account(account)).await)?;
        self.inner
            .accounts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&account);
        Ok(())
    }

    /// [`Engine::drop_channels`] for a removal, with the account's context
    /// made on first use (`cx`). Without a grant the channels cannot be
    /// stopped and only expire; any other failure retries the removal, which
    /// keeps the tokens.
    async fn stop_channels(
        &self,
        rec: &AccountRecord,
        cx: &mut Option<Option<jobs::Cx>>,
        chs: &[ChannelRecord],
        col: Option<&musubi_model::store::CollectionState>,
    ) -> Result<(), EngineError> {
        if chs.is_empty() {
            return Ok(());
        }
        if cx.is_none() {
            *cx = Some(match self.cx(rec, self.revision(rec.id), false).await {
                Ok(cx) => Some(cx),
                Err(EngineError::Sync(
                    e @ (SyncError::ReauthRequired { .. } | SyncError::Permanent(_)),
                )) => {
                    tracing::warn!(account = %rec.id, error = %e, "no grant to stop push channels with");
                    None
                }
                Err(e) => return Err(e),
            });
        }
        gone_ok(
            self.drop_channels(cx.as_ref().and_then(Option::as_ref), chs, col)
                .await,
        )
    }

    /// Stops the writer of a deleted tenant, so its store handle (and the
    /// unlinked files behind it) is released. Call after removing its accounts.
    pub fn forget_tenant(&self, tenant: TenantId) {
        self.writers().remove(&tenant);
    }

    /// Tenants with a writer (an open store handle); an idle one is dropped
    /// after [`Config::writer_idle`].
    pub fn active_writers(&self) -> usize {
        self.writers().len()
    }

    /// Runs due jobs one at a time, sync lane first, until none is due.
    /// Returns how many ran.
    pub async fn run_until_idle(&self) -> Result<usize, EngineError> {
        let mut ran = 0;
        loop {
            let mut job = None;
            for lane in [Lane::Sync, Lane::Background] {
                let (now, lease_for) = (self.now(), self.inner.config.lease_for);
                let mut jobs = self
                    .ctl(move |c| c.lease_due(lane, now, 1, lease_for))
                    .await?;
                if let Some(j) = jobs.pop() {
                    job = Some(j);
                    break;
                }
            }
            let Some(job) = job else { return Ok(ran) };
            self.execute(job).await;
            ran += 1;
        }
    }

    /// Queues a delta of every collection with an open push channel. Call
    /// once at startup: notifications sent while the node was down are lost
    /// (Google does not retry them). Returns how many were queued. Pending
    /// channels left by a crash are stopped by their collection's next sync.
    pub async fn catch_up(&self) -> Result<usize, EngineError> {
        let channels = self.ctl(|c| c.channels_expiring(Timestamp::MAX)).await?;
        let now = self.now();
        for ch in &channels {
            self.enqueue(Self::push_job(ch.tenant, ch.account, ch.collection, now))
                .await?;
        }
        Ok(channels.len())
    }

    /// Starts the scheduler: each lane leases due jobs up to its concurrency.
    pub fn start(&self) -> Running {
        let c = &self.inner.config;
        let lanes = [
            (Lane::Sync, c.sync_concurrency),
            (Lane::Background, c.background_concurrency),
        ];
        Running {
            tasks: lanes
                .into_iter()
                .map(|(lane, n)| tokio::spawn(self.clone().lane_loop(lane, n.max(1))))
                .collect(),
        }
    }

    async fn lane_loop(self, lane: Lane, concurrency: usize) {
        let slots = Arc::new(Semaphore::new(concurrency));
        loop {
            let free = slots.available_permits();
            if free > 0 {
                let (now, lease_for) = (self.now(), self.inner.config.lease_for);
                let limit = u32::try_from(free).unwrap_or(u32::MAX);
                match self
                    .ctl(move |c| c.lease_due(lane, now, limit, lease_for))
                    .await
                {
                    Ok(jobs) => {
                        for job in jobs {
                            let Ok(slot) = slots.clone().acquire_owned().await else {
                                return;
                            };
                            let engine = self.clone();
                            tokio::spawn(async move {
                                engine.execute(job).await;
                                drop(slot);
                            });
                        }
                    }
                    Err(e) => tracing::warn!(?lane, error = %e, "leasing jobs failed"),
                }
            }
            let wake = self.inner.wake[lane_ix(lane)].notified();
            let _ = tokio::time::timeout(self.inner.config.tick, wake).await;
        }
    }

    /// Runs one leased job while extending its lease, then completes or
    /// reschedules it. A lost lease abandons the job: another runner owns it.
    /// A panicking job is recorded and retried like a failed one.
    async fn execute(&self, job: Job) {
        let (id, lease) = (job.id, job.lease);
        let (engine, j) = (self.clone(), job.clone());
        // A task of its own so a panic is caught; aborted with `execute`.
        let mut run = AbortOnDrop(tokio::spawn(async move { engine.run_job(&j).await }));
        let beat = std::pin::pin!(self.heartbeat(&job));
        let outcome = match futures_util::future::select(&mut run.0, beat).await {
            futures_util::future::Either::Left((Ok(o), _)) => o,
            futures_util::future::Either::Left((Err(e), _)) => {
                let reason = match e.try_into_panic() {
                    Ok(p) => p
                        .downcast_ref::<&str>()
                        .map(|s| s.to_string())
                        .or_else(|| p.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "unknown panic".into()),
                    Err(e) => e.to_string(),
                };
                self.panicked(&job, format!("job panicked: {reason}")).await
            }
            futures_util::future::Either::Right(((), _)) => return,
        };
        let res = match outcome {
            jobs::Outcome::Done => self.ctl(move |c| c.complete(id, lease)).await,
            jobs::Outcome::Retry { due, count } => {
                self.ctl(move |c| c.reschedule(id, lease, due, count)).await
            }
        };
        if let Err(e) = res {
            tracing::warn!(job = id.0, error = %e, "finishing job failed");
        }
    }

    async fn heartbeat(&self, job: &Job) {
        let lease_for = self.inner.config.lease_for;
        let (id, lease) = (job.id, job.lease);
        loop {
            tokio::time::sleep(lease_for / 3).await;
            let now = self.now();
            if let Err(e) = self
                .ctl(move |c| c.extend_lease(id, lease, now, lease_for))
                .await
            {
                tracing::warn!(job = id.0, error = %e, "lease lost; job abandoned");
                return;
            }
        }
    }

    fn now(&self) -> Timestamp {
        self.inner.clock.now()
    }

    /// The occurrence index bounds of a decode now.
    fn index(&self) -> ingest::Index {
        let c = &self.inner.config;
        let now = self.now();
        ingest::Index {
            horizon: after(now, c.index_forward),
            fallback: after(now, c.window_forward),
            limit: c.index_row_limit,
        }
    }

    async fn enqueue(&self, spec: JobSpec) -> Result<(), EngineError> {
        let (lane, due) = (spec.lane, spec.due);
        self.ctl(move |c| c.enqueue(&spec).map(drop)).await?;
        // A job due soon (a coalesced push) runs when due, not up to a tick
        // later; later jobs are found by the tick.
        let wait = due.duration_since(self.now());
        if wait <= jiff::SignedDuration::from_secs(1) {
            let wait = Duration::try_from(wait).unwrap_or_default();
            let inner = Arc::downgrade(&self.inner);
            tokio::spawn(async move {
                tokio::time::sleep(wait).await;
                if let Some(inner) = inner.upgrade() {
                    inner.wake[lane_ix(lane)].notify_one();
                }
            });
        }
        Ok(())
    }

    async fn account(&self, account: AccountId) -> Result<Option<AccountRecord>, EngineError> {
        self.ctl(move |c| c.account(account)).await
    }

    /// Runs a control store call off the async runtime.
    async fn ctl<R: Send + 'static>(
        &self,
        f: impl FnOnce(&dyn ControlStore) -> Result<R, StoreError> + Send + 'static,
    ) -> Result<R, EngineError> {
        let control = self.inner.control.clone();
        blocking(move || Ok(f(&*control)?)).await
    }

    /// Runs a read on its own store handle off the async runtime.
    async fn read<R: Send + 'static>(
        &self,
        tenant: TenantId,
        f: impl FnOnce(&S::Tenant) -> Result<R, StoreError> + Send + 'static,
    ) -> Result<R, EngineError> {
        let storage = self.inner.storage.clone();
        blocking(move || Ok(f(&storage.open(tenant)?)?)).await
    }

    /// Runs a write on the tenant's writer.
    async fn write<R: Send + 'static>(
        &self,
        tenant: TenantId,
        f: impl FnOnce(&mut S::Tenant) -> Result<R, StoreError> + Send + 'static,
    ) -> Result<R, EngineError> {
        let storage = self.inner.storage.clone();
        let open = move || storage.open(tenant);
        writer::run(
            self.writer(tenant),
            tenant,
            open,
            self.inner.sink.clone(),
            f,
        )
        .await
    }

    /// The tenant's writer; a new one is dropped after [`Config::writer_idle`]
    /// without a write.
    fn writer(&self, tenant: TenantId) -> Arc<Writer<S::Tenant>> {
        let mut writers = self.writers();
        if let Some(w) = writers.get(&tenant) {
            return w.clone();
        }
        let w = Arc::new(writer::new());
        writers.insert(tenant, w.clone());
        let (inner, weak) = (Arc::downgrade(&self.inner), Arc::downgrade(&w));
        let idle = self.inner.config.writer_idle;
        tokio::spawn(async move {
            let mut wait = idle;
            loop {
                tokio::time::sleep(wait).await;
                let (Some(inner), Some(w)) = (inner.upgrade(), weak.upgrade()) else {
                    return;
                };
                let mut writers = inner.writers.lock().unwrap_or_else(|e| e.into_inner());
                if !writers.get(&tenant).is_some_and(|m| Arc::ptr_eq(m, &w)) {
                    return; // forgotten
                }
                // Only the map and `w` hold it: no write holds or awaits it, and
                // the next one must take the map lock to get it.
                wait = match w.try_lock() {
                    Ok(st) if Arc::strong_count(&w) == 2 => idle.saturating_sub(st.used.elapsed()),
                    _ => idle,
                };
                if wait.is_zero() {
                    writers.remove(&tenant);
                    drop(writers);
                    // Closing the last handle of a tenant may checkpoint its WAL.
                    tokio::task::spawn_blocking(move || drop(w));
                    return;
                }
            }
        });
        w
    }

    fn writers(&self) -> std::sync::MutexGuard<'_, HashMap<TenantId, Arc<Writer<S::Tenant>>>> {
        self.inner.writers.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Runs blocking work on the blocking pool; a panic there resumes here.
struct AbortOnDrop<T>(tokio::task::JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn blocking<R: Send + 'static>(
    f: impl FnOnce() -> Result<R, EngineError> + Send + 'static,
) -> Result<R, EngineError> {
    match tokio::task::spawn_blocking(f).await {
        Ok(r) => r,
        Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
        Err(_) => Err(EngineError::Stopped),
    }
}

fn lane_ix(lane: Lane) -> usize {
    match lane {
        Lane::Sync => 0,
        Lane::Background => 1,
    }
}

fn after(t: Timestamp, d: Duration) -> Timestamp {
    t.saturating_add(d).unwrap_or(Timestamp::MAX)
}

/// `NotFound` from a step that deletes something means it is done already.
fn gone_ok(r: Result<(), EngineError>) -> Result<(), EngineError> {
    match r {
        Err(EngineError::Store(StoreError::NotFound)) => Ok(()),
        r => r,
    }
}

/// The account's discovery job, due at `due`; runs before its collections'.
fn discover_job(tenant: TenantId, account: AccountId, due: Timestamp) -> JobSpec {
    JobSpec {
        tenant,
        lane: Lane::Sync,
        kind: JobKind::Discover { account },
        priority: 10,
        due,
        dedup: Some(format!("discover:{account}")),
    }
}

fn sync_dedup(collection: CollectionId) -> String {
    format!("sync:{collection}")
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

    use musubi_model::provider::{Access, RemoteCollection};
    use musubi_model::store::DiscoveryWrite;
    use musubi_model::{CollectionRef, EntityKind};
    use musubi_store_sqlite::{SqliteControl, SqliteStorage};
    use musubi_testkit::{CanonicalCodec, FakeProvider, ManualClock, MemoryVault};

    use super::*;

    const TENANT: TenantId = TenantId::from_u128(0x7e);

    fn collection(name: String) -> RemoteCollection {
        RemoteCollection::new(
            CollectionRef(name.clone()),
            EntityKind::Event,
            name,
            Access::Owner,
        )
    }

    /// A fresh store directory under the temp dir.
    fn temp_dir(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "musubi-{name}-{}-{}",
            std::process::id(),
            Timestamp::now().as_nanosecond()
        ))
    }

    /// An engine over `storage` whose writers are dropped after 1 ms idle.
    fn engine(
        dir: &std::path::Path,
        storage: Arc<SqliteStorage>,
        sink: CommitSink,
    ) -> Engine<SqliteStorage> {
        let control = Arc::new(SqliteControl::open(&dir.join("control.db")).unwrap());
        let codecs = Codecs {
            event: Some(DecodeFn::of::<CanonicalCodec>()),
            ..Codecs::default()
        };
        Engine::new(
            storage,
            Providers::new().with(Arc::new(FakeProvider::new()), codecs),
            control,
            Arc::new(MemoryVault::default()),
            Arc::new(ManualClock::new(Timestamp::now())),
            sink,
            Config {
                writer_idle: Duration::from_millis(1),
                ..Config::default()
            },
        )
    }

    fn discovered(name: String) -> DiscoveryWrite {
        DiscoveryWrite {
            account: AccountId::from_u128(1),
            collections: vec![collection(name)],
            complete: false,
        }
    }

    fn names(storage: &SqliteStorage, tenant: TenantId) -> std::collections::HashSet<String> {
        let store = storage.open(tenant).unwrap();
        let cols = store.collections().unwrap();
        cols.into_iter().map(|c| c.remote.remote.0).collect()
    }

    /// Writes cancelled at random moments while the writer keeps retiring:
    /// never two writes of a tenant at once, heads only go up, nothing
    /// confirmed is lost.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn idle_writer_retiring_under_cancelled_writes_stays_single() {
        let dir = temp_dir("writer-race");
        let storage = Arc::new(SqliteStorage::new(&dir.join("tenants"), 4).unwrap());
        storage.create_tenant(TENANT).unwrap();
        let (last, backwards) = (
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicBool::new(false)),
        );
        let sink: CommitSink = {
            let (last, backwards) = (last.clone(), backwards.clone());
            Arc::new(move |_, head: Seq| {
                if last.fetch_max(head.0, Ordering::SeqCst) >= head.0 {
                    backwards.store(true, Ordering::SeqCst);
                }
            })
        };
        let engine = engine(&dir, storage.clone(), sink);
        static INSIDE: AtomicUsize = AtomicUsize::new(0);
        static OVERLAP: AtomicBool = AtomicBool::new(false);
        let put = |name: String, slow: bool| {
            engine.write(TENANT, move |s| {
                if INSIDE.fetch_add(1, Ordering::SeqCst) > 0 {
                    OVERLAP.store(true, Ordering::SeqCst);
                }
                if slow {
                    // Still running when a second writer would start.
                    std::thread::sleep(Duration::from_millis(2));
                }
                let out = s.apply_discovery(&discovered(name));
                INSIDE.fetch_sub(1, Ordering::SeqCst);
                out
            })
        };
        let mut rng = 0x9e37_79b9_7f4a_7c15_u64;
        let mut confirmed = Vec::new();
        for i in 0..600 {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            // Around the moment the idle writer retires.
            std::thread::sleep(Duration::from_micros(500 + rng % 1000));
            // Sent, then dropped by its caller before it ran.
            let cancelled = tokio::time::timeout(Duration::ZERO, put(format!("x{i}"), true));
            if let Ok(res) = cancelled.await {
                res.unwrap();
                confirmed.push(format!("x{i}"));
            }
            // Started next to the cancelled one if its writer was retired.
            std::thread::sleep(Duration::from_micros(300));
            put(format!("c{i}"), false).await.unwrap();
            confirmed.push(format!("c{i}"));
        }
        assert!(!OVERLAP.load(Ordering::SeqCst), "two writers ran at once");
        assert!(
            !backwards.load(Ordering::SeqCst),
            "a commit head went backwards"
        );
        let have = names(&storage, TENANT);
        for name in &confirmed {
            assert!(have.contains(name), "{name} confirmed but lost");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 64 tenants on a blocking pool of two threads: every write runs on that
    /// pool (no thread per tenant), each tenant's writes run in the order they
    /// asked, each commit is notified once in order, and none is lost while
    /// idle writers keep being dropped.
    #[test]
    fn writes_of_many_tenants_keep_order_on_a_bounded_pool() {
        const POOL: usize = 2;
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .max_blocking_threads(POOL)
            .enable_all()
            .build()
            .unwrap();
        let dir = temp_dir("writer-pool");
        let storage = Arc::new(SqliteStorage::new(&dir.join("tenants"), 4).unwrap());
        let tenants: Vec<TenantId> = (1..=64).map(TenantId::from_u128).collect();
        for &t in &tenants {
            storage.create_tenant(t).unwrap();
        }
        type Log<V> = Arc<Mutex<HashMap<TenantId, Vec<V>>>>;
        let (notified, ran): (Log<u64>, Log<usize>) = Default::default();
        let sink: CommitSink = {
            let notified = notified.clone();
            Arc::new(move |t, head: Seq| {
                notified.lock().unwrap().entry(t).or_default().push(head.0)
            })
        };
        let threads = Arc::new(Mutex::new(std::collections::HashSet::new()));
        let engine = engine(&dir, storage.clone(), sink);
        let put = |t: TenantId, i: usize| {
            let (ran, threads) = (ran.clone(), threads.clone());
            Box::pin(engine.write(t, move |s| {
                threads.lock().unwrap().insert(std::thread::current().id());
                ran.lock().unwrap().entry(t).or_default().push(i);
                s.apply_discovery(&discovered(format!("c{i}")))
            }))
        };
        const ROUNDS: usize = 5;
        const PER_ROUND: usize = 20;
        rt.block_on(async {
            for round in 0..ROUNDS {
                // Polled once in this order, each write asks for its tenant's
                // writer in index order before any of them is awaited.
                let mut pending = Vec::new();
                for i in round * PER_ROUND..(round + 1) * PER_ROUND {
                    for &t in &tenants {
                        let mut w = put(t, i);
                        match futures_util::FutureExt::now_or_never(w.as_mut()) {
                            Some(r) => r.unwrap(),
                            None => pending.push(w),
                        }
                    }
                }
                for r in futures_util::future::join_all(pending).await {
                    r.unwrap();
                }
                // Idle writers are dropped between rounds.
                while engine.active_writers() > 0 {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            }
        });
        let used = threads.lock().unwrap().len();
        assert!(used <= POOL, "writes ran on {used} threads");
        let (ran, notified) = (ran.lock().unwrap(), notified.lock().unwrap());
        let want: Vec<usize> = (0..ROUNDS * PER_ROUND).collect();
        for t in &tenants {
            assert_eq!(ran[t], want, "{t}: writes ran out of order");
            let heads = &notified[t];
            assert!(heads.windows(2).all(|w| w[0] < w[1]), "{t}: {heads:?}");
            let head = storage.open(*t).unwrap().head().unwrap();
            assert_eq!(heads.last(), Some(&head.0), "{t}: last commit not notified");
            let have = names(&storage, *t);
            assert!(
                want.iter().all(|i| have.contains(&format!("c{i}"))),
                "{t}: a write was lost"
            );
        }
        drop(rt);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
