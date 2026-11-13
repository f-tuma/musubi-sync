//! `ControlStore` behaviour of `SqliteControl`.

mod common;

use std::time::Duration;

use common::{TempDir, t};
use musubi_model::capability::ProviderId;
use musubi_model::control::{
    AccountRecord, AccountState, ApiKeyHash, ApiKeyRecord, ChannelRecord, ChannelState, JobKind,
    JobSpec, Lane, OAuthPending, OAuthStateHash, Scope, SealedTokens,
};
use musubi_model::provider::{ChannelId, Endpoint, Subscription};
use musubi_model::{
    AccountId, CollectionId, CollectionRef, ControlStore, EntityKind, StoreError, TenantId,
};
use musubi_store_sqlite::SqliteControl;
use secrecy::{ExposeSecret, SecretString};

const T1: TenantId = TenantId::from_u128(1);
const T2: TenantId = TenantId::from_u128(2);
const A1: AccountId = AccountId::from_u128(10);
const A2: AccountId = AccountId::from_u128(20);

fn open() -> (TempDir, SqliteControl) {
    let d = TempDir::new();
    let c = SqliteControl::open(&d.path().join("control.db")).unwrap();
    c.insert_tenant(T1).unwrap();
    (d, c)
}

fn account(id: AccountId, subject: &str) -> AccountRecord {
    AccountRecord {
        id,
        tenant: T1,
        provider: ProviderId("google".into()),
        subject: subject.into(),
        state: AccountState::Connecting,
    }
}

fn job(kind: JobKind, priority: i32, due: i64, dedup: Option<&str>) -> JobSpec {
    JobSpec {
        tenant: T1,
        lane: Lane::Sync,
        kind,
        priority,
        due: t(due),
        dedup: dedup.map(Into::into),
    }
}

fn channel(id: &str, account: AccountId, expires: i64) -> ChannelRecord {
    ChannelRecord {
        tenant: T1,
        account,
        collection: CollectionId::from_u128(5),
        state: ChannelState::Open(Subscription::new(
            ChannelId(id.into()),
            CollectionRef("primary".into()),
            "res",
            SecretString::from("tok"),
            t(expires),
        )),
    }
}

fn pending(id: &str, account: AccountId) -> ChannelRecord {
    ChannelRecord {
        state: ChannelState::Pending(ChannelId(id.into())),
        ..channel(id, account, 0)
    }
}

const LEASE: Duration = Duration::from_secs(60);

#[test]
fn tenants_and_api_keys() {
    let (_d, c) = open();
    assert!(matches!(
        c.insert_tenant(T1),
        Err(StoreError::AlreadyExists)
    ));
    let key = ApiKeyRecord {
        hash: ApiKeyHash([7; 32]),
        tenant: T1,
        scope: Scope::Admin,
        created: t(0),
        revoked: None,
    };
    c.insert_api_key(&key).unwrap();
    assert!(matches!(
        c.insert_api_key(&key),
        Err(StoreError::AlreadyExists)
    ));
    assert_eq!(c.find_api_key(&key.hash).unwrap(), Some(key.clone()));
    c.revoke_api_key(&key.hash, t(1)).unwrap();
    c.revoke_api_key(&key.hash, t(2)).unwrap(); // idempotent
    assert_eq!(c.find_api_key(&key.hash).unwrap(), None);
    assert!(matches!(
        c.revoke_api_key(&ApiKeyHash([8; 32]), t(1)),
        Err(StoreError::NotFound)
    ));
}

