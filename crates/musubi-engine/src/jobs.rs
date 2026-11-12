//! Job bodies: account gating and tokens, discovery, the collection read
//! state machine, push channels and the occurrence index horizon.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::StreamExt;
use jiff::Timestamp;
use musubi_model::control::{
    AccountRecord, AccountState, AccountTokens, ChannelRecord, ChannelState, Job, JobKind, JobSpec,
    Lane,
};
use musubi_model::provider::{
    AccountCx, ChannelId, FetchHint, Limiter, Pass, PushSource, Start, Subscription,
};
use musubi_model::remote::RemoteRef;
use musubi_model::store::{
    CollectionError, CollectionState, CollectionStatus, Coverage, DiscoveryWrite, Include,
    OccurrenceWrite, PageWrite, SeriesExtension,
};
use musubi_model::{
    AccountId, ChangePage, Codec, CollectionId, Cursor, EntityKind, Event, ReadId, RemoteKey, Seq,
    Storage, SyncError, TenantId, TenantStore, TimeWindow,
};

use crate::{AccountRt, CollectionRt, Engine, EngineError, after, blocking, ingest, sync_dedup};

/// What becomes of a job after it ran.
pub(crate) enum Outcome {
    Done,
    Retry { due: Timestamp, count: bool },
}

/// Account-bound work of a job.
enum Work {
    Discover,
    Sync(CollectionId),
    Renew(ChannelRecord),
}

/// Series extended per index pass.
const EXTEND_BATCH: u32 = 200;
/// Access tokens this close to expiry are refreshed before use.
const REFRESH_EARLY: Duration = Duration::from_secs(60);
/// Wait after `RateLimited` without `Retry-After`.
const RATE_LIMIT_WAIT: Duration = Duration::from_secs(30);
/// Wait for a job whose collection another job is working on.
const BUSY_WAIT: Duration = Duration::from_secs(1);
/// Longest backoff of a failing job or of repeated full reads.
const BACKOFF_CAP: Duration = Duration::from_secs(3600);
/// A collection whose push channel the provider refused is polled only and
/// asked for a channel again after this long (Google channels live ~7 days).
const PUSH_RETRY: Duration = Duration::from_secs(86_400);
/// How long a collection stays full-read-only before a delta is tried again.
const DELTA_RETRY: Duration = Duration::from_secs(86_400);

/// `base` doubled `n` times, at most [`BACKOFF_CAP`].
fn backoff(base: Duration, n: u32) -> Duration {
    base.saturating_mul(1 << n.min(12)).min(BACKOFF_CAP)
}

impl<S: Storage + 'static, C: Codec<Event> + 'static> Engine<S, C> {
    pub(crate) async fn run_job(&self, job: &Job) -> Outcome {
        let (account, work) = match &job.spec.kind {
            JobKind::Discover { account } => (*account, Work::Discover),
            JobKind::Sync {
                account,
                collection,
            } => (*account, Work::Sync(*collection)),
            JobKind::RenewChannel { channel } => {
                let id = channel.clone();
                match self.ctl(move |c| c.find_channel(&id)).await {
                    Ok(Some(ch)) if ch.open().is_some() => (ch.account, Work::Renew(ch)),
                    // Gone, or pending: a pending channel is never renewed.
                    Ok(_) => return Outcome::Done,
                    Err(e) => return self.retry(job, e),
                }
            }
            JobKind::ExtendIndex => {
                return match self.extend_index(job.spec.tenant).await {
                    Ok(()) => Outcome::Done,
                    Err(e) => self.retry(job, e),
                };
            }
            // shortcut: a no-op; pruning and WAL checkpoints come with the
            // maintenance checkpoint, before tenants accumulate tombstones.
            JobKind::Maintain => return Outcome::Done,
        };
        self.account_job(job, account, work).await
    }

