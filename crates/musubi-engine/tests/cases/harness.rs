use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use jiff::tz::TimeZone;
use jiff::{SignedDuration, Timestamp};
use musubi_engine::{CommitSink, Codecs, Config, DecodeFn, Engine, Providers};
use musubi_model::capability::{
    DeltaSupport, KindCapabilities, Limits, ProviderId, PushSupport, RecurrenceSupport, ZoneSupport,
};
use musubi_model::control::{
    AccountRecord, AccountState, AccountTokens, ChannelRecord, Credentials, JobKind, JobSpec, Lane,
};
use musubi_model::journal::{ChangeOp, ChangeTarget};
use musubi_model::provider::{Access, ChannelId, RemoteCollection};
use musubi_model::store::{CollectionState, Coverage, RangePage, RangeQuery};
use musubi_model::{
    AccountId, CalendarEvent, Change, Codec, CollectionId, CollectionRef, ControlStore, Decoded,
    EntityId, EntityKind, Event, Raw, ReauthReason, RemoteKey, Seq, Storage, StoreError, SyncError,
    TenantId, TenantStore, TimeWindow, TokenVault, ProviderDescriptor,
};
use musubi_store_sqlite::{SqliteControl, SqliteStorage};
use musubi_testkit::{
    CanonicalCodec, FakeObject, FakeProvider, ManualClock, MemoryVault, Op, event_object,
};
use secrecy::SecretString;
use serde_json::json;

const TENANT: TenantId = TenantId::from_u128(0x7e);
const ACCOUNT: AccountId = AccountId::from_u128(0xacc);

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let p = std::env::temp_dir().join(format!(
            "musubi-engine-{}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed),
            Timestamp::now().as_nanosecond()
        ));
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

type TestEngine = Engine<SqliteStorage>;

/// The provider the tests run against. Every case runs twice: once with the
/// fake's default descriptor (`SECOND == false`) and once as another provider
/// with a different id and descriptor, so the engine never branches on the id.
fn provider() -> (ProviderId, FakeProvider) {
    if !SECOND {
        return (ProviderId("fake".into()), FakeProvider::new());
    }
    let id = ProviderId("other".into());
    let events = KindCapabilities::new(
        EntityKind::Event,
        DeltaSupport::SyncToken,
        PushSupport::Webhook,
    );
    let tasks = KindCapabilities::new(EntityKind::Task, DeltaSupport::CtagEtag, PushSupport::None);
    let mut limits = Limits::new(2);
    limits.max_batch = Some(2);
    let mut d = ProviderDescriptor::new(id.clone(), vec![events, tasks], limits);
    d.recurrence = RecurrenceSupport::Full;
    d.zones = vec![ZoneSupport::Iana, ZoneSupport::CustomVtimezone];
    (id, FakeProvider::new().with_descriptor(d))
}

struct H {
    storage: Arc<SqliteStorage>,
    control: Arc<SqliteControl>,
    vault: Arc<MemoryVault>,
    clock: ManualClock,
    fake: FakeProvider,
    provider: ProviderId,
    commits: Arc<Mutex<Vec<Seq>>>,
    config: Config,
    engine: TestEngine,
    _dir: TempDir,
}

fn cal(name: &str) -> CollectionRef {
    CollectionRef(name.into())
}

fn meta(name: &str) -> RemoteCollection {
    let mut c = RemoteCollection::new(cal(name), EntityKind::Event, name, Access::Owner);
    c.primary = name == "cal1";
    c
}

impl H {
    fn new() -> Self {
        Self::with(|_| {})
    }

    fn with(tune: impl FnOnce(&mut Config)) -> Self {
        Self::build(tune, |f| f)
    }

