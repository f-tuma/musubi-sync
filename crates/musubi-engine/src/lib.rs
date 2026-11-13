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
use std::marker::PhantomData;
use std::num::NonZeroU32;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use http::Uri;
use jiff::Timestamp;
use musubi_model::Codec;
use musubi_model::control::{
    AccountRecord, AccountState, ChannelRecord, Job, JobKind, JobSpec, Lane,
};
use musubi_model::provider::{Clock, Limiter};
use musubi_model::{
    AccountId, CollectionId, ControlStore, Event, Provider, Seq, Storage, StoreError, SyncError,
    TenantId, TenantStore, TokenVault,
};
use tokio::sync::{Notify, Semaphore};

mod ingest;
mod jobs;
mod writer;

use writer::Writer;

/// Called by the tenant writer after every commit with the tenant's new head.
pub type CommitSink = Arc<dyn Fn(TenantId, Seq) + Send + Sync>;

/// How long push notifications for one collection are coalesced into one delta.
pub const COALESCE: Duration = Duration::from_millis(150);

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
    /// A tenant writer without a write for this long stops its thread.
    pub writer_idle: Duration,
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
        }
    }
}

/// The sync engine. Cheap to clone; clones share all state.
pub struct Engine<S: Storage, C> {
    inner: Arc<Inner<S, C>>,
}

impl<S: Storage, C> Clone for Engine<S, C> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

struct Inner<S: Storage, C> {
    storage: Arc<S>,
    provider: Arc<dyn Provider>,
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
    // Same shortcut, and lost on restart: one extra full read or watch request per restart.
    collections: Mutex<HashMap<CollectionId, CollectionRt>>,
    codec: PhantomData<fn() -> C>,
}