    /// The account state machine gates every account job: only `Active`
    /// accounts sync, a `Connecting` one only discovers, a `Removing` one's
    /// discovery job continues the removal, and other jobs of `ReauthRequired`,
    /// `Paused` and `Removing` accounts are dropped (no polling).
    ///
    /// One job per collection, and one discovery per account, runs at a time;
    /// a job finding its lock busy comes back shortly instead of holding a
    /// lane slot.
    async fn account_job(&self, job: &Job, account: AccountId, work: Work) -> Outcome {
        let rec = match self.account(account).await {
            Ok(Some(r)) => r,
            Ok(None) => return Outcome::Done,
            Err(e) => return self.retry(job, e),
        };
        let runnable = match rec.state {
            AccountState::Active => true,
            AccountState::Connecting => matches!(work, Work::Discover),
            // A removal cut short by a crash (plan 3.3).
            AccountState::Removing if matches!(work, Work::Discover) => {
                return match self.removal(&rec).await {
                    Ok(()) => Outcome::Done,
                    Err(e) => self.retry(job, e),
                };
            }
            _ => false,
        };
        if !runnable {
            return Outcome::Done;
        }
        if let Some(until) = self.rt(account).blocked_until
            && until > self.now()
        {
            return Outcome::Retry {
                due: until,
                count: false,
            };
        }
        let lock = match &work {
            Work::Discover => self.account_lock(account),
            Work::Sync(c) => self.lock(*c),
            Work::Renew(ch) => self.lock(ch.collection),
        };
        let Ok(_lock) = lock.try_lock_owned() else {
            return Outcome::Retry {
                due: after(self.now(), BUSY_WAIT),
                count: false,
            };
        };
        let res = match self.cx(account, false).await {
            Ok(cx) => match self.work(&rec, &cx, job, &work).await {
                // The access token was revoked early: refresh once and run again.
                Err(EngineError::Sync(SyncError::Unauthorized)) => {
                    match self.cx(account, true).await {
                        Ok(cx) => self.work(&rec, &cx, job, &work).await,
                        Err(e) => Err(e),
                    }
                }
                r => r,
            },
            Err(e) => Err(e),
        };
        match res {
            Ok(()) => {
                self.with_rt(account, |rt| rt.unauthorized = false);
                Outcome::Done
            }
            Err(e) => self.failed(job, &rec, &work, e).await,
        }
    }

    async fn work(
        &self,
        rec: &AccountRecord,
        cx: &AccountCx,
        job: &Job,
        work: &Work,
    ) -> Result<(), EngineError> {
        match work {
            Work::Discover => self.discover(rec, cx).await,
            Work::Sync(col) => self.sync(rec, cx, *col, job.spec.lane).await,
            Work::Renew(ch) => self.renew(cx, ch).await,
        }
    }

    async fn failed(&self, job: &Job, rec: &AccountRecord, work: &Work, e: EngineError) -> Outcome {
        let col = match work {
            Work::Sync(c) => Some(*c),
            _ => None,
        };
        match e {
            // `Unauthorized` right after a refresh may be a provider hiccup:
            // one counted retry before the user is asked to sign in again.
            EngineError::Sync(SyncError::Unauthorized)
                if !self.with_rt(rec.id, |rt| std::mem::replace(&mut rt.unauthorized, true)) =>
            {
                self.record_error(rec.tenant, col, e.to_string(), false)
                    .await;
                self.retry(job, e)
            }
            EngineError::Sync(SyncError::ReauthRequired | SyncError::Unauthorized) => {
                self.with_rt(rec.id, |rt| rt.unauthorized = false);
                tracing::warn!(account = %rec.id, "account requires sign-in");
                let id = rec.id;
                match self
                    .ctl(move |c| c.set_account_state(id, AccountState::ReauthRequired))
                    .await
                {
                    Ok(()) => Outcome::Done,
                    Err(e) => self.retry(job, e),
                }
            }
            EngineError::Sync(SyncError::RateLimited { retry_after }) => {
                let until = after(self.now(), retry_after.unwrap_or(RATE_LIMIT_WAIT));
                self.with_rt(rec.id, |rt| rt.blocked_until = Some(until));
                Outcome::Retry {
                    due: until,
                    count: false,
                }
            }
            EngineError::Sync(SyncError::Permanent(err)) => {
                tracing::warn!(job = job.id.0, collection = ?col, error = %err, "permanent provider error");
                self.record_error(rec.tenant, col, err.to_string(), true)
                    .await;
                Outcome::Done
            }
            e => {
                self.record_error(rec.tenant, col, e.to_string(), false)
                    .await;
                self.retry(job, e)
            }
        }
    }

    /// A job that panicked: recorded on its collection and retried with
    /// backoff like any failure.
    pub(crate) async fn panicked(&self, job: &Job, message: String) -> Outcome {
        let col = match &job.spec.kind {
            JobKind::Sync { collection, .. } => Some(*collection),
            _ => None,
        };
        self.record_error(job.spec.tenant, col, message.clone(), false)
            .await;
        self.retry(job, message)
    }

    fn retry(&self, job: &Job, e: impl std::fmt::Display) -> Outcome {
        let wait = backoff(Duration::from_secs(10), job.attempts);
        tracing::warn!(job = job.id.0, error = %e, ?wait, "job failed; retrying");
        Outcome::Retry {
            due: after(self.now(), wait),
            count: true,
        }
    }

    async fn record_error(
        &self,
        tenant: TenantId,
        col: Option<CollectionId>,
        message: String,
        permanent: bool,
    ) {
        let Some(col) = col else { return };
        let at = self.now();
        let res = self
            .set_status(tenant, col, move |s| {
                s.error = Some(CollectionError {
                    message,
                    at,
                    permanent,
                });
            })
            .await;
        if let Err(e) = res {
            tracing::warn!(collection = %col, error = %e, "recording sync error failed");
        }
    }