    /// With a tuned config and fake provider.
    fn build(
        tune: impl FnOnce(&mut Config),
        tune_fake: impl FnOnce(FakeProvider) -> FakeProvider,
    ) -> Self {
        let dir = TempDir::new();
        let storage = Arc::new(SqliteStorage::new(&dir.0.join("tenants"), 4).unwrap());
        storage.create_tenant(TENANT).unwrap();
        let control = Arc::new(SqliteControl::open(&dir.0.join("control.db")).unwrap());
        control.insert_tenant(TENANT).unwrap();
        control
            .upsert_account(&AccountRecord {
                id: ACCOUNT,
                tenant: TENANT,
                provider: provider().0,
                subject: "user@example.test".into(),
                state: AccountState::Connecting,
            })
            .unwrap();
        // The dedup of the job queue compares leases with the wall clock: start there.
        let clock = ManualClock::new(Timestamp::now());
        let vault = Arc::new(MemoryVault::default());
        vault
            .store(
                ACCOUNT,
                &Credentials::OAuth(AccountTokens {
                    refresh: SecretString::from("refresh"),
                    access: SecretString::from("access"),
                    access_until: Timestamp::now() + SignedDuration::from_hours(24 * 365 * 10),
                }),
            )
            .unwrap();
        let (provider, fake) = provider();
        let fake = tune_fake(fake);
        fake.add_collection(meta("cal1"));
        let mut config = Config {
            guard_threshold: 0,
            callback: Some("https://musubi.example.test/push".parse().unwrap()),
            ..Config::default()
        };
        tune(&mut config);
        let commits = Arc::new(Mutex::new(Vec::new()));
        let mut h = Self {
            engine: engine_for(
                &storage,
                &control,
                vault.clone(),
                &clock,
                &fake,
                Codecs::default(),
                &commits,
                &config,
            ),
            storage,
            control,
            vault,
            clock,
            fake,
            provider,
            commits,
            config,
            _dir: dir,
        };
        h.engine = h.restart();
        h
    }

    /// A new engine over the same stores, as after a process restart.
    fn restart(&self) -> TestEngine {
        self.engine_with::<CanonicalCodec>(self.vault.clone())
    }