#[test]
fn accounts_and_tokens() {
    let (_d, c) = open();
    c.upsert_account(&account(A1, "sub-1")).unwrap();
    assert!(matches!(
        c.upsert_account(&account(A2, "sub-1")),
        Err(StoreError::AlreadyExists)
    ));
    c.write_tokens(A1, &SealedTokens(vec![1, 2, 3])).unwrap();

    let mut reconnected = account(A1, "sub-1");
    reconnected.state = AccountState::Active;
    c.upsert_account(&reconnected).unwrap();
    assert_eq!(c.account(A1).unwrap(), Some(reconnected.clone()));
    assert_eq!(
        c.read_tokens(A1).unwrap(),
        Some(SealedTokens(vec![1, 2, 3])),
        "upsert keeps tokens"
    );
    let mut moved = reconnected.clone();
    moved.tenant = T2;
    c.insert_tenant(T2).unwrap();
    assert!(
        matches!(c.upsert_account(&moved), Err(StoreError::AlreadyExists)),
        "an account never changes tenant"
    );
    assert_eq!(c.account(A1).unwrap(), Some(reconnected.clone()));
    let google = ProviderId("google".into());
    assert_eq!(
        c.account_by_subject(T1, &google, "sub-1").unwrap(),
        Some(reconnected)
    );
    assert_eq!(c.account_by_subject(T2, &google, "sub-1").unwrap(), None);

    c.set_account_state(A1, AccountState::Paused).unwrap();
    assert_eq!(c.accounts(T1).unwrap()[0].state, AccountState::Paused);
    assert!(matches!(
        c.set_account_state(A2, AccountState::Paused),
        Err(StoreError::NotFound)
    ));
    assert!(matches!(
        c.write_tokens(A2, &SealedTokens(vec![])),
        Err(StoreError::NotFound)
    ));
    assert_eq!(c.read_tokens(A2).unwrap(), None);
}

#[test]
fn oauth_state_is_taken_once_and_expires() {
    let (_d, c) = open();
    let pending = |h: u8, expires| OAuthPending {
        state: OAuthStateHash([h; 32]),
        tenant: T1,
        provider: ProviderId("google".into()),
        account: Some(A1),
        verifier: SecretString::from("verifier"),
        expires,
    };
    let now = jiff::Timestamp::now();
    let later = now + jiff::SignedDuration::from_mins(10);
    c.put_oauth_state(&pending(1, later)).unwrap();
    assert!(matches!(
        c.put_oauth_state(&pending(1, later)),
        Err(StoreError::AlreadyExists)
    ));
    let got = c
        .take_oauth_state(&OAuthStateHash([1; 32]), now)
        .unwrap()
        .unwrap();
    assert_eq!(
        (got.account, got.verifier.expose_secret()),
        (Some(A1), "verifier")
    );
    assert!(
        c.take_oauth_state(&OAuthStateHash([1; 32]), now)
            .unwrap()
            .is_none()
    );

    c.put_oauth_state(&pending(2, later)).unwrap();
    assert!(
        c.take_oauth_state(&OAuthStateHash([2; 32]), later)
            .unwrap()
            .is_none()
    );
    // Expired states are deleted on the next put.
    c.put_oauth_state(&pending(3, t(0))).unwrap();
    c.put_oauth_state(&pending(4, later)).unwrap();
    assert!(
        c.take_oauth_state(&OAuthStateHash([3; 32]), t(-1))
            .unwrap()
            .is_none()
    );
}

#[test]
fn enqueue_coalesces_unleased_jobs_per_tenant() {
    let (_d, c) = open();
    c.insert_tenant(T2).unwrap();
    let sync = JobKind::Sync {
        account: A1,
        collection: CollectionId::from_u128(5),
    };
    let a = c
        .enqueue(&job(sync.clone(), 0, 10, Some("sync:5")))
        .unwrap();
    assert_eq!(
        c.enqueue(&job(sync.clone(), 0, 5, Some("sync:5"))).unwrap(),
        a
    );
    assert_eq!(
        c.enqueue(&job(sync.clone(), 0, 20, Some("sync:5")))
            .unwrap(),
        a
    );
    let mut other = job(sync.clone(), 0, 5, Some("sync:5"));
    other.tenant = T2;
    assert_ne!(
        c.enqueue(&other).unwrap(),
        a,
        "dedup is scoped to the tenant"
    );

    // Leased now (enqueue checks lease expiry against the wall clock).
    let leased = c
        .lease_due(Lane::Sync, jiff::Timestamp::now(), 10, LEASE)
        .unwrap();
    assert_eq!(leased.len(), 2);
    let mine = leased.iter().find(|j| j.id == a).unwrap();
    assert_eq!(mine.spec.due, t(5), "due became the earlier one");
    // A leased job no longer absorbs new ones.
    let b = c.enqueue(&job(sync.clone(), 0, 5, Some("sync:5"))).unwrap();
    assert_ne!(b, a);
    assert_eq!(
        c.enqueue(&job(sync, 0, 1, Some("sync:5"))).unwrap(),
        b,
        "absorbed by the unleased one"
    );
}