    /// The account's provider context, refreshing the access token when it is
    /// about to expire or `force` is set.
    pub(crate) async fn cx(
        &self,
        account: AccountId,
        force: bool,
    ) -> Result<AccountCx, EngineError> {
        let vault = self.inner.vault.clone();
        let tokens = blocking(move || Ok(vault.load(account)?))
            .await?
            .ok_or(SyncError::ReauthRequired)?;
        let now = self.now();
        let tokens = if force || tokens.access_until <= after(now, REFRESH_EARLY) {
            let t = self.inner.provider.refresh(&tokens.refresh).await?;
            let fresh = AccountTokens {
                refresh: t.rotated_refresh.unwrap_or(tokens.refresh),
                access: t.access,
                access_until: after(now, t.expires_in),
            };
            let (vault, stored) = (self.inner.vault.clone(), fresh.clone());
            blocking(move || Ok(vault.store(account, &stored)?)).await?;
            fresh
        } else {
            tokens
        };
        Ok(AccountCx {
            account,
            token: tokens.access,
            limiter: self.rt(account).limiter,
            clock: self.inner.clock.clone(),
        })
    }

    fn rt(&self, account: AccountId) -> AccountRt {
        self.with_rt(account, |rt| rt.clone())
    }

    fn with_col<R>(&self, id: CollectionId, f: impl FnOnce(&mut CollectionRt) -> R) -> R {
        let mut map = self
            .inner
            .collections
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        f(map.entry(id).or_default())
    }

    fn with_rt<R>(&self, account: AccountId, f: impl FnOnce(&mut AccountRt) -> R) -> R {
        let mut map = self
            .inner
            .accounts
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let rt = map.entry(account).or_insert_with(|| AccountRt {
            limiter: Arc::new(Pacer::new(
                self.inner.provider.descriptor().limits.requests_per_minute,
            )),
            blocked_until: None,
            unauthorized: false,
        });
        f(rt)
    }

    async fn discover(&self, rec: &AccountRecord, cx: &AccountCx) -> Result<(), EngineError> {
        let (tenant, account) = (rec.tenant, rec.id);
        let (collections, complete) = match self.inner.provider.discover(cx).await {
            Ok(d) => (d.collections, true),
            // A partial listing only adds and updates: a missing calendar is not gone.
            Err(SyncError::PartialDiscovery { found, source }) => {
                tracing::warn!(%account, error = %source, "partial discovery");
                (found, false)
            }
            Err(e) => return Err(e.into()),
        };
        // Under the account lock the state cannot become `Removing` meanwhile:
        // `remove_account` waits for it.
        let state = self.account(account).await?.map(|r| r.state);
        if !matches!(state, Some(AccountState::Active | AccountState::Connecting)) {
            return Ok(());
        }
        let d = DiscoveryWrite {
            account,
            collections,
            complete,
        };
        self.write(tenant, move |s| s.apply_discovery(&d)).await?;
        let cols: Vec<_> = self
            .collections(tenant)
            .await?
            .into_iter()
            .filter(|c| c.account == account)
            .collect();
        for col in cols.iter().filter(|c| c.gone) {
            // A job still on it: the next discovery drops it.
            let Ok(_lock) = self.lock(col.id).try_lock_owned() else {
                continue;
            };
            self.drop_collection(tenant, cx, col.id).await?;
        }
        if state == Some(AccountState::Connecting) {
            self.ctl(move |c| c.set_account_state(account, AccountState::Active))
                .await?;
        }
        let now = self.now();
        for col in cols
            .iter()
            .filter(|c| !c.gone && c.remote.kind == EntityKind::Event)
        {
            self.enqueue(sync_job(tenant, account, col.id, Lane::Sync, now))
                .await?;
        }
        let c = &self.inner.config;
        self.enqueue(JobSpec {
            tenant,
            lane: Lane::Sync,
            kind: JobKind::Discover { account },
            priority: 10,
            due: after(now, c.rediscover_every),
            dedup: Some(format!("discover:{account}")),
        })
        .await?;
        self.enqueue(JobSpec {
            tenant,
            lane: Lane::Background,
            kind: JobKind::ExtendIndex,
            priority: -10,
            due: after(now, c.extend_every),
            dedup: Some("extend".into()),
        })
        .await
    }

    async fn drop_collection(
        &self,
        tenant: TenantId,
        cx: &AccountCx,
        id: CollectionId,
    ) -> Result<(), EngineError> {
        let channels = self.collection_channels(tenant, cx.account, id).await?;
        self.drop_channels(cx, &channels).await?;
        self.write(tenant, move |s| s.purge_collection(id)).await?;
        Ok(())
    }

    /// Push channel rows of one collection, pending and open.
    async fn collection_channels(
        &self,
        tenant: TenantId,
        account: AccountId,
        id: CollectionId,
    ) -> Result<Vec<ChannelRecord>, EngineError> {
        let mut channels = self.ctl(move |c| c.channels(tenant, Some(account))).await?;
        channels.retain(|c| c.collection == id);
        Ok(channels)
    }

