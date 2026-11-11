//! `ControlStore` behaviour of `SqliteControl`.

mod common;

use std::time::Duration;

use common::{TempDir, t};
use musubi_model::capability::ProviderId;
use musubi_model::control::{
    AccountRecord, AccountState, ApiKeyHash, ApiKeyRecord, ChannelRecord, JobKind, JobSpec, Lane,
    OAuthPending, OAuthStateHash, Scope, SealedTokens,
};
use musubi_model::provider::{ChannelId, Subscription};
use musubi_model::{AccountId, CollectionId, CollectionRef, ControlStore, StoreError, TenantId};
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
        sub: Subscription {
            channel: ChannelId(id.into()),
            collection: CollectionRef("primary".into()),
            resource: "res".into(),
            token: SecretString::from("tok"),
            expiration: t(expires),
        },
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

    let leased = c.lease_due(Lane::Sync, t(5), 10, LEASE).unwrap();
    assert_eq!(leased.len(), 2);
    let mine = leased.iter().find(|j| j.id == a).unwrap();
    assert_eq!(mine.spec.due, t(5), "due became the earlier one");
    // A leased job no longer absorbs new ones.
    assert_ne!(c.enqueue(&job(sync, 0, 5, Some("sync:5"))).unwrap(), a);
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
    assert_eq!(
        (ch.sub.expiration, ch.sub.token.expose_secret()),
        (t(20), "tok")
    );
    assert_eq!(
        (ch.account, ch.collection, ch.sub.resource.as_str()),
        (A1, CollectionId::from_u128(5), "res")
    );
    assert_eq!(c.channels(T1, Some(A2)).unwrap().len(), 1);
    assert_eq!(c.channels(T1, None).unwrap().len(), 2);
    let exp: Vec<_> = c
        .channels_expiring(t(30))
        .unwrap()
        .into_iter()
        .map(|c| c.sub.channel.0)
        .collect();
    assert_eq!(exp, ["ch1"]);
    c.delete_channel(&ChannelId("ch1".into())).unwrap();
    c.delete_channel(&ChannelId("ch1".into())).unwrap();
    assert_eq!(c.channels(T1, None).unwrap().len(), 1);
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