#[test]
fn expired_lease_absorbs_and_reschedule_merges_duplicates() {
    let (_d, c) = open();
    let sync = JobKind::Sync {
        account: A1,
        collection: CollectionId::from_u128(5),
    };
    let a = c.enqueue(&job(sync.clone(), 0, 0, Some("k"))).unwrap();
    // Leased long ago: the lease has expired by the wall clock.
    let old = c.lease_due(Lane::Sync, t(0), 10, LEASE).unwrap();
    assert_eq!(c.enqueue(&job(sync.clone(), 0, 3, Some("k"))).unwrap(), a);
    assert!(matches!(
        c.complete(a, old[0].lease),
        Err(StoreError::LeaseLost)
    ));

    // A job leased now, a duplicate enqueued meanwhile, then the leased one is
    // rescheduled: one unleased job with the earlier due remains.
    let now = jiff::Timestamp::now();
    let mine = c.lease_due(Lane::Sync, now, 10, LEASE).unwrap();
    assert_eq!(mine[0].id, a);
    let dup = c.enqueue(&job(sync, 0, 7, Some("k"))).unwrap();
    assert_ne!(dup, a);
    c.reschedule(a, mine[0].lease, t(9), false).unwrap();
    let left = c.lease_due(Lane::Sync, t(100), 10, LEASE).unwrap();
    assert_eq!(left.len(), 1, "{left:?}");
    assert_eq!((left[0].id, left[0].spec.due), (a, t(7)));
}

#[test]
fn release_leases_frees_jobs_and_merges_duplicates() {
    let (_d, c) = open();
    let sync = JobKind::Sync {
        account: A1,
        collection: CollectionId::from_u128(5),
    };
    let now = jiff::Timestamp::now();
    let a = c.enqueue(&job(sync.clone(), 0, 5, Some("k"))).unwrap();
    let plain = c.enqueue(&job(JobKind::Maintain, 0, 6, None)).unwrap();
    assert_eq!(c.lease_due(Lane::Sync, now, 10, LEASE).unwrap().len(), 2);
    // Enqueued while `a` is leased: a second row with the same key.
    let dup = c.enqueue(&job(sync, 0, 2, Some("k"))).unwrap();
    assert_ne!(dup, a);

    assert_eq!(c.release_leases().unwrap(), 2);
    let free = c.lease_due(Lane::Sync, now, 10, LEASE).unwrap();
    let mut got: Vec<_> = free.iter().map(|j| (j.id, j.spec.due)).collect();
    got.sort();
    assert_eq!(got, [(a, t(2)), (plain, t(6))]);
    assert_eq!(c.release_leases().unwrap(), 2);
    assert_eq!(c.release_leases().unwrap(), 0);
}

#[test]
fn lease_order_expiry_and_lease_lost() {
    let (_d, c) = open();
    let low = c.enqueue(&job(JobKind::Maintain, 0, 0, None)).unwrap();
    let high = c.enqueue(&job(JobKind::ExtendIndex, 5, 3, None)).unwrap();
    let later = c.enqueue(&job(JobKind::Maintain, 9, 100, None)).unwrap();
    let mut bg = job(JobKind::Maintain, 9, 0, None);
    bg.lane = Lane::Background;
    c.enqueue(&bg).unwrap();

    let jobs = c.lease_due(Lane::Sync, t(10), 10, LEASE).unwrap();
    assert_eq!(jobs.iter().map(|j| j.id).collect::<Vec<_>>(), [high, low]);
    assert!(!jobs.iter().any(|j| j.id == later));
    assert!(
        c.lease_due(Lane::Sync, t(10), 10, LEASE)
            .unwrap()
            .is_empty(),
        "leased"
    );

    let first = jobs[0].clone();
    let until = c
        .extend_lease(first.id, first.lease, t(10), Duration::from_secs(3600))
        .unwrap();
    assert_eq!(until, t(11));
    // The other lease expires and is taken over.
    let again = c
        .lease_due(
            Lane::Sync,
            t(10) + jiff::SignedDuration::from_secs(61),
            10,
            LEASE,
        )
        .unwrap();
    assert_eq!(again.len(), 1);
    assert_eq!(again[0].id, low);
    assert!(matches!(
        c.complete(low, jobs[1].lease),
        Err(StoreError::LeaseLost)
    ));
    assert!(matches!(
        c.reschedule(low, jobs[1].lease, t(0), true),
        Err(StoreError::LeaseLost)
    ));

    c.complete(first.id, first.lease).unwrap();
    c.reschedule(low, again[0].lease, t(20), true).unwrap();
    let retried = c.lease_due(Lane::Sync, t(20), 10, LEASE).unwrap();
    assert_eq!((retried[0].id, retried[0].attempts), (low, 1));
    c.complete(low, retried[0].lease).unwrap();
    assert!(matches!(
        c.complete(low, retried[0].lease),
        Err(StoreError::LeaseLost)
    ));
}