    /// Stops push channels at the provider (best effort) and forgets them.
    /// A pending one is stopped by its id with the resource of an open channel
    /// of its collection in `chs` (Google accepts any watch's resource id of
    /// the calendar); without one it is only forgotten and expires there.
    pub(crate) async fn drop_channels(
        &self,
        cx: &AccountCx,
        chs: &[ChannelRecord],
    ) -> Result<(), EngineError> {
        for ch in chs {
            let sub = match &ch.state {
                ChannelState::Open(s) => Some(s.clone()),
                ChannelState::Pending(id) => chs
                    .iter()
                    .filter(|o| o.collection == ch.collection)
                    .find_map(ChannelRecord::open)
                    .map(|known| pending_sub(id, known)),
            };
            if let (Some(push), Some(sub)) = (self.inner.provider.push(), sub)
                && let Err(e) = push.unsubscribe(cx, &sub).await
            {
                tracing::warn!(channel = %sub.channel.0, error = %e, "unsubscribe failed");
            }
            let id = ch.id().clone();
            self.ctl(move |c| c.delete_channel(&id)).await?;
        }
        Ok(())
    }

    /// Stops the pending channels among `chs` (left by a lost subscribe reply
    /// or a crash) with `known`'s resource and forgets them. One that cannot
    /// be stopped now stays for the next try.
    async fn stop_pending(
        &self,
        cx: &AccountCx,
        push: &dyn PushSource,
        chs: &[ChannelRecord],
        known: &Subscription,
    ) -> Result<(), EngineError> {
        for ch in chs {
            let ChannelState::Pending(id) = &ch.state else {
                continue;
            };
            if *id == known.channel {
                continue;
            }
            match push.unsubscribe(cx, &pending_sub(id, known)).await {
                Ok(()) => {
                    let id = id.clone();
                    self.ctl(move |c| c.delete_channel(&id)).await?;
                }
                Err(e) => {
                    tracing::warn!(channel = %id.0, error = %e, "pending channel not stopped");
                }
            }
        }
        Ok(())
    }

    /// Opens a channel under a fresh id that is stored as pending first, so a
    /// lost reply or a crash leaves a row to stop the channel by. On success
    /// the collection's earlier pending channels (`chs`) are stopped; on a
    /// refusal the new row is removed. The caller stores the open channel.
    async fn open_channel(
        &self,
        cx: &AccountCx,
        push: &dyn PushSource,
        callback: &http::Uri,
        tenant: TenantId,
        col: &CollectionState,
        chs: &[ChannelRecord],
    ) -> Result<Result<Subscription, SyncError>, EngineError> {
        let id = new_channel_id(col.id);
        let pending = ChannelRecord {
            tenant,
            account: col.account,
            collection: col.id,
            state: ChannelState::Pending(id.clone()),
        };
        self.ctl(move |c| c.put_channel(&pending)).await?;
        let res = push.subscribe(cx, &col.remote.remote, callback, &id).await;
        match &res {
            Ok(sub) => self.stop_pending(cx, push, chs, sub).await?,
            Err(SyncError::Permanent(_)) => self.ctl(move |c| c.delete_channel(&id)).await?,
            // The channel may be open: the row stays until it is stopped.
            Err(_) => {}
        }
        Ok(res)
    }

    /// The read state machine of one collection (plan ch. 5, proposal 3.1).
    ///
    /// A completed full read is finished first (guard, deletions). The sync
    /// lane runs the window read of a new collection and deltas; full reads
    /// run in the background lane, except the one replacing an expired cursor.
    async fn sync(
        &self,
        rec: &AccountRecord,
        cx: &AccountCx,
        id: CollectionId,
        lane: Lane,
    ) -> Result<(), EngineError> {
        let tenant = rec.tenant;
        let Some(col) = self.collection(tenant, id).await? else {
            return Ok(());
        };
        if col.gone || col.remote.kind != EntityKind::Event {
            return Ok(());
        }
        let background = lane == Lane::Background;
        if let Some(r) = col.active_read {
            if col.read_complete {
                return self.finish(rec, cx, &col, r).await;
            }
            if !background {
                return self.ensure_full(tenant, rec.id, id).await;
            }
            let r = match self
                .read_pass(tenant, cx, &col, col.cursor.clone(), Start::Full, Some(r))
                .await
            {
                Err(EngineError::Sync(SyncError::CursorExpired)) => {
                    self.full_read(tenant, cx, &col).await?
                }
                res => res.map(|()| r)?,
            };
            return self.finish(rec, cx, &col, r).await;
        }
        let wants_full = col.cursor.is_none() || col.status.coverage != Coverage::Full;
        if background && wants_full {
            let r = self.full_read(tenant, cx, &col).await?;
            return self.finish(rec, cx, &col, r).await;
        }
        // A background job with no full read left is one whose finish
        // committed before a crash took its `synced`: the delta below
        // schedules what that would have.
        if wants_full {
            if col.status.coverage == Coverage::None {
                self.window_read(tenant, cx, &col).await?;
            }
            return self.ensure_full(tenant, rec.id, id).await;
        }
        // A full-read-only collection skips the delta its provider would
        // refuse, until `DELTA_RETRY` has passed and a delta is tried again.
        let now = self.now();
        let full_only = |rt: &CollectionRt| rt.full_only_until.is_some_and(|t| t > now);
        let res = match self.with_col(id, |rt| full_only(rt)) {
            true => Err(EngineError::Sync(SyncError::CursorExpired)),
            false => {
                self.read_pass(tenant, cx, &col, col.cursor.clone(), Start::Full, None)
                    .await
            }
        };
        match res {
            Err(EngineError::Sync(SyncError::CursorExpired)) => {
                // Expired right after the full read that issued it, twice in a
                // row (live: a Google holiday calendar answers every fresh token
                // with 410), or a cursor that says no delta is possible: the
                // provider gives no usable delta here, so every sync is a full
                // read. One expiry alone is an ordinary expired token.
                let (was, is) = self.with_col(id, |rt| {
                    let was = full_only(rt);
                    if !was && rt.full_reads > 1 {
                        rt.full_only_until = Some(after(now, DELTA_RETRY));
                    }
                    (was, full_only(rt))
                });
                match (was, is) {
                    (false, true) => {
                        tracing::info!(collection = %id, retry_in = ?DELTA_RETRY, "no usable delta cursor after a full read; full reads only, at the poll interval")
                    }
                    (false, false) => {
                        tracing::info!(collection = %id, "delta cursor expired; full read")
                    }
                    (true, _) => {
                        tracing::debug!(collection = %id, "full-read-only collection; full read")
                    }
                }
                let r = self.full_read(tenant, cx, &col).await?;
                self.finish(rec, cx, &col, r).await
            }
            res => {
                res?;
                self.synced(rec, cx, &col, false).await
            }
        }
    }

