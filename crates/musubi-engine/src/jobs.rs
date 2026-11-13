//! Job bodies: account gating and tokens, discovery, the collection read
//! state machine, push channels and the occurrence index horizon.

use std::collections::HashSet;
use std::ops::Deref;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::StreamExt;
use jiff::Timestamp;
use musubi_model::capability::{DeltaSupport, PushSupport};
use musubi_model::control::{
    AccountRecord, AccountState, AccountTokens, ChannelRecord, ChannelState, Credentials, Job,
    JobKind, JobSpec, Lane,
};
use musubi_model::provider::{
    AccountCx, ChannelId, Credential, FetchHint, Limiter, Pass, PushSource, Start, Subscription,
};
use musubi_model::remote::RemoteRef;
use musubi_model::store::{
    CollectionError, CollectionState, CollectionStatus, Coverage, DiscoveryWrite, Include,
    OccurrenceWrite, PageWrite, SeriesExtension,
};
use musubi_model::{
    AccountId, ChangePage, CollectionId, Cursor, Provider, ReadId, ReauthReason, RemoteKey,
    RemoteObject, Seq, Storage, SyncError, TenantId, TenantStore, TimeWindow,
};
use secrecy::SecretString;
use tokio::sync::Semaphore;

use crate::{
    AccountRt, Codecs, CollectionRt, Engine, EngineError, MISSING_SCOPE, after, blocking,
    discover_job, gone_ok, ingest, sync_dedup,
};

/// An account's provider context: the adapter's [`AccountCx`] with the
/// account's provider instance and its codecs.
pub(crate) struct Cx {
    cx: AccountCx,
    pub(crate) provider: Arc<dyn Provider>,
    codecs: Codecs,
}

impl Deref for Cx {
    type Target = AccountCx;
    fn deref(&self) -> &AccountCx {
        &self.cx
    }
}

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
/// A collection asks for at most one rediscovery this often; asking again
/// sooner after a discovery listed it is a transient error.
const REDISCOVER_QUIET: Duration = Duration::from_secs(15 * 60);
/// A `missing_scope` collection is retried by a discovery not reporting the
/// kind unauthorised only after this long, unless the account's missing
/// kinds changed (an inconsistent provider would loop otherwise).
const MISSING_SCOPE_RETRY: Duration = Duration::from_secs(15 * 60);
/// Wait for a job whose collection another job is working on.
const BUSY_WAIT: Duration = Duration::from_secs(1);
/// Longest backoff of a failing job or of repeated full reads.
const BACKOFF_CAP: Duration = Duration::from_secs(3600);
/// A collection whose push channel the provider refused is polled only and
/// asked for a channel again after this long (Google channels live ~7 days).
const PUSH_RETRY: Duration = Duration::from_secs(86_400);
/// How long a collection stays full-read-only before a delta is tried again.
const DELTA_RETRY: Duration = Duration::from_secs(86_400);
/// Refs per [`Provider::fetch`] call when the provider sets no `max_batch`.
const FETCH_BATCH: u32 = 100;
/// Prefix of the synthetic validator of a server without ETags (plan 3.3).
const HASH_VALIDATOR: &str = "musubi:sha256:";

/// `base` doubled `n` times, at most [`BACKOFF_CAP`].
fn backoff(base: Duration, n: u32) -> Duration {
    base.saturating_mul(1 << n.min(12)).min(BACKOFF_CAP)
}