#[test]
fn delete_account_removes_its_jobs_and_channels() {
    let (_d, c) = open();
    c.upsert_account(&account(A1, "s1")).unwrap();
    c.upsert_account(&account(A2, "s2")).unwrap();
    c.put_channel(&channel("ch1", A1, 100)).unwrap();
    c.put_channel(&channel("ch2", A2, 100)).unwrap();
    let col = CollectionId::from_u128(5);
    for (kind, _) in [
        (JobKind::Discover { account: A1 }, ()),
        (
            JobKind::Sync {
                account: A1,
                collection: col,
            },
            (),
        ),
        (
            JobKind::RenewChannel {
                channel: ChannelId("ch1".into()),
            },
            (),
        ),
        (
            JobKind::Sync {
                account: A2,
                collection: col,
            },
            (),
        ),
        (JobKind::Maintain, ()),
        (JobKind::ExtendIndex, ()),
    ] {
        c.enqueue(&job(kind, 0, 0, None)).unwrap();
    }
    c.delete_account(A1).unwrap();
    assert!(matches!(c.delete_account(A1), Err(StoreError::NotFound)));
    let left: Vec<_> = c
        .lease_due(Lane::Sync, t(0), 10, LEASE)
        .unwrap()
        .into_iter()
        .map(|j| j.spec.kind)
        .collect();
    assert_eq!(
        left,
        [
            JobKind::Sync {
                account: A2,
                collection: col
            },
            JobKind::Maintain,
            JobKind::ExtendIndex
        ]
    );
    assert!(c.find_channel(&ChannelId("ch1".into())).unwrap().is_none());
    assert_eq!(c.channels(T1, None).unwrap().len(), 1);
}

#[test]
fn channels() {
    let (_d, c) = open();
    c.put_channel(&channel("ch1", A1, 10)).unwrap();
    c.put_channel(&channel("ch2", A2, 50)).unwrap();
    c.put_channel(&channel("ch1", A1, 20)).unwrap(); // renewal replaces
    let ch = c.find_channel(&ChannelId("ch1".into())).unwrap().unwrap();
    let sub = ch.open().unwrap();
    assert_eq!((sub.expiration, sub.token.expose_secret()), (t(20), "tok"));
    assert_eq!(
        (ch.account, ch.collection, sub.resource.as_str()),
        (A1, CollectionId::from_u128(5), "res")
    );
    assert_eq!(c.channels(T1, Some(A2)).unwrap().len(), 1);
    assert_eq!(c.channels(T1, None).unwrap().len(), 2);
    let exp: Vec<_> = c
        .channels_expiring(t(30))
        .unwrap()
        .into_iter()
        .map(|c| c.id().0.clone())
        .collect();
    assert_eq!(exp, ["ch1"]);
    c.delete_channel(&ChannelId("ch1".into())).unwrap();
    c.delete_channel(&ChannelId("ch1".into())).unwrap();
    assert_eq!(c.channels(T1, None).unwrap().len(), 1);
}

#[test]
fn pending_channels_are_listed_but_never_expire() {
    let (_d, c) = open();
    c.put_channel(&pending("p1", A1)).unwrap();
    let ch = c.find_channel(&ChannelId("p1".into())).unwrap().unwrap();
    assert!(matches!(&ch.state, ChannelState::Pending(id) if id.0 == "p1"));
    assert_eq!(c.channels(T1, Some(A1)).unwrap().len(), 1);
    assert!(
        c.channels_expiring(jiff::Timestamp::MAX)
            .unwrap()
            .is_empty()
    );
    // The reply arrived: the open channel replaces the pending row.
    c.put_channel(&channel("p1", A1, 10)).unwrap();
    let ch = c.find_channel(&ChannelId("p1".into())).unwrap().unwrap();
    assert_eq!(ch.open().unwrap().expiration, t(10));
    assert_eq!(c.channels_expiring(t(30)).unwrap().len(), 1);
}