    /// The window around today, read before the full read so the user sees
    /// current data early. It deletes nothing.
    async fn window_read(
        &self,
        tenant: TenantId,
        cx: &AccountCx,
        col: &CollectionState,
    ) -> Result<(), EngineError> {
        let now = self.now();
        let c = &self.inner.config;
        let start = now.saturating_sub(c.window_back).unwrap_or(Timestamp::MIN);
        let Some(w) = TimeWindow::new(start, after(now, c.window_forward)) else {
            return Ok(());
        };
        self.read_pass(tenant, cx, col, None, Start::Window(w), None)
            .await?;
        self.set_status(tenant, col.id, move |s| s.coverage = Coverage::Window(w))
            .await
    }

    /// A full read from scratch under a new read id.
    async fn full_read(
        &self,
        tenant: TenantId,
        cx: &AccountCx,
        col: &CollectionState,
    ) -> Result<ReadId, EngineError> {
        let last = col.active_read.max(col.last_read).map_or(0, |r| r.0);
        let r = ReadId(last + 1);
        self.read_pass(tenant, cx, col, None, Start::Full, Some(r))
            .await?;
        Ok(r)
    }

    /// Reads pages until the provider's last one; one transaction per page.
    ///
    /// shortcut: no pipelining, the next page is requested only after the
    /// previous one committed; overlap fetch and commit once full reads of
    /// large calendars are measured too slow.
    async fn read_pass(
        &self,
        tenant: TenantId,
        cx: &AccountCx,
        col: &CollectionState,
        cursor: Option<Cursor>,
        start: Start,
        read: Option<ReadId>,
    ) -> Result<(), EngineError> {
        let hint = FetchHint {
            start,
            page_size: self.inner.config.page_size,
        };
        let mut pages = self
            .inner
            .provider
            .changes(cx, &col.remote.remote, cursor, hint);
        while let Some(page) = pages.next().await {
            let page = page?;
            let last = page.last;
            self.ingest(tenant, col.id, page, read).await?;
            if last {
                break;
            }
        }
        Ok(())
    }

    /// Decodes and expands a page off the runtime, then commits it.
    async fn ingest(
        &self,
        tenant: TenantId,
        id: CollectionId,
        page: ChangePage,
        read: Option<ReadId>,
    ) -> Result<Seq, EngineError> {
        let ChangePage {
            pass,
            upserts,
            removals,
            cursor,
            last,
        } = page;
        let c = &self.inner.config;
        let now = self.now();
        let index = ingest::Index {
            horizon: after(now, c.index_forward),
            fallback: after(now, c.window_forward),
            limit: c.index_row_limit,
        };
        let storage = self.inner.storage.clone();
        let (upserts, unchanged) = blocking(move || {
            let keys: Vec<RemoteKey> = upserts.iter().map(|o| o.key.clone()).collect();
            let known = storage.open(tenant)?.items_by_remote(id, &keys)?;
            Ok(ingest::prepare::<C>(upserts, known, index))
        })
        .await?;
        let w = PageWrite {
            collection: id,
            pass,
            read,
            last,
            cursor,
            upserts,
            unchanged,
            removals,
        };
        self.write(tenant, move |s| s.apply_page(&w)).await
    }

