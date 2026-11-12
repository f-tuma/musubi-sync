//! [`ControlStore`] over `control.db` (`synchronous=FULL`: a returned write is durable).
//!
//! Job scoping:
//! - `jobs.account` is derived from the kind on enqueue (see [`crate::schema::CONTROL`]);
//!   `delete_account` deletes the jobs with that account, `ExtendIndex` and
//!   `Maintain` belong to the tenant and stay until `delete_tenant`.
//! - Dedup keys are scoped to the tenant: `(tenant, dedup)` among unleased jobs.
//!
//! Channel tokens are stored in plain text. They are not credentials: a forged
//! notification can only trigger a sync of the channel's collection, which reads
//! the provider with the account's own tokens. `PushSource::verify` needs the
//! plain value on every notification, without a vault round trip.

use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use jiff::Timestamp;
use musubi_model::capability::ProviderId;
use musubi_model::control::{
    AccountRecord, AccountState, ApiKeyHash, ApiKeyRecord, ControlStore, Job, JobId, JobKind,
    JobSpec, Lane, LeaseId, OAuthPending, OAuthStateHash, SealedTokens,
};
use musubi_model::control::{ChannelRecord, ChannelState};
use musubi_model::error::StoreError;
use musubi_model::id::{AccountId, CollectionId, TenantId};
use musubi_model::provider::{ChannelId, Subscription};
use musubi_model::remote::CollectionRef;
use rusqlite::{
    Connection, ErrorCode, OptionalExtension, Row, Transaction, TransactionBehavior, params,
};
use secrecy::{ExposeSecret, SecretString};

use crate::{Result, connect, enum_text, opt_ts, parse_enum, ts, us};

/// The instance's control plane (`control.db`).
pub struct SqliteControl {
    writer: Mutex<Connection>,
    reader: Mutex<Connection>,
}

fn lock(m: &Mutex<Connection>) -> MutexGuard<'_, Connection> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// A unique or primary key violation becomes `AlreadyExists`.
fn unique(e: rusqlite::Error) -> crate::Error {
    match e.sqlite_error_code() {
        Some(ErrorCode::ConstraintViolation) => StoreError::AlreadyExists.into(),
        _ => e.into(),
    }
}

fn found(n: usize) -> Result<()> {
    if n == 0 {
        Err(StoreError::NotFound.into())
    } else {
        Ok(())
    }
}

fn after(now: Timestamp, d: Duration) -> Result<Timestamp> {
    let d = jiff::SignedDuration::try_from(d).map_err(crate::corrupt)?;
    now.checked_add(d).map_err(crate::corrupt)
}

impl SqliteControl {
    /// Opens (creating if missing) the control database at `path`.
    pub fn open(path: &std::path::Path) -> Result<Self, StoreError> {
        let mut writer = connect(path, "FULL", 1000)?;
        writer
            .execute_batch(crate::schema::CONTROL)
            .map_err(crate::Error::from)?;
        crate::schema::migrate_control(&mut writer)?;
        let reader = connect(path, "FULL", 1000)?;
        Ok(Self {
            writer: Mutex::new(writer),
            reader: Mutex::new(reader),
        })
    }