#[test]
fn control_db_from_before_pending_channels_is_migrated() {
    let d = TempDir::new();
    let path = d.path().join("control.db");
    {
        let c = rusqlite::Connection::open(&path).unwrap();
        c.execute_batch(
            "CREATE TABLE channels(
               id TEXT PRIMARY KEY, tenant BLOB NOT NULL, account BLOB NOT NULL,
               collection BLOB NOT NULL, remote TEXT NOT NULL, resource TEXT NOT NULL,
               token TEXT NOT NULL, expiration INTEGER NOT NULL);
             CREATE INDEX channels_tenant ON channels(tenant, account);
             CREATE INDEX channels_expiration ON channels(expiration);",
        )
        .unwrap();
        c.execute(
            "INSERT INTO channels VALUES ('old', ?1, ?2, ?3, 'primary', 'res', 'tok', 20)",
            (
                T1.to_bytes(),
                A1.to_bytes(),
                CollectionId::from_u128(5).to_bytes(),
            ),
        )
        .unwrap();
    }
    for _ in 0..2 {
        let c = SqliteControl::open(&path).unwrap();
        let old = c.find_channel(&ChannelId("old".into())).unwrap().unwrap();
        assert_eq!(old.open().unwrap().resource, "res");
        c.put_channel(&pending("p1", A1)).unwrap();
        assert_eq!(c.channels(T1, Some(A1)).unwrap().len(), 2);
    }
}

#[test]
fn delete_tenant_cascades_and_survives_reopen() {
    let d = TempDir::new();
    let path = d.path().join("control.db");
    {
        let c = SqliteControl::open(&path).unwrap();
        for t_ in [T1, T2] {
            c.insert_tenant(t_).unwrap();
        }
        c.upsert_account(&account(A1, "s1")).unwrap();
        c.put_channel(&channel("ch1", A1, 10)).unwrap();
        c.enqueue(&job(JobKind::Maintain, 0, 0, None)).unwrap();
        let mut j2 = job(JobKind::Maintain, 0, 0, None);
        j2.tenant = T2;
        c.enqueue(&j2).unwrap();
    }
    let c = SqliteControl::open(&path).unwrap();
    assert_eq!(c.accounts(T1).unwrap().len(), 1, "durable across reopen");
    c.delete_tenant(T1).unwrap();
    assert!(matches!(c.delete_tenant(T1), Err(StoreError::NotFound)));
    assert!(c.accounts(T1).unwrap().is_empty());
    assert!(c.channels(T1, None).unwrap().is_empty());
    let left = c.lease_due(Lane::Sync, t(0), 10, LEASE).unwrap();
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].spec.tenant, T2);
    c.insert_tenant(T1).unwrap();
}

#[test]
fn renew_channel_job_needs_its_channel() {
    let (_d, c) = open();
    let renew = JobKind::RenewChannel {
        channel: ChannelId("gone".into()),
    };
    assert!(matches!(
        c.enqueue(&job(renew, 0, 0, None)),
        Err(StoreError::NotFound)
    ));
}

fn schema_version(path: &std::path::Path) -> u32 {
    rusqlite::Connection::open(path)
        .unwrap()
        .query_row("SELECT v FROM meta WHERE k = 'schema'", [], |r| r.get(0))
        .unwrap()
}