    /// Ends a completed full read: deletes what it did not see. Above the
    /// guard threshold every missing item is first fetched again; those that
    /// still exist are marked seen and survive.
    async fn finish(
        &self,
        rec: &AccountRecord,
        cx: &AccountCx,
        col: &CollectionState,
        r: ReadId,
    ) -> Result<(), EngineError> {
        let (tenant, id) = (rec.tenant, col.id);
        let missing = self.read(tenant, move |s| s.unseen_count(id, r)).await?;
        let batch = self.inner.config.guard_batch.max(1);
        if missing > self.inner.config.guard_threshold {
            tracing::info!(collection = %id, missing, "verifying missing items before deletion");
            let mut from: Option<RemoteKey> = None;
            loop {
                let keys = self
                    .read(tenant, move |s| s.unseen(id, r, from.as_ref(), batch))
                    .await?;
                let Some(last) = keys.last().cloned() else {
                    break;
                };
                let more = keys.len() >= batch as usize;
                let refs: Vec<RemoteRef> = keys
                    .into_iter()
                    .map(|key| RemoteRef {
                        collection: col.remote.remote.clone(),
                        key,
                    })
                    .collect();
                let found = self.inner.provider.fetch(cx, &refs).await?;
                if !found.is_empty() {
                    let page = ChangePage {
                        pass: Pass::Full,
                        upserts: found,
                        removals: Vec::new(),
                        cursor: None,
                        last: false,
                    };
                    self.ingest(tenant, id, page, Some(r)).await?;
                }
                if !more {
                    break;
                }
                from = Some(last);
            }
        }
        self.write(tenant, move |s| s.finish_full_read(id, r))
            .await?;
        self.synced(rec, cx, col, true).await
    }

    /// Records a successful sync, makes sure a push channel exists and
    /// schedules the safety poll.
    async fn synced(
        &self,
        rec: &AccountRecord,
        cx: &AccountCx,
        col: &CollectionState,
        full: bool,
    ) -> Result<(), EngineError> {
        let now = self.now();
        let c = &self.inner.config;
        let (full_reads, full_only) = self.with_col(col.id, |rt| {
            rt.full_reads = if full { rt.full_reads + 1 } else { 0 };
            if !full {
                // A delta worked: the cursor is usable again.
                rt.full_only_until = None;
            }
            (rt.full_reads, rt.full_only_until.is_some_and(|t| t > now))
        });
        // A third full read without a delta between, on a collection not
        // known to be full-read-only: a guard against a cursor that keeps
        // expiring on some other path, so the collection cannot spin.
        let backed_off =
            (!full_only && full_reads > 2).then(|| backoff(c.poll_every, full_reads - 3));
        let error = backed_off.map(|wait| {
            tracing::warn!(collection = %col.id, full_reads, ?wait, "no usable delta cursor after a full read; backing off");
            CollectionError {
                message: format!(
                    "no usable delta cursor after {full_reads} full reads in a row; next read in {} s",
                    wait.as_secs()
                ),
                at: now,
                permanent: false,
            }
        });
        self.set_status(rec.tenant, col.id, move |s| {
            if full {
                s.coverage = Coverage::Full;
                s.full_read_at = Some(now);
            }
            s.synced_at = Some(now);
            s.error = error;
        })
        .await?;
        let push = self.ensure_channel(rec, cx, col).await?;
        let every = match push {
            _ if let Some(wait) = backed_off => wait,
            // No catch-up delta: each sync of such a collection is a full read.
            Push::Existing if full_only => c.poll_every_push,
            _ if full_only => c.poll_every,
            // A full read ends at its own snapshot, and a sync job arriving
            // during it only deferred to it; a new channel missed what changed
            // before it existed. One delta now picks both up.
            _ if full => crate::COALESCE,
            Push::Opened => crate::COALESCE,
            Push::Existing => c.poll_every_push,
            Push::None => c.poll_every,
        };
        let mut job = sync_job(rec.tenant, rec.id, col.id, Lane::Sync, now);
        job.due = after(now, every);
        self.enqueue(job).await
    }

    async fn ensure_full(
        &self,
        tenant: TenantId,
        account: AccountId,
        id: CollectionId,
    ) -> Result<(), EngineError> {
        let mut job = sync_job(tenant, account, id, Lane::Background, self.now());
        job.dedup = Some(format!("full:{id}"));
        self.enqueue(job).await
    }