    /// A new engine over the same stores with another event codec or vault.
    fn engine_with<C: Codec<Event> + 'static>(&self, vault: Arc<dyn TokenVault>) -> TestEngine {
        self.engine_codecs(
            vault,
            Codecs {
                event: Some(DecodeFn::of::<C>()),
                ..Codecs::default()
            },
        )
    }

    /// A new engine over the same stores with these codecs.
    fn engine_codecs(&self, vault: Arc<dyn TokenVault>, codecs: Codecs) -> TestEngine {
        engine_for(
            &self.storage,
            &self.control,
            vault,
            &self.clock,
            &self.fake,
            codecs,
            &self.commits,
            &self.config,
        )
    }

    fn put(&self, col: &str, key: &str, validator: &str, ev: &CalendarEvent, master: Option<&str>) {
        let start = ev.start.to_zoned(TimeZone::UTC).unwrap().timestamp();
        self.fake.put(
            &cal(col),
            FakeObject {
                object: event_object(key, validator, ev, master),
                span: Some((start, start + SignedDuration::from_hours(1))),
            },
        );
    }

    /// A one-hour event `days` from now.
    fn put_at(&self, key: &str, days: i64) {
        let start = self.clock_now() + SignedDuration::from_hours(24 * days);
        self.put("cal1", key, "v1", &single(key, start), None);
    }

    fn clock_now(&self) -> Timestamp {
        musubi_model::provider::Clock::now(&self.clock)
    }

    fn advance(&self, d: Duration) {
        self.clock.advance(SignedDuration::try_from(d).unwrap());
    }

    async fn connect_and_sync(&self) {
        self.engine.connect(ACCOUNT).await.unwrap();
        self.engine.run_until_idle().await.unwrap();
    }

    fn store(&self) -> musubi_store_sqlite::SqliteTenant {
        self.storage.open(TENANT).unwrap()
    }

    fn collection(&self, name: &str) -> Option<CollectionState> {
        self.store()
            .collections()
            .unwrap()
            .into_iter()
            .find(|c| c.remote.remote == cal(name))
    }

    fn col(&self, name: &str) -> CollectionId {
        self.collection(name).unwrap().id
    }

    /// Live keys among `keys` in collection `name`.
    fn live(&self, name: &str, keys: &[&str]) -> Vec<String> {
        let keys: Vec<RemoteKey> = keys.iter().map(|k| RemoteKey((*k).into())).collect();
        let mut live: Vec<String> = self
            .store()
            .items_by_remote(self.col(name), &keys)
            .unwrap()
            .into_iter()
            .filter(|i| !i.tombstone)
            .map(|i| i.key.0)
            .collect();
        live.sort();
        live
    }

    fn entity_of(&self, name: &str, key: &str) -> EntityId {
        self.store()
            .items_by_remote(self.col(name), &[RemoteKey(key.into())])
            .unwrap()[0]
            .entity
    }

    fn journal(&self) -> Vec<Change> {
        self.store().changes(Seq(0), 10_000).unwrap().changes
    }

    fn entity_changes(&self, op: ChangeOp) -> Vec<EntityId> {
        self.journal()
            .into_iter()
            .filter(|c| c.op == op)
            .filter_map(|c| match c.target {
                ChangeTarget::Entity(e) => Some(e),
                _ => None,
            })
            .collect()
    }

    /// Queued jobs of `lane` (leased with a zero lease, then released).
    fn jobs(&self, lane: Lane) -> Vec<JobSpec> {
        let far = self.clock_now() + SignedDuration::from_hours(24 * 365 * 100);
        let jobs = self
            .control
            .lease_due(lane, far, 1000, Duration::ZERO)
            .unwrap();
        self.control.release_leases().unwrap();
        jobs.into_iter().map(|j| j.spec).collect()
    }

    /// Due time of the queued sync job of collection `name`.
    fn sync_due(&self, name: &str) -> Option<Timestamp> {
        let id = self.col(name);
        self.jobs(Lane::Sync)
            .into_iter()
            .find(|j| matches!(j.kind, JobKind::Sync { collection, .. } if collection == id))
            .map(|j| j.due)
    }

    /// The stored item of `key` in collection `name`.
    fn item(&self, name: &str, key: &str) -> musubi_model::store::ItemView {
        let store = self.store();
        let id = store
            .items_by_remote(self.col(name), &[RemoteKey(key.into())])
            .unwrap()[0]
            .item;
        store.item(id, Default::default()).unwrap().unwrap()
    }

    fn range(&self, start: Timestamp, end: Timestamp) -> RangePage {
        self.store()
            .range(&RangeQuery {
                window: TimeWindow::new(start, end).unwrap(),
                collections: None,
                zone: TimeZone::UTC,
                after: None,
                limit: 1000,
                ids_only: false,
            })
            .unwrap()
    }
}

#[allow(clippy::too_many_arguments)]
fn engine_for(
    storage: &Arc<SqliteStorage>,
    control: &Arc<SqliteControl>,
    vault: Arc<dyn TokenVault>,
    clock: &ManualClock,
    fake: &FakeProvider,
    codecs: Codecs,
    commits: &Arc<Mutex<Vec<Seq>>>,
    config: &Config,
) -> TestEngine {
    let commits = commits.clone();
    let sink: CommitSink = Arc::new(move |tenant, head| {
        assert_eq!(tenant, TENANT);
        commits.lock().unwrap().push(head);
    });
    Engine::new(
        storage.clone(),
        Providers::new().with(Arc::new(fake.clone()), codecs),
        control.clone(),
        vault,
        Arc::new(clock.clone()),
        sink,
        config.clone(),
    )
}

fn ev(v: serde_json::Value) -> CalendarEvent {
    serde_json::from_value(v).unwrap()
}

fn single(uid: &str, start: Timestamp) -> CalendarEvent {
    let local = start.to_zoned(TimeZone::UTC).datetime();
    ev(json!({
        "uid": uid, "start": local.to_string(), "timeZone": "Etc/UTC", "duration": "PT1H"
    }))
}

fn ts(s: &str) -> Timestamp {
    s.parse().unwrap()
}

/// Waits until `f` holds, failing after five seconds.
async fn until(mut f: impl FnMut() -> bool) {
    for _ in 0..500 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("condition not reached");
}