/// docs/plan/phase2.md 4.3: a phase-1 file (no `schema` key, `channels` from
/// before pending channels) is adopted as version 1 and migrated to 2 with its
/// rows; an empty file goes straight to 2.
#[test]
fn phase1_control_db_migrates_to_version_2() {
    let d = TempDir::new();
    let path = d.path().join("control.db");
    {
        let c = rusqlite::Connection::open(&path).unwrap();
        c.pragma_update(None, "journal_mode", "WAL").unwrap();
        c.execute_batch(
            "CREATE TABLE meta(k TEXT PRIMARY KEY, v INTEGER NOT NULL);
             INSERT INTO meta VALUES ('lease', 7);
             CREATE TABLE tenants(id BLOB PRIMARY KEY);
             CREATE TABLE accounts(
               id BLOB PRIMARY KEY, tenant BLOB NOT NULL, provider TEXT NOT NULL,
               subject TEXT NOT NULL, state TEXT NOT NULL, tokens BLOB,
               UNIQUE(tenant, provider, subject));
             CREATE TABLE channels(
               id TEXT PRIMARY KEY, tenant BLOB NOT NULL, account BLOB NOT NULL,
               collection BLOB NOT NULL, remote TEXT NOT NULL, resource TEXT NOT NULL,
               token TEXT NOT NULL, expiration INTEGER NOT NULL);
             CREATE INDEX channels_tenant ON channels(tenant, account);
             CREATE INDEX channels_expiration ON channels(expiration);",
        )
        .unwrap();
        c.execute("INSERT INTO tenants VALUES (?1)", [T1.to_bytes()])
            .unwrap();
        c.execute(
            "INSERT INTO accounts VALUES (?1, ?2, 'google', 'me', 'active', x'0102')",
            (A1.to_bytes(), T1.to_bytes()),
        )
        .unwrap();
        c.execute(
            "INSERT INTO channels VALUES ('old', ?1, ?2, ?3, 'primary', 'res', 'tok', 20)",
            (
                T1.to_bytes(),
                A1.to_bytes(),
                CollectionId::from_u128(5).to_bytes(),
            ),
        )
        .unwrap();
    }
    for _ in 0..2 {
        let c = SqliteControl::open(&path).unwrap();
        let a = c.account(A1).unwrap().unwrap();
        assert_eq!((a.subject.as_str(), a.state), ("me", AccountState::Active));
        assert_eq!(c.read_tokens(A1).unwrap().unwrap().0, vec![1, 2]);
        let old = c.find_channel(&ChannelId("old".into())).unwrap().unwrap();
        assert_eq!(old.open().unwrap().resource, "res");
        c.put_channel(&pending("p1", A1)).unwrap();
        assert_eq!(c.endpoint(A1).unwrap(), None);
        assert!(c.missing(A1).unwrap().is_empty());
        drop(c);
        assert_eq!(schema_version(&path), 2);
    }
    let lease: i64 = rusqlite::Connection::open(&path)
        .unwrap()
        .query_row("SELECT v FROM meta WHERE k = 'lease'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(lease, 7, "lease counter kept");

    let fresh = d.path().join("fresh.db");
    drop(SqliteControl::open(&fresh).unwrap());
    assert_eq!(schema_version(&fresh), 2);
}

#[test]
fn newer_control_db_is_refused_untouched() {
    let d = TempDir::new();
    let path = d.path().join("control.db");
    drop(SqliteControl::open(&path).unwrap());
    {
        let c = rusqlite::Connection::open(&path).unwrap();
        c.execute("UPDATE meta SET v = 3 WHERE k = 'schema'", [])
            .unwrap();
        c.pragma_update(None, "journal_mode", "DELETE").unwrap();
    }
    let before = std::fs::read(&path).unwrap();
    assert!(matches!(
        SqliteControl::open(&path),
        Err(StoreError::SchemaTooNew {
            found: 3,
            supported: 2
        })
    ));
    assert_eq!(std::fs::read(&path).unwrap(), before, "file untouched");
    assert!(!path.with_extension("db-wal").exists());
}

#[test]
fn endpoint_and_missing_round_trip() {
    let (_d, c) = open();
    c.upsert_account(&account(A1, "s1")).unwrap();
    assert_eq!(c.endpoint(A1).unwrap(), None);
    let e = Endpoint::new(
        "https://dav.example/",
        serde_json::json!({"principal": "/p/1/", "homes": ["/c/1/"]}),
    );
    c.set_endpoint(A1, &e).unwrap();
    assert_eq!(c.endpoint(A1).unwrap(), Some(e));
    c.set_missing(A1, &[EntityKind::Task, EntityKind::Contact])
        .unwrap();
    assert_eq!(
        c.missing(A1).unwrap(),
        vec![EntityKind::Task, EntityKind::Contact]
    );
    // Upserting the account (a reconnect) keeps both.
    c.upsert_account(&account(A1, "s1")).unwrap();
    assert!(c.endpoint(A1).unwrap().is_some());
    assert_eq!(c.missing(A1).unwrap().len(), 2);
    c.set_missing(A1, &[]).unwrap();
    assert!(c.missing(A1).unwrap().is_empty());
    for r in [
        c.set_endpoint(A2, &Endpoint::new("x", serde_json::Value::Null)),
        c.set_missing(A2, &[]),
        c.endpoint(A2).map(drop),
        c.missing(A2).map(drop),
    ] {
        assert!(matches!(r, Err(StoreError::NotFound)));
    }
}