impl<S: Storage + 'static> Engine<S> {
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
        if let Some(until) = self.rt(&rec).blocked_until
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
        // `max_concurrency_per_account` (plan 7.4): over it, back in a second.
        let running = match work {
            Work::Renew(_) => None,
            _ => self.rt(&rec).running,
        };
        let _slot = match running.map(Semaphore::try_acquire_owned) {
            Some(Err(_)) => {
                return Outcome::Retry {
                    due: after(self.now(), BUSY_WAIT),
                    count: false,
                };
            }
            slot => slot,
        };
        let res = match self.cx(&rec, false).await {
            Ok(cx) => match self.work(&rec, &cx, job, &work).await {
                // Without OAuth there is nothing to refresh.
                Err(EngineError::Sync(SyncError::Unauthorized))
                    if cx.provider.oauth().is_none() =>
                {
                    Err(SyncError::ReauthRequired {
                        reason: ReauthReason::Rejected,
                    }
                    .into())
                }
                // The access token was revoked early: refresh once and run again.
                Err(EngineError::Sync(SyncError::Unauthorized)) => {
                    match self.cx(&rec, true).await {
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
                self.with_rt(&rec, |rt| rt.unauthorized = false);
                if let Work::Sync(c) = work {
                    self.with_col(c, |r| r.rediscovered = None);
                }
                Outcome::Done
            }
            Err(e) => self.failed(job, &rec, &work, e).await,
        }
    }

    async fn work(
        &self,
        rec: &AccountRecord,
        cx: &Cx,
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
                if !self.with_rt(rec, |rt| std::mem::replace(&mut rt.unauthorized, true)) =>
            {
                self.record_error(rec.tenant, col, e.to_string(), false)
                    .await;
                self.retry(job, e)
            }
            EngineError::Sync(SyncError::ReauthRequired { .. } | SyncError::Unauthorized) => {
                self.with_rt(rec, |rt| rt.unauthorized = false);
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
                self.with_rt(rec, |rt| rt.blocked_until = Some(until));
                Outcome::Retry {
                    due: until,
                    count: false,
                }
            }
            // Not retried until a discovery finds the kind authorised again.
            EngineError::Sync(SyncError::MissingScope { kind }) => {
                tracing::warn!(collection = ?col, %kind, "missing permission; rediscovering");
                self.record_error(rec.tenant, col, MISSING_SCOPE.into(), true)
                    .await;
                match self
                    .enqueue(discover_job(rec.tenant, rec.id, self.now()))
                    .await
                {
                    Ok(()) => Outcome::Done,
                    Err(e) => self.retry(job, e),
                }
            }
            // The collection's address changed: rediscover. A discovery that
            // works queues a sync of every collection it lists, this one too
            // (merged into this job, earliest due wins), so the job waits a
            // quiet period uncounted: it stays the collection's poll job if
            // the discovery fails or never lists it.
            EngineError::Sync(SyncError::Rediscover) if !matches!(work, Work::Discover) => {
                let now = self.now();
                let wait = Outcome::Retry {
                    due: after(now, REDISCOVER_QUIET),
                    count: false,
                };
                let Some(c) = col else {
                    // Queued or not, the job retries.
                    let _ = self.enqueue(discover_job(rec.tenant, rec.id, now)).await;
                    return self.retry(job, e);
                };
                let asked = self.with_col(c, |r| r.rediscovered);
                let (started, worked) = self.with_rt(rec, |rt| (rt.discoveries, rt.discovered));
                match asked {
                    // A discovery since the last ask listed it again: not a
                    // move (a stale listing, ACL drift). Backed off as a
                    // transient error until the ask expires.
                    Some((at, n)) if after(at, REDISCOVER_QUIET) > now && worked > n => {
                        tracing::warn!(collection = ?col, "collection still moved after a rediscovery");
                        self.record_error(rec.tenant, col, e.to_string(), false)
                            .await;
                        self.retry(job, e)
                    }
                    // The discovery asked for has not worked yet.
                    Some((at, _)) if after(at, REDISCOVER_QUIET) > now => wait,
                    _ => {
                        tracing::info!(collection = ?col, "collection moved; rediscovering");
                        self.with_col(c, |r| r.rediscovered = Some((now, started)));
                        match self.enqueue(discover_job(rec.tenant, rec.id, now)).await {
                            Ok(()) => wait,
                            Err(e) => self.retry(job, e),
                        }
                    }
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

    /// The account's provider context with its stored endpoint. OAuth:
    /// refreshes the access token when it is about to expire or `force` is
    /// set, if the provider has [`musubi_model::Provider::oauth`]. Basic
    /// credentials are used as they are.
    pub(crate) async fn cx(&self, rec: &AccountRecord, force: bool) -> Result<Cx, EngineError> {
        let Some((provider, codecs)) = self.inner.providers.get(&rec.provider).cloned() else {
            let msg = format!("no provider `{}` registered", rec.provider.0);
            return Err(SyncError::Permanent(msg.into()).into());
        };
        let account = rec.id;
        let vault = self.inner.vault.clone();
        let creds =
            blocking(move || Ok(vault.load(account)?))
                .await?
                .ok_or(SyncError::ReauthRequired {
                    reason: ReauthReason::Rejected,
                })?;
        let credential = match creds {
            Credentials::OAuth(tokens) => {
                Credential::Bearer(self.access(&*provider, account, tokens, force).await?)
            }
            Credentials::Basic { username, password } => Credential::Basic { username, password },
            _ => return Err(SyncError::Permanent("unsupported credentials".into()).into()),
        };
        let mut cx = AccountCx::new(
            account,
            credential,
            self.rt(rec).limiter,
            self.inner.clock.clone(),
        );
        cx.endpoint = self.ctl(move |c| c.endpoint(account)).await?;
        Ok(Cx {
            cx,
            provider,
            codecs,
        })
    }

    /// A usable access token: `tokens.access`, or a refreshed one stored first.
    async fn access(
        &self,
        provider: &dyn Provider,
        account: AccountId,
        tokens: AccountTokens,
        force: bool,
    ) -> Result<secrecy::SecretString, EngineError> {
        let now = self.now();
        let Some(oauth) = provider.oauth() else {
            return Ok(tokens.access);
        };
        if !force && tokens.access_until > after(now, REFRESH_EARLY) {
            return Ok(tokens.access);
        }
        let t = oauth.refresh(&tokens.refresh).await?;
        let fresh = AccountTokens {
            refresh: t.rotated_refresh.unwrap_or(tokens.refresh),
            access: t.access,
            access_until: after(now, t.expires_in),
        };
        let access = fresh.access.clone();
        let vault = self.inner.vault.clone();
        blocking(move || Ok(vault.store(account, &Credentials::OAuth(fresh))?)).await?;
        Ok(access)
    }

    fn rt(&self, rec: &AccountRecord) -> AccountRt {
        self.with_rt(rec, |rt| rt.clone())
    }

    pub(crate) fn with_col<R>(
        &self,
        id: CollectionId,
        f: impl FnOnce(&mut CollectionRt) -> R,
    ) -> R {
        let mut map = self
            .inner
            .collections
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        f(map.entry(id).or_default())
    }

    /// The account's in-memory state, made with its provider's limits.
    fn with_rt<R>(&self, rec: &AccountRecord, f: impl FnOnce(&mut AccountRt) -> R) -> R {
        let mut map = self
            .inner
            .accounts
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let rt = map.entry(rec.id).or_insert_with(|| {
            let limits = self
                .inner
                .providers
                .get(&rec.provider)
                .map(|(p, _)| p.descriptor().limits);
            let limit = |f: fn(&musubi_model::capability::Limits) -> Option<u32>| {
                limits.as_ref().and_then(f)
            };
            AccountRt {
                limiter: Arc::new(Pacer::new(limit(|l| l.requests_per_minute))),
                running: limit(|l| l.max_concurrency_per_account)
                    .map(|n| Arc::new(Semaphore::new(n.max(1) as usize))),
                blocked_until: None,
                unauthorized: false,
                discoveries: 0,
                discovered: 0,
            }
        });
        f(rt)
    }

    async fn discover(&self, rec: &AccountRecord, cx: &Cx) -> Result<(), EngineError> {
        let (tenant, account) = (rec.tenant, rec.id);
        let number = self.with_rt(rec, |rt| {
            rt.discoveries += 1;
            rt.discoveries
        });
        let was_missing = self.ctl(move |c| c.missing(account)).await?;
        // `missing`: the kinds without permission after this round. `keep`:
        // the kinds whose stored collections stay although the listing lacks
        // them (plan 3.5).
        let (mut collections, complete, missing, keep, endpoint) =
            match cx.provider.discover(cx).await {
                Ok(d) => {
                    // A kind in both lists is unauthorised.
                    let incomplete: Vec<_> = d
                        .incomplete
                        .into_iter()
                        .filter(|k| !d.unauthorized.contains(k))
                        .collect();
                    // An incomplete kind keeps its stored permission state.
                    let mut missing = d.unauthorized.clone();
                    missing.extend(was_missing.iter().filter(|k| incomplete.contains(k)));
                    let mut keep = d.unauthorized;
                    keep.extend(incomplete);
                    (d.collections, true, missing, keep, d.endpoint)
                }
                // A partial listing only adds and updates: a missing calendar
                // is not gone, and no missing permission is cleared.
                Err(SyncError::PartialDiscovery { found, source }) => {
                    tracing::warn!(%account, error = %source, "partial discovery");
                    (found, false, was_missing.clone(), Vec::new(), None)
                }
                Err(e) => return Err(e.into()),
            };
        // Under the account lock the state cannot become `Removing` meanwhile:
        // `remove_account` waits for it.
        let state = self.account(account).await?.map(|r| r.state);
        if !matches!(state, Some(AccountState::Active | AccountState::Connecting)) {
            return Ok(());
        }
        if let Some(e) = endpoint {
            self.ctl(move |c| c.set_endpoint(account, &e)).await?;
        }
        let stored: Vec<_> = self
            .collections(tenant)
            .await?
            .into_iter()
            .filter(|c| c.account == account)
            .collect();
        if complete {
            // Collections of an unauthorised or incomplete kind are absent
            // from the listing but not gone (plan 3.5).
            for col in &stored {
                if !col.gone
                    && keep.contains(&col.remote.kind)
                    && !collections.iter().any(|c| c.remote == col.remote.remote)
                {
                    collections.push(col.remote.clone());
                }
            }
        }
        let d = DiscoveryWrite {
            account,
            collections,
            complete,
        };
        self.write(tenant, move |s| s.apply_discovery(&d)).await?;
        if complete {
            let kinds = missing.clone();
            self.ctl(move |c| c.set_missing(account, &kinds)).await?;
        }
        self.with_rt(rec, |rt| rt.discovered = rt.discovered.max(number));
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
            self.drop_collection(tenant, cx, col).await?;
        }
        if state == Some(AccountState::Connecting) {
            self.ctl(move |c| c.set_account_state(account, AccountState::Active))
                .await?;
        }
        let now = self.now();
        for col in cols.iter().filter(|c| !c.gone) {
            let kind = col.remote.kind;
            let scoped = col
                .status
                .error
                .as_ref()
                .filter(|e| e.message == MISSING_SCOPE)
                .map(|e| e.at);
            // Known to have permission back: the kind was listed this round
            // and was missing before, or the error is old enough to try again.
            let back = |at| {
                complete
                    && !keep.contains(&kind)
                    && (was_missing.contains(&kind) || after(at, MISSING_SCOPE_RETRY) <= now)
            };
            // Without permission, or not known to have it back: not retried.
            if missing.contains(&kind) || scoped.is_some_and(|at| !back(at)) {
                continue;
            }
            if scoped.is_some() {
                self.set_status(tenant, col.id, |s| s.error = None).await?;
            }
            self.enqueue(sync_job(tenant, account, col.id, Lane::Sync, now))
                .await?;
        }
        let c = &self.inner.config;
        // A kind skipped this round is asked again soon, not in hours.
        let skipped = !complete || keep.iter().any(|k| !missing.contains(k));
        let every = if skipped {
            REDISCOVER_QUIET.min(c.rediscover_every)
        } else {
            c.rediscover_every
        };
        self.enqueue(discover_job(tenant, account, after(now, every)))
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
        cx: &Cx,
        col: &CollectionState,
    ) -> Result<(), EngineError> {
        let id = col.id;
        let channels = self.collection_channels(tenant, cx.account, id).await?;
        self.drop_channels(Some(cx), &channels, Some(col)).await?;
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
    /// the calendar), else with `col`'s stored
    /// [`CollectionState::push_resource`]; without either, or without `cx`
    /// (an account that cannot sign in), it is only forgotten and expires there.
    pub(crate) async fn drop_channels(
        &self,
        cx: Option<&Cx>,
        chs: &[ChannelRecord],
        col: Option<&CollectionState>,
    ) -> Result<(), EngineError> {
        // Pending rows first: they borrow an open row's resource, so a crash
        // between the two must not leave a pending row behind alone.
        let (pending, open): (Vec<_>, Vec<_>) = chs.iter().partition(|ch| ch.open().is_none());
        for ch in pending.into_iter().chain(open) {
            let sub = match &ch.state {
                ChannelState::Open(s) => Some(s.clone()),
                ChannelState::Pending(id) => chs
                    .iter()
                    .filter(|o| o.collection == ch.collection)
                    .find_map(ChannelRecord::open)
                    .map(|known| pending_sub(id, known))
                    .or_else(|| {
                        let col = col.filter(|c| c.id == ch.collection)?;
                        let resource = col.push_resource.clone()?;
                        Some(Subscription::new(
                            id.clone(),
                            col.remote.remote.clone(),
                            resource,
                            SecretString::from(String::new()),
                            self.now(),
                        ))
                    }),
            };
            if let (Some(cx), Some(sub)) = (cx, sub)
                && let Some(push) = cx.provider.push()
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
        cx: &Cx,
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
        cx: &Cx,
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
            Ok(sub) => {
                // Kept for stopping a lone pending channel later (plan ch. 10 item 3).
                if col.push_resource.as_deref() != Some(sub.resource.as_str()) {
                    let (cid, resource) = (col.id, sub.resource.clone());
                    gone_ok(
                        self.write(tenant, move |s| s.set_push_resource(cid, &resource))
                            .await,
                    )?;
                }
                self.stop_pending(cx, push, chs, sub).await?
            }
            // Refused or not tried: no channel was made.
            Err(
                SyncError::Permanent(_)
                | SyncError::RateLimited { .. }
                | SyncError::ReauthRequired { .. }
                | SyncError::Unauthorized,
            ) => self.ctl(move |c| c.delete_channel(&id)).await?,
            // The channel may be open (lost reply, timeout, 5xx): the row
            // stays until it is stopped.
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
        cx: &Cx,
        id: CollectionId,
        lane: Lane,
    ) -> Result<(), EngineError> {
        let tenant = rec.tenant;
        let Some(col) = self.collection(tenant, id).await? else {
            return Ok(());
        };
        if col.gone {
            return Ok(());
        }
        let background = lane == Lane::Background;
        if let Some(r) = col.active_read {
            if col.read_complete {
                return self.finish(rec, cx, &col, r, true).await;
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
                res => res.map(|_| r)?,
            };
            return self.finish(rec, cx, &col, r, true).await;
        }
        let wants_full = col.cursor.is_none() || col.status.coverage != Coverage::Full;
        if background && wants_full {
            let r = self.full_read(tenant, cx, &col).await?;
            return self.finish(rec, cx, &col, r, true).await;
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
        let no_delta = delta_support(&*cx.provider, &col) == Some(DeltaSupport::None);
        let full_only = |rt: &CollectionRt| no_delta || rt.full_only_until.is_some_and(|t| t > now);
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
                self.finish(rec, cx, &col, r, true).await
            }
            // The delta listed the whole collection (`Pass::Full`, a ctag
            // change): what it did not list is gone, as after a full read.
            Ok(Some(r)) => self.finish(rec, cx, &col, r, false).await,
            Ok(None) => self.synced(rec, cx, &col, false).await,
            Err(e) => Err(e),
        }
    }

    /// The window around today, read before the full read so the user sees
    /// current data early. It deletes nothing.
    async fn window_read(
        &self,
        tenant: TenantId,
        cx: &Cx,
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
        cx: &Cx,
        col: &CollectionState,
    ) -> Result<ReadId, EngineError> {
        let last = col.active_read.max(col.last_read).map_or(0, |r| r.0);
        let r = ReadId(last + 1);
        self.read_pass(tenant, cx, col, None, Start::Full, Some(r))
            .await?;
        Ok(r)
    }

    /// Reads pages until the provider's last one; one transaction per page.
    /// Returns the read the pages were part of: `read`, or a new one started
    /// by a delta whose pages turned out a full listing (`Pass::Full`).
    ///
    /// shortcut: no pipelining, the next page is requested only after the
    /// previous one committed; overlap fetch and commit once full reads of
    /// large calendars are measured too slow.
    async fn read_pass(
        &self,
        tenant: TenantId,
        cx: &Cx,
        col: &CollectionState,
        cursor: Option<Cursor>,
        start: Start,
        mut read: Option<ReadId>,
    ) -> Result<Option<ReadId>, EngineError> {
        let mut hint = FetchHint::new(start);
        hint.page_size = self.inner.config.page_size;
        let mut pages = cx.provider.changes(cx, &col.remote.remote, cursor, hint);
        while let Some(page) = pages.next().await {
            let page = page?;
            if read.is_none() && page.pass == Pass::Full {
                let last = col.active_read.max(col.last_read).map_or(0, |r| r.0);
                read = Some(ReadId(last + 1));
            }
            let last = page.last;
            self.ingest(tenant, cx, col, page, read).await?;
            if last {
                break;
            }
        }
        Ok(read)
    }

    /// Fetches the page's changed stubs (plan 3.4), decodes and expands the
    /// page off the runtime, then commits it with its cursor.
    async fn ingest(
        &self,
        tenant: TenantId,
        cx: &Cx,
        col: &CollectionState,
        page: ChangePage,
        read: Option<ReadId>,
    ) -> Result<Seq, EngineError> {
        let ChangePage {
            pass,
            mut upserts,
            stubs,
            removals,
            cursor,
            last,
            ..
        } = page;
        let id = col.id;
        let hashed = upserts
            .iter()
            .map(|o| &o.validator)
            .chain(stubs.iter().filter_map(|s| s.validator.as_ref()))
            .any(|v| v.0.starts_with(HASH_VALIDATOR));
        let index = self.index();
        let storage = self.inner.storage.clone();
        let keys: Vec<RemoteKey> = upserts
            .iter()
            .map(|o| o.key.clone())
            .chain(stubs.iter().map(|s| s.key.clone()))
            .collect();
        let known = blocking(move || Ok(storage.open(tenant)?.items_by_remote(id, &keys)?)).await?;
        let mut unchanged = Vec::new();
        let mut refs = Vec::new();
        for stub in stubs {
            // An equal validator is unchanged whatever its versions: a stale
            // view is the re-decode's job, not a reason to download.
            let same = known.iter().any(|v| {
                v.key == stub.key && !v.tombstone && stub.validator.as_ref() == Some(&v.validator)
            });
            match same {
                true => unchanged.push(stub.key),
                false => refs.push(RemoteRef {
                    collection: col.remote.remote.clone(),
                    key: stub.key,
                }),
            }
        }
        // An object `fetch` does not return was deleted since the listing:
        // absence removes it after a full read, the next delta otherwise.
        upserts.extend(self.fetch(cx, refs).await?);
        let codecs = cx.codecs;
        let (upserts, more) =
            blocking(move || Ok(ingest::prepare(upserts, known, &codecs, index))).await?;
        unchanged.extend(more);
        let changed = !upserts.is_empty() || !removals.is_empty();
        self.with_col(id, |rt| {
            rt.changed |= changed;
            rt.hashed |= hashed;
        });
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
        cx: &Cx,
        col: &CollectionState,
        r: ReadId,
        full: bool,
    ) -> Result<(), EngineError> {
        let (tenant, id) = (rec.tenant, col.id);
        let missing = self.read(tenant, move |s| s.unseen_count(id, r)).await?;
        if missing > 0 {
            self.with_col(id, |rt| rt.changed = true);
        }
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
                let found = self.fetch(cx, refs).await?;
                if !found.is_empty() {
                    let mut page = ChangePage::new(Pass::Full, None, false);
                    page.upserts = found;
                    self.ingest(tenant, cx, col, page, Some(r)).await?;
                }
                if !more {
                    break;
                }
                from = Some(last);
            }
        }
        self.write(tenant, move |s| s.finish_full_read(id, r))
            .await?;
        self.synced(rec, cx, col, full).await
    }

    /// [`Provider::fetch`] in calls of at most `limits.max_batch` refs.
    async fn fetch(&self, cx: &Cx, refs: Vec<RemoteRef>) -> Result<Vec<RemoteObject>, EngineError> {
        let batch = cx
            .provider
            .descriptor()
            .limits
            .max_batch
            .unwrap_or(FETCH_BATCH);
        let mut out = Vec::new();
        for chunk in refs.chunks(batch.max(1) as usize) {
            out.extend(cx.provider.fetch(cx, chunk).await?);
        }
        Ok(out)
    }

    /// Records a successful sync, makes sure a push channel exists and
    /// schedules the safety poll.
    async fn synced(
        &self,
        rec: &AccountRecord,
        cx: &Cx,
        col: &CollectionState,
        full: bool,
    ) -> Result<(), EngineError> {
        let now = self.now();
        let c = &self.inner.config;
        let no_delta = delta_support(&*cx.provider, col) == Some(DeltaSupport::None);
        let (full_reads, full_only, quiet, hashed) = self.with_col(col.id, |rt| {
            rt.full_reads = if full { rt.full_reads + 1 } else { 0 };
            if !full {
                // A delta worked: the cursor is usable again.
                rt.full_only_until = None;
            }
            rt.quiet = match std::mem::take(&mut rt.changed) {
                true => 0,
                false => rt.quiet.saturating_add(1),
            };
            // Full-read-only until a delta works, also once the retry is due:
            // the read that ends past it is no reason to back off.
            let full_only = no_delta || rt.full_only_until.is_some();
            (rt.full_reads, full_only, rt.quiet, rt.hashed)
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
        let adaptive = || self.adaptive(rec.tenant, quiet, hashed);
        let every = match push {
            _ if let Some(wait) = backed_off => wait,
            // No catch-up delta: each sync of such a collection is a full read.
            Push::Existing if full_only => c.poll_every_push,
            _ if full_only => adaptive(),
            // A full read ends at its own snapshot, and a sync job arriving
            // during it only deferred to it; a new channel missed what changed
            // before it existed. One delta now picks both up.
            _ if full => crate::COALESCE,
            Push::Opened => crate::COALESCE,
            Push::Existing => c.poll_every_push,
            Push::None => adaptive(),
        };
        let mut job = sync_job(rec.tenant, rec.id, col.id, Lane::Sync, now);
        job.due = after(now, every);
        self.enqueue(job).await
    }

    /// The adaptive poll interval of a collection without push (plan 7.2):
    /// `min · 2^quiet` within the bounds of the tenant's interest, ±10 %.
    fn adaptive(&self, tenant: TenantId, quiet: u32, hashed: bool) -> Duration {
        let c = &self.inner.config;
        let now = self.now();
        let touched = self
            .inner
            .interest
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&tenant)
            .copied();
        let bounds = match touched {
            // Every read of a hashed collection downloads every body: it does
            // not poll faster than normal, while sleeping it sleeps too.
            Some(t) if after(t, c.active_for) > now && hashed => c.poll_normal,
            Some(t) if after(t, c.active_for) > now => c.poll_active,
            // Interest before a restart is unknown: it counts from the start.
            t => {
                let since = t.map_or(self.inner.started, |t| t.max(self.inner.started));
                match after(since, c.sleep_after) <= now {
                    true => c.poll_sleeping,
                    false => c.poll_normal,
                }
            }
        };
        let base = bounds
            .min
            .saturating_mul(1 << quiet.min(20))
            .min(bounds.max)
            .max(bounds.min);
        use std::hash::BuildHasher;
        let r = std::hash::RandomState::new().hash_one(());
        let f = (r % 2001) as f64 / 1000.0 - 1.0;
        base.mul_f64(1.0 + 0.1 * f)
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
        cx: &Cx,
        col: &CollectionState,
    ) -> Result<Push, EngineError> {
        let (Some(push), Some(callback)) = (cx.provider.push(), &self.inner.config.callback) else {
            return Ok(Push::None);
        };
        if push_support(&*cx.provider, col) == Some(PushSupport::None) {
            return Ok(Push::None);
        }
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
        cx: &Cx,
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
            self.drop_channels(Some(cx), std::slice::from_ref(&ch), None)
                .await?;
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
    async fn renew(&self, cx: &Cx, old: &ChannelRecord) -> Result<(), EngineError> {
        if let (Some(push), Some(callback)) = (cx.provider.push(), &self.inner.config.callback)
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
                    self.drop_channels(Some(cx), std::slice::from_ref(old), Some(&col))
                        .await?;
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
        self.drop_channels(Some(cx), std::slice::from_ref(old), None)
            .await
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

/// The collection's delta support: its own, else its kind's default.
fn delta_support(provider: &dyn Provider, col: &CollectionState) -> Option<DeltaSupport> {
    col.remote.delta.or_else(|| {
        let d = provider.descriptor();
        d.kinds
            .iter()
            .find(|k| k.kind == col.remote.kind)
            .map(|k| k.delta)
    })
}

/// The collection's push support: its own, else its kind's default.
fn push_support(provider: &dyn Provider, col: &CollectionState) -> Option<PushSupport> {
    col.remote.push.or_else(|| {
        let d = provider.descriptor();
        d.kinds
            .iter()
            .find(|k| k.kind == col.remote.kind)
            .map(|k| k.push)
    })
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

pub(crate) fn sync_job(
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
    let mut sub = known.clone();
    sub.channel = id.clone();
    sub
}