/// In-memory per-account state: request pacing, a rate-limit block and
/// whether the last attempt ended `Unauthorized` despite a fresh token.
#[derive(Clone)]
struct AccountRt {
    limiter: Arc<dyn Limiter>,
    blocked_until: Option<Timestamp>,
    unauthorized: bool,
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

impl<S: Storage + 'static, C: Codec<Event> + 'static> Engine<S, C> {
    /// Creates an engine. Nothing runs until [`Engine::start`] or
    /// [`Engine::run_until_idle`].
    pub fn new(
        storage: Arc<S>,
        provider: Arc<dyn Provider>,
        control: Arc<dyn ControlStore>,
        vault: Arc<dyn TokenVault>,
        clock: Arc<dyn Clock>,
        sink: CommitSink,
        config: Config,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                storage,
                provider,
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
                collections: Default::default(),
                codec: PhantomData,
            }),
        }
    }

    /// Starts discovery of an account left in `Connecting` by the OAuth
    /// callback. Discovery moves it to `Active` and schedules its collections.
    pub async fn connect(&self, account: AccountId) -> Result<(), EngineError> {
        let rec = self.account(account).await?.ok_or(StoreError::NotFound)?;
        let spec = JobSpec {
            tenant: rec.tenant,
            lane: Lane::Sync,
            kind: JobKind::Discover { account },
            priority: 10,
            due: self.now(),
            dedup: Some(format!("discover:{account}")),
        };
        self.enqueue(spec).await
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
            self.enqueue(JobSpec {
                tenant: rec.tenant,
                lane: Lane::Sync,
                kind: JobKind::Discover { account },
                priority: 10,
                due,
                dedup: Some(format!("discover:{account}")),
            })
            .await?;
        }
        self.removal(&rec).await
    }

    /// The steps of [`Engine::remove_account`] after the state change; each
    /// one can run again, also concurrently with a resumed removal, so what a
    /// step finds already gone counts as done.
    pub(crate) async fn removal(&self, rec: &AccountRecord) -> Result<(), EngineError> {
        let (tenant, account) = (rec.tenant, rec.id);
        // Under the collection lock: a running job finishes first (including a
        // channel it opens), and later ones find the collection gone.
        let cols = self.collections(tenant).await?;
        for col in cols.into_iter().filter(|c| c.account == account) {
            let _lock = self.lock(col.id).lock_owned().await;
            gone_ok(
                self.write(tenant, move |s| s.purge_collection(col.id).map(drop))
                    .await,
            )?;
        }
        let channels = self.ctl(move |c| c.channels(tenant, Some(account))).await?;
        if !channels.is_empty() {
            match self.cx(account, false).await {
                Ok(cx) => gone_ok(self.drop_channels(&cx, &channels).await)?,
                // Without a grant the channels cannot be stopped; they expire.
                // Any other failure retries the removal, which keeps the tokens.
                Err(EngineError::Sync(SyncError::ReauthRequired { .. })) => {
                    tracing::warn!(%account, "no grant to stop push channels with");
                    for ch in channels {
                        gone_ok(self.ctl(move |c| c.delete_channel(ch.id())).await)?;
                    }
                }
                Err(e) => return Err(e),
            }
        }
        gone_ok(self.ctl(move |c| c.delete_account(account)).await)?;
        self.inner
            .accounts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&account);
        Ok(())
    }

    /// Stops the writer of a deleted tenant, so its store handle (and the
    /// unlinked files behind it) is released. Call after removing its accounts.
    pub fn forget_tenant(&self, tenant: TenantId) {
        self.writers().remove(&tenant);
    }

    /// Tenants with a running writer thread; an idle one stops after
    /// [`Config::writer_idle`].
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
        let writer = self.writer(tenant).await?;
        let res = writer.run(f).await;
        if matches!(res, Err(EngineError::Stopped)) {
            // The thread died (a panic in the store); the next write starts a
            // new one, unless another caller already replaced it.
            let mut writers = self.writers();
            if writers
                .get(&tenant)
                .is_some_and(|w| Arc::ptr_eq(w, &writer))
            {
                writers.remove(&tenant);
            }
        }
        res
    }

    async fn writer(&self, tenant: TenantId) -> Result<Arc<Writer<S::Tenant>>, EngineError> {
        if let Some(w) = self.writers().get(&tenant) {
            return Ok(w.clone());
        }
        // Opened without holding the map: other tenants' writes go on meanwhile.
        let storage = self.inner.storage.clone();
        let store = blocking(move || Ok(storage.open(tenant)?)).await?;
        let mut writers = self.writers();
        if let Some(w) = writers.get(&tenant) {
            // Another caller won the race; this handle is dropped unused.
            return Ok(w.clone());
        }
        let inner = Arc::downgrade(&self.inner);
        let retire = move |id| {
            let Some(inner) = inner.upgrade() else { return };
            let mut writers = inner.writers.lock().unwrap_or_else(|e| e.into_inner());
            // Only the map holds it: no write is between getting it and its reply
            // (a queued command holds a handle too, even if its caller was
            // cancelled), and the next one must take this lock to get it.
            if writers
                .get(&tenant)
                .is_some_and(|w| w.id == id && Arc::strong_count(w) == 1)
            {
                writers.remove(&tenant);
            }
        };
        let idle = self.inner.config.writer_idle;
        let w = Arc::new(
            Writer::spawn(tenant, store, self.inner.sink.clone(), idle, retire)
                .map_err(StoreError::Io)?,
        );
        writers.insert(tenant, w.clone());
        Ok(w)
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

    /// Writes cancelled at random moments while the writer keeps retiring:
    /// never two writer threads at once, heads only go up, nothing confirmed
    /// is lost.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn idle_writer_retiring_under_cancelled_writes_stays_single() {
        let dir = std::env::temp_dir().join(format!(
            "musubi-writer-race-{}-{}",
            std::process::id(),
            Timestamp::now().as_nanosecond()
        ));
        let storage = Arc::new(SqliteStorage::new(&dir.join("tenants"), 4).unwrap());
        storage.create_tenant(TENANT).unwrap();
        let control = Arc::new(SqliteControl::open(&dir.join("control.db")).unwrap());
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
        let engine: Engine<SqliteStorage, CanonicalCodec> = Engine::new(
            storage.clone(),
            Arc::new(FakeProvider::new()),
            control,
            Arc::new(MemoryVault::default()),
            Arc::new(ManualClock::new(Timestamp::now())),
            sink,
            Config {
                writer_idle: Duration::from_millis(1),
                ..Config::default()
            },
        );
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
                let out = s.apply_discovery(&DiscoveryWrite {
                    account: AccountId::from_u128(1),
                    collections: vec![collection(name)],
                    complete: false,
                });
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
        let have: std::collections::HashSet<String> = storage
            .open(TENANT)
            .unwrap()
            .collections()
            .unwrap()
            .into_iter()
            .map(|c| c.remote.remote.0)
            .collect();
        for name in &confirmed {
            assert!(have.contains(name), "{name} confirmed but lost");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