    /// Opens a push channel for the collection unless one is open; failing
    /// to open one only means polling. Stops pending channels left behind.
    async fn ensure_channel(
        &self,
        rec: &AccountRecord,
        cx: &AccountCx,
        col: &CollectionState,
    ) -> Result<Push, EngineError> {
        let (Some(push), Some(callback)) =
            (self.inner.provider.push(), &self.inner.config.callback)
        else {
            return Ok(Push::None);
        };
        let (tenant, account, id) = (rec.tenant, rec.id, col.id);
        let channels = self.collection_channels(tenant, account, id).await?;
        if let Some(open) = channels.iter().find_map(ChannelRecord::open) {
            self.stop_pending(cx, push, &channels, open).await?;
            return Ok(Push::Existing);
        }
        let now = self.now();
        if self.with_col(id, |rt| rt.push_refused_until.is_some_and(|t| t > now)) {
            return Ok(Push::None);
        }
        match self
            .open_channel(cx, push, callback, tenant, col, &channels)
            .await?
        {
            Ok(sub) => Ok(match self.put_channel(cx, tenant, id, sub).await? {
                true => Push::Opened,
                false => Push::None,
            }),
            Err(SyncError::Permanent(e)) => {
                self.refuse_push(id, &e);
                Ok(Push::None)
            }
            Err(e) => {
                tracing::warn!(collection = %id, error = %e, "push channel not opened; polling");
                Ok(Push::None)
            }
        }
    }

    /// The provider refused a push channel: poll only, ask again after [`PUSH_RETRY`].
    fn refuse_push(&self, id: CollectionId, e: &musubi_model::error::BoxError) {
        tracing::warn!(collection = %id, error = %e, retry_in = ?PUSH_RETRY, "push channel refused; polling only");
        let until = after(self.now(), PUSH_RETRY);
        self.with_col(id, |rt| rt.push_refused_until = Some(until));
    }

    /// Stores a new open channel (replacing its pending row) and schedules its
    /// renewal at two thirds of its life. Returns false, after stopping it, if
    /// the account is no longer syncing (a removal that started while it was
    /// being opened).
    async fn put_channel(
        &self,
        cx: &AccountCx,
        tenant: TenantId,
        collection: CollectionId,
        sub: Subscription,
    ) -> Result<bool, EngineError> {
        let account = cx.account;
        let state = self.account(account).await?.map(|r| r.state);
        let ch = ChannelRecord {
            tenant,
            account,
            collection,
            state: ChannelState::Open(sub.clone()),
        };
        if !matches!(state, Some(AccountState::Active | AccountState::Connecting)) {
            self.drop_channels(cx, std::slice::from_ref(&ch)).await?;
            return Ok(false);
        }
        let now = self.now();
        let left = (sub.expiration.as_second() - now.as_second()).max(0);
        let renew = Timestamp::from_second(now.as_second() + (left * 2 / 3).max(60))
            .unwrap_or(sub.expiration);
        let job = JobSpec {
            tenant: ch.tenant,
            lane: Lane::Background,
            kind: JobKind::RenewChannel {
                channel: sub.channel.clone(),
            },
            priority: 0,
            due: renew,
            dedup: Some(format!("renew:{}", sub.channel.0)),
        };
        // The renewal job names the channel, so the channel row goes first.
        self.ctl(move |c| c.put_channel(&ch)).await?;
        self.enqueue(job).await?;
        Ok(true)
    }

    /// Replaces a channel before it expires, then stops the old one.
    async fn renew(&self, cx: &AccountCx, old: &ChannelRecord) -> Result<(), EngineError> {
        if let (Some(push), Some(callback)) =
            (self.inner.provider.push(), &self.inner.config.callback)
            && let Some(col) = self.collection(old.tenant, old.collection).await?
            && !col.gone
        {
            let channels = self
                .collection_channels(old.tenant, old.account, old.collection)
                .await?;
            let opened = self
                .open_channel(cx, push, callback, old.tenant, &col, &channels)
                .await?;
            let sub = match opened {
                Ok(sub) => sub,
                // Refused now: like a first refusal, the collection is polled
                // only. Its row would otherwise read as a live channel until
                // long after Google's has expired.
                Err(SyncError::Permanent(e)) => {
                    self.refuse_push(col.id, &e);
                    self.drop_channels(cx, std::slice::from_ref(old)).await?;
                    let now = self.now();
                    return self
                        .enqueue(sync_job(old.tenant, old.account, col.id, Lane::Sync, now))
                        .await;
                }
                Err(e) => return Err(e.into()),
            };
            self.put_channel(cx, old.tenant, old.collection, sub)
                .await?;
        }
        self.drop_channels(cx, std::slice::from_ref(old)).await
    }