    /// Releases every job lease; returns how many were held. For a node
    /// starting on its own data directory: leases left by a killed run would
    /// otherwise hold their jobs for the rest of `lease_for`. Jobs that share a
    /// dedup key merge into the oldest, keeping the earliest `due`.
    pub fn release_leases(&self) -> Result<usize, StoreError> {
        self.write(|c| {
            c.execute(
                "UPDATE jobs SET due = (SELECT min(due) FROM jobs k
                   WHERE k.tenant = jobs.tenant AND k.dedup = jobs.dedup)
                 WHERE dedup IS NOT NULL",
                [],
            )?;
            c.execute(
                "DELETE FROM jobs WHERE dedup IS NOT NULL AND EXISTS (SELECT 1 FROM jobs k
                   WHERE k.tenant = jobs.tenant AND k.dedup = jobs.dedup AND k.id < jobs.id)",
                [],
            )?;
            Ok(c.execute(
                "UPDATE jobs SET lease = NULL, lease_until = NULL WHERE lease IS NOT NULL",
                [],
            )?)
        })
    }

    fn read<T>(&self, f: impl FnOnce(&Transaction<'_>) -> Result<T>) -> Result<T, StoreError> {
        let mut c = lock(&self.reader);
        let tx = c.transaction().map_err(crate::Error::from)?;
        Ok(f(&tx)?)
    }

    fn write<T>(&self, f: impl FnOnce(&Transaction<'_>) -> Result<T>) -> Result<T, StoreError> {
        let mut c = lock(&self.writer);
        let tx = c
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(crate::Error::from)?;
        let r = f(&tx)?;
        tx.commit().map_err(crate::Error::from)?;
        Ok(r)
    }
}

fn account(r: &Row<'_>) -> rusqlite::Result<Result<AccountRecord>> {
    let (id, tenant, provider, subject, state): ([u8; 16], [u8; 16], String, String, String) =
        (r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?);
    Ok(parse_enum(state).map(|state| AccountRecord {
        id: AccountId::from_bytes(id),
        tenant: TenantId::from_bytes(tenant),
        provider: ProviderId(provider),
        subject,
        state,
    }))
}

const ACCOUNT: &str = "SELECT id, tenant, provider, subject, state FROM accounts";

fn api_key(r: &Row<'_>) -> rusqlite::Result<Result<ApiKeyRecord>> {
    let (hash, tenant, scope, created, revoked): ([u8; 32], [u8; 16], String, i64, Option<i64>) =
        (r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?);
    Ok((|| {
        Ok(ApiKeyRecord {
            hash: ApiKeyHash(hash),
            tenant: TenantId::from_bytes(tenant),
            scope: parse_enum(scope)?,
            created: ts(created)?,
            revoked: opt_ts(revoked)?,
        })
    })())
}

const CHANNEL: &str =
    "SELECT id, tenant, account, collection, remote, resource, token, expiration FROM channels";

fn channel(r: &Row<'_>) -> rusqlite::Result<Result<ChannelRecord>> {
    let (id, tenant, account, collection): (String, [u8; 16], [u8; 16], [u8; 16]) =
        (r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?);
    type Open = (Option<String>, Option<String>, Option<String>, Option<i64>);
    let (remote, resource, token, expiration): Open = (r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?);
    let id = ChannelId(id);
    Ok((|| {
        let state = match (remote, resource, token, expiration) {
            (None, None, None, None) => ChannelState::Pending(id),
            (Some(remote), Some(resource), Some(token), Some(expiration)) => {
                ChannelState::Open(Subscription {
                    channel: id,
                    collection: CollectionRef(remote),
                    resource,
                    token: SecretString::from(token),
                    expiration: ts(expiration)?,
                })
            }
            _ => return Err(crate::corrupt(format!("channel {}: partly pending", id.0))),
        };
        Ok(ChannelRecord {
            tenant: TenantId::from_bytes(tenant),
            account: AccountId::from_bytes(account),
            collection: CollectionId::from_bytes(collection),
            state,
        })
    })())
}

fn collect<T>(
    c: &Connection,
    sql: &str,
    p: impl rusqlite::Params,
    f: fn(&Row<'_>) -> rusqlite::Result<Result<T>>,
) -> Result<Vec<T>> {
    let mut st = c.prepare_cached(sql)?;
    let rows = st.query_map(p, f)?;
    rows.map(|r| r?).collect()
}

/// The account a job belongs to, or `None` for tenant-level jobs. Renewing a
/// channel that is not stored is `NotFound`: the job would have no account and
/// outlive `delete_account`.
fn job_account(c: &Connection, kind: &JobKind) -> Result<Option<[u8; 16]>> {
    Ok(match kind {
        JobKind::Discover { account } | JobKind::Sync { account, .. } => Some(account.to_bytes()),
        JobKind::RenewChannel { channel } => Some(
            c.prepare_cached("SELECT account FROM channels WHERE id = ?1")?
                .query_row([&channel.0], |r| r.get(0))
                .optional()?
                .ok_or(StoreError::NotFound)?,
        ),
        JobKind::ExtendIndex | JobKind::Maintain => None,
    })
}

impl ControlStore for SqliteControl {
    fn insert_tenant(&self, t: TenantId) -> Result<(), StoreError> {
        self.write(|c| {
            c.execute("INSERT INTO tenants(id) VALUES (?1)", [t.to_bytes()])
                .map_err(unique)?;
            Ok(())
        })
    }

    fn delete_tenant(&self, t: TenantId) -> Result<(), StoreError> {
        self.write(|c| {
            let id = t.to_bytes();
            found(c.execute("DELETE FROM tenants WHERE id = ?1", [id])?)?;
            for table in ["api_keys", "accounts", "oauth_states", "jobs", "channels"] {
                c.execute(&format!("DELETE FROM {table} WHERE tenant = ?1"), [id])?;
            }
            Ok(())
        })
    }

    fn insert_api_key(&self, k: &ApiKeyRecord) -> Result<(), StoreError> {
        self.write(|c| {
            c.execute(
                "INSERT INTO api_keys(hash, tenant, scope, created, revoked) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    k.hash.0,
                    k.tenant.to_bytes(),
                    enum_text(&k.scope)?,
                    us(k.created),
                    k.revoked.map(us)
                ],
            )
            .map_err(unique)?;
            Ok(())
        })
    }

    fn find_api_key(&self, hash: &ApiKeyHash) -> Result<Option<ApiKeyRecord>, StoreError> {
        self.read(|c| {
            Ok(collect(
                c,
                "SELECT hash, tenant, scope, created, revoked FROM api_keys
                 WHERE hash = ?1 AND revoked IS NULL",
                [hash.0],
                api_key,
            )?
            .pop())
        })
    }

    fn revoke_api_key(&self, hash: &ApiKeyHash, at: Timestamp) -> Result<(), StoreError> {
        self.write(|c| {
            found(c.execute(
                "UPDATE api_keys SET revoked = coalesce(revoked, ?2) WHERE hash = ?1",
                params![hash.0, us(at)],
            )?)
        })
    }

    fn upsert_account(&self, a: &AccountRecord) -> Result<(), StoreError> {
        self.write(|c| {
            // An account never moves to another tenant: the same id there is `AlreadyExists`.
            let n = c.execute(
                "INSERT INTO accounts(id, tenant, provider, subject, state) VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(id) DO UPDATE SET provider = ?3, subject = ?4, state = ?5
                 WHERE tenant = ?2",
                params![
                    a.id.to_bytes(),
                    a.tenant.to_bytes(),
                    a.provider.0,
                    a.subject,
                    enum_text(&a.state)?
                ],
            )
            .map_err(unique)?;
            if n == 0 {
                return Err(StoreError::AlreadyExists.into());
            }
            Ok(())
        })
    }

    fn account(&self, id: AccountId) -> Result<Option<AccountRecord>, StoreError> {
        self.read(|c| {
            Ok(collect(
                c,
                &format!("{ACCOUNT} WHERE id = ?1"),
                [id.to_bytes()],
                account,
            )?
            .pop())
        })
    }

    fn account_by_subject(
        &self,
        tenant: TenantId,
        provider: &ProviderId,
        subject: &str,
    ) -> Result<Option<AccountRecord>, StoreError> {
        self.read(|c| {
            Ok(collect(
                c,
                &format!("{ACCOUNT} WHERE tenant = ?1 AND provider = ?2 AND subject = ?3"),
                params![tenant.to_bytes(), provider.0, subject],
                account,
            )?
            .pop())
        })
    }

    fn accounts(&self, tenant: TenantId) -> Result<Vec<AccountRecord>, StoreError> {
        self.read(|c| {
            collect(
                c,
                &format!("{ACCOUNT} WHERE tenant = ?1 ORDER BY id"),
                [tenant.to_bytes()],
                account,
            )
        })
    }

    fn delete_account(&self, id: AccountId) -> Result<(), StoreError> {
        self.write(|c| {
            let id = id.to_bytes();
            found(c.execute("DELETE FROM accounts WHERE id = ?1", [id])?)?;
            c.execute("DELETE FROM jobs WHERE account = ?1", [id])?;
            c.execute("DELETE FROM channels WHERE account = ?1", [id])?;
            c.execute("DELETE FROM oauth_states WHERE account = ?1", [id])?;
            Ok(())
        })
    }

    fn set_account_state(&self, id: AccountId, s: AccountState) -> Result<(), StoreError> {
        self.write(|c| {
            found(c.execute(
                "UPDATE accounts SET state = ?2 WHERE id = ?1",
                params![id.to_bytes(), enum_text(&s)?],
            )?)
        })
    }

    fn write_tokens(&self, id: AccountId, sealed: &SealedTokens) -> Result<(), StoreError> {
        self.write(|c| {
            found(c.execute(
                "UPDATE accounts SET tokens = ?2 WHERE id = ?1",
                params![id.to_bytes(), sealed.0],
            )?)
        })
    }

    fn read_tokens(&self, id: AccountId) -> Result<Option<SealedTokens>, StoreError> {
        self.read(|c| {
            let t: Option<Option<Vec<u8>>> = c
                .query_row(
                    "SELECT tokens FROM accounts WHERE id = ?1",
                    [id.to_bytes()],
                    |r| r.get(0),
                )
                .optional()?;
            Ok(t.flatten().map(SealedTokens))
        })
    }

    fn put_oauth_state(&self, s: &OAuthPending) -> Result<(), StoreError> {
        self.write(|c| {
            c.execute(
                "DELETE FROM oauth_states WHERE expires <= ?1",
                [us(Timestamp::now())],
            )?;
            c.execute(
                "INSERT INTO oauth_states(hash, tenant, provider, account, verifier, expires)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    s.state.0,
                    s.tenant.to_bytes(),
                    s.provider.0,
                    s.account.map(|a| a.to_bytes()),
                    s.verifier.expose_secret(),
                    us(s.expires)
                ],
            )
            .map_err(unique)?;
            Ok(())
        })
    }

    fn take_oauth_state(
        &self,
        hash: &OAuthStateHash,
        now: Timestamp,
    ) -> Result<Option<OAuthPending>, StoreError> {
        self.write(|c| {
            let row = c
                .query_row(
                    "DELETE FROM oauth_states WHERE hash = ?1
                     RETURNING tenant, provider, account, verifier, expires",
                    [hash.0],
                    |r| {
                        Ok((
                            r.get::<_, [u8; 16]>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, Option<[u8; 16]>>(2)?,
                            r.get::<_, String>(3)?,
                            r.get::<_, i64>(4)?,
                        ))
                    },
                )
                .optional()?;
            let Some((tenant, provider, account, verifier, expires)) = row else {
                return Ok(None);
            };
            let expires = ts(expires)?;
            if expires <= now {
                return Ok(None);
            }
            Ok(Some(OAuthPending {
                state: *hash,
                tenant: TenantId::from_bytes(tenant),
                provider: ProviderId(provider),
                account: account.map(AccountId::from_bytes),
                verifier: SecretString::from(verifier),
                expires,
            }))
        })
    }

    fn enqueue(&self, job: &JobSpec) -> Result<JobId, StoreError> {
        self.write(|c| {
            let tenant = job.tenant.to_bytes();
            if let Some(key) = &job.dedup {
                // An unleased job, else one whose lease expired (its holder gets
                // `LeaseLost`). `enqueue` has no `now`, so expiry is by the wall clock.
                let existing: Option<i64> = c
                    .query_row(
                        "UPDATE jobs SET due = min(due, ?3), lease = NULL, lease_until = NULL
                         WHERE id = (SELECT id FROM jobs
                           WHERE tenant = ?1 AND dedup = ?2 AND (lease IS NULL OR lease_until <= ?4)
                           ORDER BY lease IS NOT NULL, id LIMIT 1)
                         RETURNING id",
                        params![tenant, key, us(job.due), us(Timestamp::now())],
                        |r| r.get(0),
                    )
                    .optional()?;
                if let Some(id) = existing {
                    return Ok(JobId(id as u64));
                }
            }
            c.execute(
                "INSERT INTO jobs(tenant, account, lane, kind, priority, due, dedup)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    tenant,
                    job_account(c, &job.kind)?,
                    enum_text(&job.lane)?,
                    serde_json::to_string(&job.kind)?,
                    job.priority,
                    us(job.due),
                    job.dedup
                ],
            )?;
            Ok(JobId(c.last_insert_rowid() as u64))
        })
    }

    fn lease_due(
        &self,
        lane: Lane,
        now: Timestamp,
        limit: u32,
        lease_for: Duration,
    ) -> Result<Vec<Job>, StoreError> {
        let until = after(now, lease_for)?;
        self.write(|c| {
            let ids: Vec<i64> = c
                .prepare_cached(
                    "SELECT id FROM jobs
                     WHERE lane = ?1 AND due <= ?2 AND (lease IS NULL OR lease_until <= ?2)
                     ORDER BY priority DESC, due, id LIMIT ?3",
                )?
                .query_map(params![enum_text(&lane)?, us(now), limit], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            let mut jobs = Vec::with_capacity(ids.len());
            for id in ids {
                let lease: i64 = c.query_row(
                    "UPDATE meta SET v = v + 1 WHERE k = 'lease' RETURNING v",
                    [],
                    |r| r.get(0),
                )?;
                let (tenant, lane, kind, priority, due, dedup, attempts): (
                    [u8; 16],
                    String,
                    String,
                    i32,
                    i64,
                    Option<String>,
                    u32,
                ) = c.query_row(
                    "UPDATE jobs SET lease = ?2, lease_until = ?3 WHERE id = ?1
                     RETURNING tenant, lane, kind, priority, due, dedup, attempts",
                    params![id, lease, us(until)],
                    |r| {
                        Ok((
                            r.get(0)?,
                            r.get(1)?,
                            r.get(2)?,
                            r.get(3)?,
                            r.get(4)?,
                            r.get(5)?,
                            r.get(6)?,
                        ))
                    },
                )?;
                jobs.push(Job {
                    id: JobId(id as u64),
                    lease: LeaseId(lease as u64),
                    lease_until: until,
                    attempts,
                    spec: JobSpec {
                        tenant: TenantId::from_bytes(tenant),
                        lane: parse_enum(lane)?,
                        kind: serde_json::from_str(&kind)?,
                        priority,
                        due: ts(due)?,
                        dedup,
                    },
                });
            }
            Ok(jobs)
        })
    }

    fn extend_lease(
        &self,
        id: JobId,
        lease: LeaseId,
        now: Timestamp,
        lease_for: Duration,
    ) -> Result<Timestamp, StoreError> {
        let until = after(now, lease_for)?;
        self.write(|c| {
            held(c.execute(
                "UPDATE jobs SET lease_until = ?3 WHERE id = ?1 AND lease = ?2",
                params![id.0 as i64, lease.0 as i64, us(until)],
            )?)?;
            Ok(until)
        })
    }

    fn complete(&self, id: JobId, lease: LeaseId) -> Result<(), StoreError> {
        self.write(|c| {
            held(c.execute(
                "DELETE FROM jobs WHERE id = ?1 AND lease = ?2",
                params![id.0 as i64, lease.0 as i64],
            )?)
        })
    }

    fn reschedule(
        &self,
        id: JobId,
        lease: LeaseId,
        due: Timestamp,
        count_attempt: bool,
    ) -> Result<(), StoreError> {
        self.write(|c| {
            let (id, lease) = (id.0 as i64, lease.0 as i64);
            let (tenant, dedup): ([u8; 16], Option<String>) = c
                .query_row(
                    "SELECT tenant, dedup FROM jobs WHERE id = ?1 AND lease = ?2",
                    params![id, lease],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?
                .ok_or(StoreError::LeaseLost)?;
            // A job enqueued with the same key while this one ran is merged into
            // it, as if enqueued after the reschedule (the earlier `due` wins).
            let mut due = us(due);
            if let Some(key) = dedup {
                let other: Option<i64> = c
                    .query_row(
                        "DELETE FROM jobs WHERE tenant = ?1 AND dedup = ?2 AND lease IS NULL
                         RETURNING due",
                        params![tenant, key],
                        |r| r.get(0),
                    )
                    .optional()?;
                due = other.map_or(due, |o| o.min(due));
            }
            c.execute(
                "UPDATE jobs SET lease = NULL, lease_until = NULL, due = ?2, attempts = attempts + ?3
                 WHERE id = ?1",
                params![id, due, i64::from(count_attempt)],
            )?;
            Ok(())
        })
    }

    fn put_channel(&self, ch: &ChannelRecord) -> Result<(), StoreError> {
        self.write(|c| {
            let s = ch.open();
            c.execute(
                "INSERT OR REPLACE INTO channels(id, tenant, account, collection, remote, resource,
                   token, expiration)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    ch.id().0,
                    ch.tenant.to_bytes(),
                    ch.account.to_bytes(),
                    ch.collection.to_bytes(),
                    s.map(|s| &s.collection.0),
                    s.map(|s| &s.resource),
                    s.map(|s| s.token.expose_secret()),
                    s.map(|s| us(s.expiration))
                ],
            )?;
            Ok(())
        })
    }

    fn find_channel(&self, id: &ChannelId) -> Result<Option<ChannelRecord>, StoreError> {
        self.read(|c| Ok(collect(c, &format!("{CHANNEL} WHERE id = ?1"), [&id.0], channel)?.pop()))
    }

    fn channels(
        &self,
        tenant: TenantId,
        account: Option<AccountId>,
    ) -> Result<Vec<ChannelRecord>, StoreError> {
        self.read(|c| {
            collect(
                c,
                &format!(
                    "{CHANNEL} WHERE tenant = ?1 AND (?2 IS NULL OR account = ?2) ORDER BY id"
                ),
                params![tenant.to_bytes(), account.map(|a| a.to_bytes())],
                channel,
            )
        })
    }

    fn delete_channel(&self, id: &ChannelId) -> Result<(), StoreError> {
        self.write(|c| {
            c.execute("DELETE FROM channels WHERE id = ?1", [&id.0])?;
            Ok(())
        })
    }

    fn channels_expiring(&self, before: Timestamp) -> Result<Vec<ChannelRecord>, StoreError> {
        self.read(|c| {
            collect(
                c,
                &format!("{CHANNEL} WHERE expiration < ?1 ORDER BY expiration, id"),
                [us(before)],
                channel,
            )
        })
    }
}

/// No row changed: the job is gone or leased again under another lease.
fn held(n: usize) -> Result<()> {
    if n == 0 {
        Err(StoreError::LeaseLost.into())
    } else {
        Ok(())
    }
}
