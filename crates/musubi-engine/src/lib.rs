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
use tokio::sync::Semaphore;

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
    writers: tokio::sync::Mutex<HashMap<TenantId, Arc<Writer<S::Tenant>>>>,
    // shortcut: in-process only; two engine processes on one control store could
    // run two jobs of one collection at once. Upgrade to a lease per collection then.
    locks: Mutex<HashMap<CollectionId, Arc<tokio::sync::Mutex<()>>>>,
    accounts: Mutex<HashMap<AccountId, AccountRt>>,
    codec: PhantomData<fn() -> C>,
}

/// In-memory per-account state: request pacing and a rate-limit block.
#[derive(Clone)]
struct AccountRt {
    limiter: Arc<dyn Limiter>,
    blocked_until: Option<Timestamp>,
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
                locks: Default::default(),
                accounts: Default::default(),
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

    /// Removes an account: stops its push channels at the provider (while its
    /// tokens still exist), purges its collections and deletes it with its jobs.
    pub async fn remove_account(&self, account: AccountId) -> Result<(), EngineError> {
        let Some(rec) = self.account(account).await? else {
            return Ok(());
        };
        self.ctl(move |c| c.set_account_state(account, AccountState::Removing))
            .await?;
        let tenant = rec.tenant;
        let channels = self.ctl(move |c| c.channels(tenant, Some(account))).await?;
        if !channels.is_empty() {
            match self.cx(account, false).await {
                Ok(cx) => {
                    for ch in &channels {
                        self.drop_channel(&cx, ch).await?;
                    }
                }
                Err(e) => {
                    tracing::warn!(%account, error = %e, "cannot stop push channels");
                    for ch in channels {
                        self.ctl(move |c| c.delete_channel(&ch.sub.channel)).await?;
                    }
                }
            }
        }
        let cols = self.collections(tenant).await?;
        for col in cols.into_iter().filter(|c| c.account == account) {
            self.write(tenant, move |s| s.purge_collection(col.id))
                .await?;
        }
        self.ctl(move |c| c.delete_account(account)).await?;
        self.inner
            .accounts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&account);
        Ok(())
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
            tokio::time::sleep(self.inner.config.tick).await;
        }
    }

    /// Runs one leased job while extending its lease, then completes or
    /// reschedules it. A lost lease abandons the job: another runner owns it.
    async fn execute(&self, job: Job) {
        let (id, lease) = (job.id, job.lease);
        let run = std::pin::pin!(self.run_job(&job));
        let beat = std::pin::pin!(self.heartbeat(&job));
        let outcome = match futures_util::future::select(run, beat).await {
            futures_util::future::Either::Left((o, _)) => o,
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
        self.ctl(move |c| c.enqueue(&spec).map(drop)).await
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
            // The thread died (a panic in the store); the next write starts a new one.
            self.inner.writers.lock().await.remove(&tenant);
        }
        res
    }

    async fn writer(&self, tenant: TenantId) -> Result<Arc<Writer<S::Tenant>>, EngineError> {
        let mut writers = self.inner.writers.lock().await;
        if let Some(w) = writers.get(&tenant) {
            return Ok(w.clone());
        }
        let storage = self.inner.storage.clone();
        let store = blocking(move || Ok(storage.open(tenant)?)).await?;
        let w = Arc::new(
            Writer::spawn(tenant, store, self.inner.sink.clone()).map_err(StoreError::Io)?,
        );
        writers.insert(tenant, w.clone());
        Ok(w)
    }
}

/// Runs blocking work on the blocking pool; a panic there resumes here.
async fn blocking<R: Send + 'static>(
    f: impl FnOnce() -> Result<R, EngineError> + Send + 'static,
) -> Result<R, EngineError> {
    match tokio::task::spawn_blocking(f).await {
        Ok(r) => r,
        Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
        Err(_) => Err(EngineError::Stopped),
    }
}

fn after(t: Timestamp, d: Duration) -> Timestamp {
    t.saturating_add(d).unwrap_or(Timestamp::MAX)
}

fn sync_dedup(collection: CollectionId) -> String {
    format!("sync:{collection}")
}