    /// Moves the occurrence index horizon of the tenant's series forward.
    /// A series that cannot be expanded keeps its rows and horizon and is
    /// reported by `range` as unindexed past it.
    async fn extend_index(&self, tenant: TenantId) -> Result<(), EngineError> {
        let c = &self.inner.config;
        let now = self.now();
        let until = after(now, c.index_forward);
        let row_limit = c.index_row_limit;
        let mut seen = HashSet::new();
        loop {
            let limit = u32::try_from(seen.len())
                .unwrap_or(u32::MAX)
                .saturating_add(EXTEND_BATCH);
            let refs = self
                .read(tenant, move |s| s.series_to_extend(until, limit))
                .await?;
            let more = refs.len() >= limit as usize;
            let fresh: Vec<_> = refs.into_iter().filter(|r| seen.insert(r.entity)).collect();
            if fresh.is_empty() {
                break;
            }
            let storage = self.inner.storage.clone();
            let extensions = blocking(move || {
                let store = storage.open(tenant)?;
                let mut out = Vec::new();
                for r in fresh {
                    let inc = Include {
                        placements: false,
                        raw: false,
                    };
                    let Some(json) = store.entity(r.entity, inc)?.and_then(|e| e.canonical) else {
                        continue;
                    };
                    match ingest::extension(json.get(), r.indexed_until, until, row_limit) {
                        Ok(rows) => out.push(SeriesExtension {
                            entity: r.entity,
                            from: r.indexed_until,
                            rows,
                            indexed_until: until,
                        }),
                        Err(e) => tracing::warn!(entity = %r.entity, error = %e, "series index not extended"),
                    }
                }
                Ok(out)
            })
            .await?;
            if !extensions.is_empty() {
                let w = OccurrenceWrite { extensions };
                self.write(tenant, move |s| s.extend_occurrences(&w))
                    .await?;
            }
            if !more {
                break;
            }
        }
        self.enqueue(JobSpec {
            tenant,
            lane: Lane::Background,
            kind: JobKind::ExtendIndex,
            priority: -10,
            due: after(now, c.extend_every),
            dedup: Some("extend".into()),
        })
        .await
    }

    async fn set_status(
        &self,
        tenant: TenantId,
        id: CollectionId,
        f: impl FnOnce(&mut CollectionStatus) + Send + 'static,
    ) -> Result<(), EngineError> {
        self.write(tenant, move |s| {
            let Some(col) = s.collections()?.into_iter().find(|c| c.id == id) else {
                return Ok(());
            };
            let mut status = col.status;
            f(&mut status);
            s.set_collection_state(id, &status)
        })
        .await
    }

    pub(crate) async fn collections(
        &self,
        tenant: TenantId,
    ) -> Result<Vec<CollectionState>, EngineError> {
        self.read(tenant, |s| s.collections()).await
    }

    async fn collection(
        &self,
        tenant: TenantId,
        id: CollectionId,
    ) -> Result<Option<CollectionState>, EngineError> {
        Ok(self
            .collections(tenant)
            .await?
            .into_iter()
            .find(|c| c.id == id))
    }

    pub(crate) fn lock(&self, id: CollectionId) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.inner.locks.lock().unwrap_or_else(|e| e.into_inner());
        locks.entry(id).or_default().clone()
    }

    pub(crate) fn account_lock(&self, id: AccountId) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self
            .inner
            .account_locks
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        locks.entry(id).or_default().clone()
    }
}

/// A collection's push channel after [`Engine::ensure_channel`].
enum Push {
    /// None: the collection is polled.
    None,
    /// It had one already.
    Existing,
    /// Opened just now.
    Opened,
}

fn sync_job(
    tenant: TenantId,
    account: AccountId,
    collection: CollectionId,
    lane: Lane,
    due: Timestamp,
) -> JobSpec {
    JobSpec {
        tenant,
        lane,
        kind: JobKind::Sync {
            account,
            collection,
        },
        priority: 0,
        due,
        dedup: Some(sync_dedup(collection)),
    }
}

/// Spaces an account's requests by the provider's documented rate.
struct Pacer {
    spacing: Option<Duration>,
    next: tokio::sync::Mutex<tokio::time::Instant>,
}

impl Pacer {
    fn new(per_minute: Option<u32>) -> Self {
        Self {
            spacing: per_minute
                .filter(|&n| n > 0)
                .map(|n| Duration::from_secs(60) / n),
            next: tokio::sync::Mutex::new(tokio::time::Instant::now()),
        }
    }
}

#[async_trait]
impl Limiter for Pacer {
    async fn acquire(&self) {
        let Some(spacing) = self.spacing else { return };
        let mut next = self.next.lock().await;
        tokio::time::sleep_until(*next).await;
        *next = (*next).max(tokio::time::Instant::now()) + spacing;
    }
}

/// A fresh channel id: the collection and the wall-clock time in nanoseconds,
/// strictly increasing within the process. Ids of a crashed run differ by time.
/// 46 characters of `[0-9a-v-]`, within Google's 64 of `[A-Za-z0-9-_+/=]`.
fn new_channel_id(col: CollectionId) -> ChannelId {
    use std::sync::atomic::{AtomicU64, Ordering};
    static LAST: AtomicU64 = AtomicU64::new(0);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX));
    let mut last = LAST.load(Ordering::Relaxed);
    loop {
        let n = now.max(last.saturating_add(1));
        match LAST.compare_exchange_weak(last, n, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return ChannelId(format!("{col}-{n}")),
            Err(seen) => last = seen,
        }
    }
}

/// The subscription to stop a pending channel by: its id, with the resource
/// and collection of a known channel of the same collection.
fn pending_sub(id: &ChannelId, known: &Subscription) -> Subscription {
    Subscription {
        channel: id.clone(),
        ..known.clone()
    }
}
