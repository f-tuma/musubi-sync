//! Acceptance tests of the read path (plan ch. 5, K5): the engine against the
//! SQLite store on a temporary directory and the scripted `FakeProvider`.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use jiff::tz::TimeZone;
use jiff::{SignedDuration, Timestamp};
use musubi_engine::{CommitSink, Config, Engine};
use musubi_model::capability::ProviderId;
use musubi_model::control::{AccountRecord, AccountState, AccountTokens, JobKind, Lane};
use musubi_model::journal::{ChangeOp, ChangeTarget};
use musubi_model::provider::{Access, RemoteCollection};
use musubi_model::store::{CollectionState, Coverage, RangePage, RangeQuery};
use musubi_model::{
    AccountId, CalendarEvent, Change, Codec, CollectionId, CollectionRef, ControlStore, Decoded,
    EntityId, EntityKind, Event, Raw, RemoteKey, Seq, Storage, StoreError, SyncError, TenantId,
    TenantStore, TimeWindow, TokenVault,
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

type TestEngine = Engine<SqliteStorage, CanonicalCodec>;

struct H {
    storage: Arc<SqliteStorage>,
    control: Arc<SqliteControl>,
    vault: Arc<MemoryVault>,
    clock: ManualClock,
    fake: FakeProvider,
    commits: Arc<Mutex<Vec<Seq>>>,
    config: Config,
    engine: TestEngine,
    _dir: TempDir,
}

fn cal(name: &str) -> CollectionRef {
    CollectionRef(name.into())
}

fn meta(name: &str) -> RemoteCollection {
    RemoteCollection {
        remote: cal(name),
        kind: EntityKind::Event,
        name: name.into(),
        time_zone: None,
        access: Access::Owner,
        primary: name == "cal1",
    }
}

impl H {
    fn new() -> Self {
        Self::with(|_| {})
    }

    fn with(tune: impl FnOnce(&mut Config)) -> Self {
        let dir = TempDir::new();
        let storage = Arc::new(SqliteStorage::new(&dir.0.join("tenants"), 4).unwrap());
        storage.create_tenant(TENANT).unwrap();
        let control = Arc::new(SqliteControl::open(&dir.0.join("control.db")).unwrap());
        control.insert_tenant(TENANT).unwrap();
        control
            .upsert_account(&AccountRecord {
                id: ACCOUNT,
                tenant: TENANT,
                provider: ProviderId("fake".into()),
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
                &AccountTokens {
                    refresh: SecretString::from("refresh"),
                    access: SecretString::from("access"),
                    access_until: Timestamp::now() + SignedDuration::from_hours(24 * 365 * 10),
                },
            )
            .unwrap();
        let fake = FakeProvider::new();
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
                &commits,
                &config,
            ),
            storage,
            control,
            vault,
            clock,
            fake,
            commits,
            config,
            _dir: dir,
        };
        h.engine = h.restart();
        h
    }

    /// A new engine over the same stores, as after a process restart.
    fn restart(&self) -> TestEngine {
        self.engine_with(self.vault.clone())
    }

    /// A new engine over the same stores with another codec or vault.
    fn engine_with<C: Codec<Event> + 'static>(
        &self,
        vault: Arc<dyn TokenVault>,
    ) -> Engine<SqliteStorage, C> {
        engine_for(
            &self.storage,
            &self.control,
            vault,
            &self.clock,
            &self.fake,
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

fn engine_for<C: Codec<Event> + 'static>(
    storage: &Arc<SqliteStorage>,
    control: &Arc<SqliteControl>,
    vault: Arc<dyn TokenVault>,
    clock: &ManualClock,
    fake: &FakeProvider,
    commits: &Arc<Mutex<Vec<Seq>>>,
    config: &Config,
) -> Engine<SqliteStorage, C> {
    let commits = commits.clone();
    let sink: CommitSink = Arc::new(move |tenant, head| {
        assert_eq!(tenant, TENANT);
        commits.lock().unwrap().push(head);
    });
    Engine::new(
        storage.clone(),
        Arc::new(fake.clone()),
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

#[tokio::test]
async fn connect_commits_the_window_before_the_full_read() {
    let h = H::new();
    h.put_at("a-today", 1);
    h.put_at("b-old", -200);
    h.put_at("c-soon", 20);
    h.fake.hold_full_reads();

    h.engine.connect(ACCOUNT).await.unwrap();
    let engine = h.engine.clone();
    let run = tokio::spawn(async move { engine.run_until_idle().await });
    until(|| h.fake.calls().full_reads == 1).await;

    // The full read is held; the window around today is already committed.
    let all = ["a-today", "b-old", "c-soon"];
    assert_eq!(h.live("cal1", &all), ["a-today", "c-soon"]);
    assert!(matches!(
        h.collection("cal1").unwrap().status.coverage,
        Coverage::Window(_)
    ));
    assert_eq!(h.entity_changes(ChangeOp::Created).len(), 2);
    let head = h.store().head().unwrap();
    assert_eq!(
        h.commits.lock().unwrap().last(),
        Some(&head),
        "commit notified"
    );
    assert_eq!(
        h.control.account(ACCOUNT).unwrap().unwrap().state,
        AccountState::Active
    );

    h.fake.release_full_reads();
    run.await.unwrap().unwrap();
    assert_eq!(h.live("cal1", &all), all);
    let col = h.collection("cal1").unwrap();
    assert_eq!(col.status.coverage, Coverage::Full);
    assert!(col.cursor.is_some() && col.active_read.is_none());
    assert_eq!(h.entity_changes(ChangeOp::Created).len(), 3);
    let calls = h.fake.calls();
    assert_eq!((calls.window_reads, calls.full_reads), (1, 1));
    // Every commit was notified, in order, ending at the head.
    let commits = h.commits.lock().unwrap().clone();
    assert!(commits.windows(2).all(|w| w[0] < w[1]));
    assert_eq!(commits.last(), Some(&h.store().head().unwrap()));
}

#[tokio::test]
async fn unchanged_validator_writes_nothing() {
    let h = H::new();
    h.put_at("a", 1);
    h.connect_and_sync().await;
    let deltas = h.fake.calls().deltas;
    let entity = h.entity_of("cal1", "a");

    // The same object again (same validator) shows up in the next delta.
    let start = h.clock_now() + SignedDuration::from_hours(24);
    h.put("cal1", "a", "v1", &single("a", start), None);
    h.advance(h.config.poll_every_push + Duration::from_secs(1));
    h.engine.run_until_idle().await.unwrap();
    assert_eq!(h.fake.calls().deltas, deltas + 1);
    assert_eq!(h.entity_changes(ChangeOp::Updated), [], "no entity write");

    // A new validator does write.
    h.put("cal1", "a", "v2", &single("a", start), None);
    h.advance(h.config.poll_every_push + Duration::from_secs(1));
    h.engine.run_until_idle().await.unwrap();
    assert_eq!(h.entity_changes(ChangeOp::Updated), [entity]);
}

#[tokio::test]
async fn absence_deletes_only_after_a_completed_full_read() {
    let h = H::new();
    for k in ["a", "b", "c"] {
        h.put_at(k, 1);
    }
    h.connect_and_sync().await;
    let b = h.entity_of("cal1", "b");

    // `b` disappears while the delta cursor is expired: only a full read can tell.
    h.fake.delete(&cal("cal1"), &RemoteKey("b".into()));
    h.fake.expire_cursors();
    h.fake
        .fail(Op::Fetch, SyncError::Transient("network down".into()));
    h.advance(h.config.poll_every_push + Duration::from_secs(1));
    h.engine.run_until_idle().await.unwrap();

    // The full read completed, but its verification failed: nothing deleted yet.
    let col = h.collection("cal1").unwrap();
    assert!(col.active_read.is_some() && col.read_complete);
    assert_eq!(h.live("cal1", &["a", "b", "c"]), ["a", "b", "c"]);
    assert!(col.status.error.is_some());

    h.advance(Duration::from_secs(120));
    h.engine.run_until_idle().await.unwrap();
    assert_eq!(h.live("cal1", &["a", "b", "c"]), ["a", "c"]);
    assert_eq!(h.entity_changes(ChangeOp::Deleted), [b]);
    let col = h.collection("cal1").unwrap();
    assert!(col.active_read.is_none() && col.status.error.is_none());
}

#[tokio::test]
async fn deletion_guard_keeps_items_a_second_read_finds() {
    let h = H::new();
    for k in ["a", "b", "c"] {
        h.put_at(k, 1);
    }
    h.connect_and_sync().await;
    let fetches = h.fake.calls().fetch;

    // A full read misses `c` although it still exists.
    h.fake.hide_from_reads(&cal("cal1"), &RemoteKey("c".into()));
    h.fake.expire_cursors();
    h.advance(h.config.poll_every_push + Duration::from_secs(1));
    h.engine.run_until_idle().await.unwrap();

    assert_eq!(h.fake.calls().full_reads, 2);
    assert_eq!(h.fake.calls().fetch, fetches + 1, "missing items verified");
    assert_eq!(h.live("cal1", &["a", "b", "c"]), ["a", "b", "c"]);
    assert_eq!(h.entity_changes(ChangeOp::Deleted), []);
    assert!(h.collection("cal1").unwrap().active_read.is_none());
}

#[tokio::test]
async fn expired_cursor_reads_everything_again_without_duplicates() {
    let h = H::new();
    for k in ["a", "b", "c"] {
        h.put_at(k, 1);
    }
    h.connect_and_sync().await;
    h.fake.expire_cursors();
    h.put_at("d", 2);
    h.advance(h.config.poll_every_push + Duration::from_secs(1));
    h.engine.run_until_idle().await.unwrap();

    assert_eq!(h.fake.calls().full_reads, 2);
    let keys = ["a", "b", "c", "d"];
    assert_eq!(h.live("cal1", &keys), keys);
    let mut created = h.entity_changes(ChangeOp::Created);
    created.sort();
    created.dedup();
    assert_eq!(created.len(), 4, "one entity per item");
    assert_eq!(h.entity_changes(ChangeOp::Deleted), []);
    // The new delta cursor works: the next poll is a delta again.
    let deltas = h.fake.calls().deltas;
    h.advance(h.config.poll_every_push + Duration::from_secs(1));
    h.engine.run_until_idle().await.unwrap();
    assert_eq!(h.fake.calls().deltas, deltas + 1);
    assert_eq!(h.fake.calls().full_reads, 2);
}

#[tokio::test]
async fn ten_notifications_within_150_ms_cost_one_delta() {
    let h = H::new();
    h.put_at("a", 1);
    h.connect_and_sync().await;
    let channel = h.control.channels(TENANT, Some(ACCOUNT)).unwrap().remove(0);
    assert_eq!(channel.collection, h.col("cal1"));
    let deltas = h.fake.calls().deltas;

    for _ in 0..10 {
        h.engine.on_push(&channel).await.unwrap();
        assert_eq!(
            h.engine.run_until_idle().await.unwrap(),
            0,
            "still coalescing"
        );
        h.advance(Duration::from_millis(10));
    }
    h.advance(Duration::from_millis(60));
    assert_eq!(h.engine.run_until_idle().await.unwrap(), 1);
    assert_eq!(h.fake.calls().deltas, deltas + 1);
}

#[tokio::test]
async fn leased_job_runs_again_after_restart_and_never_twice_at_once() {
    let h = H::new();
    for k in ["a", "b", "c"] {
        h.put_at(k, 1);
    }
    h.fake.hold_full_reads();
    h.engine.connect(ACCOUNT).await.unwrap();
    let engine = h.engine.clone();
    let run = tokio::spawn(async move { engine.run_until_idle().await });
    until(|| h.fake.calls().full_reads == 1).await;
    // The process dies in the middle of the full read job.
    run.abort();
    let _ = run.await;
    h.fake.release_full_reads();

    let engine = h.restart();
    assert_eq!(
        engine.run_until_idle().await.unwrap(),
        0,
        "the job is still leased by the dead process"
    );
    h.advance(h.config.lease_for + Duration::from_secs(1));
    assert!(engine.run_until_idle().await.unwrap() >= 1);

    assert_eq!(h.fake.calls().full_reads, 2);
    assert_eq!(h.live("cal1", &["a", "b", "c"]), ["a", "b", "c"]);
    let col = h.collection("cal1").unwrap();
    assert_eq!(col.status.coverage, Coverage::Full);
    let mut created = h.entity_changes(ChangeOp::Created);
    let n = created.len();
    created.sort();
    created.dedup();
    assert_eq!((n, created.len()), (3, 3));
}

#[tokio::test]
async fn reauth_required_marks_the_account_and_stops_polling() {
    let h = H::new();
    h.put_at("a", 1);
    h.connect_and_sync().await;
    h.fake.fail(Op::Page, SyncError::ReauthRequired);
    h.advance(h.config.poll_every_push + Duration::from_secs(1));
    h.engine.run_until_idle().await.unwrap();
    assert_eq!(
        h.control.account(ACCOUNT).unwrap().unwrap().state,
        AccountState::ReauthRequired
    );

    let calls = h.fake.calls();
    h.advance(h.config.rediscover_every * 2);
    h.engine.run_until_idle().await.unwrap();
    let after = h.fake.calls();
    assert_eq!((after.pages, after.discover), (calls.pages, calls.discover));
}

#[tokio::test]
async fn missing_calendar_is_deleted_only_by_a_complete_discovery() {
    let h = H::new();
    h.fake.add_collection(meta("cal2"));
    h.put("cal2", "x", "v1", &single("x", h.clock_now()), None);
    h.connect_and_sync().await;
    assert_eq!(h.live("cal2", &["x"]), ["x"]);

    h.fake.unlist_collection(&cal("cal2"));
    h.fake.fail(
        Op::Discover,
        SyncError::PartialDiscovery {
            found: vec![meta("cal1")],
            source: "second page failed".into(),
        },
    );
    h.advance(h.config.rediscover_every + Duration::from_secs(1));
    h.engine.run_until_idle().await.unwrap();
    assert!(!h.collection("cal2").unwrap().gone);
    assert_eq!(h.live("cal2", &["x"]), ["x"]);

    let cal2 = h.col("cal2");
    let x = h.entity_of("cal2", "x");
    h.advance(h.config.rediscover_every + Duration::from_secs(1));
    h.engine.run_until_idle().await.unwrap();
    assert!(h.collection("cal2").is_none(), "purged");
    assert_eq!(h.entity_changes(ChangeOp::Deleted), [x]);
    assert!(
        h.journal()
            .iter()
            .any(|c| c.target == ChangeTarget::Collection(cal2) && c.op == ChangeOp::Deleted)
    );
}

/// A daily series of five from 2027-01-04 09:00 Prague.
fn series(uid: &str) -> CalendarEvent {
    ev(json!({
        "uid": uid, "start": "2027-01-04T09:00:00", "timeZone": "Europe/Prague",
        "duration": "PT1H", "recurrenceRules": ["FREQ=DAILY;COUNT=5"]
    }))
}

fn exception(uid: &str, status: &str, start: &str) -> CalendarEvent {
    ev(json!({
        "uid": uid, "start": start, "timeZone": "Europe/Prague", "duration": "PT1H",
        "recurrenceId": "2027-01-05T09:00:00", "recurrenceIdTimeZone": "Europe/Prague",
        "status": status
    }))
}

fn starts(h: &H) -> Vec<String> {
    h.range(ts("2027-01-01T00:00:00Z"), ts("2027-02-01T00:00:00Z"))
        .occurrences
        .iter()
        .map(|o| o.start.to_string())
        .collect()
}

#[tokio::test]
async fn cancelled_instance_of_imported_series_joins_its_master() {
    let h = H::new();
    // Imported series: the master keeps a foreign iCalUID its instances do not share.
    h.put(
        "cal1",
        "m1",
        "v1",
        &series("foreign-123@outlook.example"),
        None,
    );
    h.put(
        "cal1",
        "m1_20270105T080000Z",
        "v1",
        &exception("m1@google.example", "cancelled", "2027-01-05T09:00:00"),
        Some("m1"),
    );
    h.connect_and_sync().await;

    assert_eq!(
        starts(&h),
        [
            "2027-01-04T08:00:00Z",
            "2027-01-06T08:00:00Z",
            "2027-01-07T08:00:00Z",
            "2027-01-08T08:00:00Z"
        ]
    );
}

#[tokio::test]
async fn exception_on_an_earlier_page_joins_when_its_master_lands() {
    let h = H::new();
    // Page size 2: `a-exc` and `b-filler` come on page one, the master on page two.
    h.put(
        "cal1",
        "a-exc",
        "v1",
        &exception("z-master", "confirmed", "2027-01-05T15:00:00"),
        Some("z-master"),
    );
    h.put_at("b-filler", 1);
    h.put("cal1", "z-master", "v1", &series("z-master"), None);
    h.connect_and_sync().await;

    assert_eq!(
        starts(&h),
        [
            "2027-01-04T08:00:00Z",
            "2027-01-05T14:00:00Z",
            "2027-01-06T08:00:00Z",
            "2027-01-07T08:00:00Z",
            "2027-01-08T08:00:00Z"
        ]
    );
}

#[tokio::test]
async fn normal_series_with_shared_uid_is_unchanged() {
    let h = H::new();
    h.put("cal1", "s", "v1", &series("s@google.com"), None);
    h.put(
        "cal1",
        "s_20270105T080000Z",
        "v1",
        &exception("s@google.com", "confirmed", "2027-01-05T11:00:00"),
        Some("s"),
    );
    h.put("cal1", "t", "v1", &series("t@google.com"), None);
    h.put(
        "cal1",
        "t_20270105T080000Z",
        "v1",
        &exception("t@google.com", "cancelled", "2027-01-05T09:00:00"),
        Some("t"),
    );
    h.connect_and_sync().await;

    let mut expected = vec![
        "2027-01-04T08:00:00Z",
        "2027-01-04T08:00:00Z",
        "2027-01-05T10:00:00Z",
        "2027-01-06T08:00:00Z",
        "2027-01-06T08:00:00Z",
        "2027-01-07T08:00:00Z",
        "2027-01-07T08:00:00Z",
        "2027-01-08T08:00:00Z",
        "2027-01-08T08:00:00Z",
    ];
    expected.sort();
    let mut got = starts(&h);
    got.sort();
    assert_eq!(got, expected);
}

#[tokio::test]
async fn too_expensive_series_is_reported_unindexed_not_empty() {
    let h = H::new();
    let minutely = ev(json!({
        "uid": "busy", "start": "2000-01-01T00:00:00", "timeZone": "Etc/UTC",
        "duration": "PT1M", "recurrenceRules": ["FREQ=MINUTELY"]
    }));
    h.put("cal1", "busy", "v1", &minutely, None);
    h.connect_and_sync().await;

    let entity = h.entity_of("cal1", "busy");
    let now = h.clock_now();
    let page = h.range(now, now + SignedDuration::from_hours(24 * 7));
    assert!(page.occurrences.iter().all(|o| o.entity != entity));
    assert!(page.unindexed.iter().any(|s| s.entity == entity));
    assert_eq!(h.live("cal1", &["busy"]), ["busy"]);
}

#[tokio::test]
async fn failed_index_extension_keeps_indexed_occurrences() {
    let h = H::with(|c| c.index_forward = Duration::from_secs(400 * 86_400));
    let now = h.clock_now();
    // A rule that never matches walks day by day from DTSTART; 100 000 empty days
    // make the expansion too expensive. DTSTART sits so that the first horizon
    // stays below that and the moved one does not.
    let horizon = now + SignedDuration::from_hours(24 * 400);
    let dtstart = (horizon + SignedDuration::from_hours(24 * (30 - 100_000)))
        .to_zoned(TimeZone::UTC)
        .datetime()
        .round(jiff::Unit::Day)
        .unwrap();
    let rdate = (now + SignedDuration::from_hours(24 * 10))
        .to_zoned(TimeZone::UTC)
        .datetime()
        .round(jiff::Unit::Hour)
        .unwrap();
    let odd = ev(json!({
        "uid": "odd", "start": dtstart.to_string(), "timeZone": "Etc/UTC", "duration": "PT1H",
        "recurrenceRules": ["FREQ=DAILY;BYMONTH=2;BYMONTHDAY=30"],
        "recurrenceOverrides": { rdate.to_string(): {} }
    }));
    h.put("cal1", "odd", "v1", &odd, None);
    // Control: a plain daily series whose horizon does move.
    let plain = ev(json!({
        "uid": "plain", "start": "2026-01-01T09:00:00", "timeZone": "Etc/UTC",
        "duration": "PT1H", "recurrenceRules": ["FREQ=DAILY"]
    }));
    h.put("cal1", "plain", "v1", &plain, None);
    h.connect_and_sync().await;

    let entity = h.entity_of("cal1", "odd");
    let control = h.entity_of("cal1", "plain");
    let horizon_of = |h: &H, e: EntityId| {
        let refs = h.store().series_to_extend(Timestamp::MAX, 10).unwrap();
        refs.into_iter()
            .find(|s| s.entity == e)
            .unwrap()
            .indexed_until
    };
    let rdate = rdate.to_zoned(TimeZone::UTC).unwrap().timestamp();
    let around = |h: &H| {
        h.range(
            rdate - SignedDuration::from_hours(1),
            rdate + SignedDuration::from_hours(2),
        )
    };
    assert!(around(&h).occurrences.iter().any(|o| o.entity == entity));
    let old = horizon_of(&h, entity);
    let control_before = horizon_of(&h, control);

    h.advance(Duration::from_secs(60 * 86_400));
    h.engine.run_until_idle().await.unwrap();

    // The extension failed: rows and horizon stay, the series reports unindexed past it.
    assert!(around(&h).occurrences.iter().any(|o| o.entity == entity));
    assert!(
        horizon_of(&h, control) > control_before,
        "the extension job ran"
    );
    assert_eq!(horizon_of(&h, entity), old);
    let page = h.range(old, old + SignedDuration::from_hours(24 * 30));
    assert!(page.unindexed.iter().any(|s| s.entity == entity));

    // An edit re-expands the series: the new horizon is too expensive, so it
    // falls back to the previous one and keeps the indexed occurrences.
    h.put("cal1", "odd", "v2", &odd, None);
    h.advance(h.config.poll_every_push + Duration::from_secs(1));
    h.engine.run_until_idle().await.unwrap();
    assert_eq!(
        h.store()
            .items_by_remote(h.col("cal1"), &[RemoteKey("odd".into())])
            .unwrap()[0]
            .validator
            .0,
        "v2"
    );
    assert!(around(&h).occurrences.iter().any(|o| o.entity == entity));
    assert_eq!(horizon_of(&h, entity), old);
}

/// Decodes like [`CanonicalCodec`] but panics on the event with uid "boom".
struct PanickingCodec;

impl Codec<Event> for PanickingCodec {
    const MAPPING_VERSION: u32 = 1;

    fn decode(raw: &Raw) -> Decoded<CalendarEvent> {
        let d = CanonicalCodec::decode(raw);
        if let Decoded::Canonical(ev) = &d
            && ev.uid == "boom"
        {
            panic!("codec bug");
        }
        d
    }
}

/// A vault like syncd's: the tokens live in the control store and go with
/// the account.
struct AccountVault {
    control: Arc<SqliteControl>,
    inner: Arc<MemoryVault>,
}

impl TokenVault for AccountVault {
    fn load(&self, account: AccountId) -> Result<Option<AccountTokens>, StoreError> {
        if self.control.account(account)?.is_none() {
            return Ok(None);
        }
        self.inner.load(account)
    }

    fn store(&self, account: AccountId, t: &AccountTokens) -> Result<(), StoreError> {
        self.inner.store(account, t)
    }
}

impl H {
    fn account_vault(&self) -> Arc<dyn TokenVault> {
        Arc::new(AccountVault {
            control: self.control.clone(),
            inner: self.vault.clone(),
        })
    }

    fn state(&self) -> Option<AccountState> {
        self.control.account(ACCOUNT).unwrap().map(|a| a.state)
    }
}

#[tokio::test]
async fn busy_collection_neither_blocks_the_lane_nor_runs_twice() {
    let h = H::with(|c| {
        c.sync_concurrency = 2;
        c.tick = Duration::from_millis(10);
    });
    h.put_at("a", 1);
    h.connect_and_sync().await;
    let ch = h.control.channels(TENANT, Some(ACCOUNT)).unwrap().remove(0);
    let before = h.fake.calls();
    let full_before = h.collection("cal1").unwrap().status.full_read_at;

    // The safety poll finds its cursor expired and holds a full read.
    h.fake.expire_cursors();
    h.fake.hold_full_reads();
    h.advance(h.config.poll_every_push + Duration::from_secs(1));
    let running = h.engine.start();
    until(|| h.fake.calls().full_reads == before.full_reads + 1).await;

    // A push for the same collection takes the second slot of the lane and
    // gives it back: discovery still runs.
    h.engine.on_push(&ch).await.unwrap();
    h.advance(Duration::from_secs(1));
    tokio::time::sleep(Duration::from_millis(200)).await;
    h.engine.connect(ACCOUNT).await.unwrap();
    until(|| h.fake.calls().discover == before.discover + 1).await;

    h.fake.release_full_reads();
    until(|| h.collection("cal1").unwrap().status.full_read_at > full_before).await;
    drop(running);
    assert_eq!(
        h.fake.calls().max_open_reads,
        1,
        "one collection read twice at once"
    );
}

#[tokio::test]
async fn panicking_job_is_recorded_and_retried_with_backoff() {
    let h = H::new();
    h.put_at("a", 1);
    h.connect_and_sync().await;
    let start = h.clock_now() + SignedDuration::from_hours(24);
    h.put("cal1", "boom", "v1", &single("boom", start), None);
    let engine = h.engine_with::<PanickingCodec>(h.vault.clone());
    h.advance(h.config.poll_every_push + Duration::from_secs(1));
    engine.run_until_idle().await.unwrap();

    let err = h.collection("cal1").unwrap().status.error.unwrap();
    assert!(
        err.message.contains("codec bug") && !err.permanent,
        "{err:?}"
    );
    // Back after the first backoff step, with the attempt counted.
    let (now, lease) = (h.clock_now(), Duration::from_secs(60));
    let early = h
        .control
        .lease_due(Lane::Sync, now + SignedDuration::from_secs(9), 10, lease)
        .unwrap();
    assert!(early.is_empty());
    let jobs = h
        .control
        .lease_due(Lane::Sync, now + SignedDuration::from_secs(11), 10, lease)
        .unwrap();
    let job = jobs
        .iter()
        .find(|j| matches!(j.spec.kind, JobKind::Sync { .. }))
        .unwrap();
    assert_eq!(job.attempts, 1);
}

#[tokio::test]
async fn removing_an_account_stops_its_channels_before_its_tokens_go() {
    let h = H::new();
    h.fake.add_collection(meta("cal2"));
    h.put_at("a", 1);
    h.connect_and_sync().await;
    assert_eq!(h.fake.channels().len(), 2);

    let engine = h.engine_with::<CanonicalCodec>(h.account_vault());
    engine.remove_account(ACCOUNT).await.unwrap();
    assert_eq!(h.fake.calls().unsubscribe, 2);
    assert!(h.fake.channels().is_empty());
    assert_eq!(h.state(), None);
    assert!(
        h.control
            .channels(TENANT, Some(ACCOUNT))
            .unwrap()
            .is_empty()
    );
    assert!(h.store().collections().unwrap().is_empty());

    // Nothing of the account runs any more.
    let calls = h.fake.calls();
    h.advance(h.config.rediscover_every * 2);
    engine.run_until_idle().await.unwrap();
    assert_eq!(h.fake.calls(), calls);
}

#[tokio::test]
async fn removal_cut_short_continues_after_a_restart() {
    let h = H::new();
    h.put_at("a", 1);
    h.connect_and_sync().await;
    // The process died right after marking the account.
    h.control
        .set_account_state(ACCOUNT, AccountState::Removing)
        .unwrap();

    let engine = h.engine_with::<CanonicalCodec>(h.account_vault());
    h.advance(h.config.rediscover_every + Duration::from_secs(1));
    engine.run_until_idle().await.unwrap();
    assert_eq!(h.state(), None);
    assert_eq!(h.fake.calls().unsubscribe, 1);
    assert!(h.fake.channels().is_empty());
    assert!(h.store().collections().unwrap().is_empty());
}

#[tokio::test]
async fn unauthorized_twice_in_a_row_requires_sign_in() {
    let h = H::new();
    h.put_at("a", 1);
    h.connect_and_sync().await;
    // Each attempt refreshes the token and tries again once.
    for _ in 0..4 {
        h.fake.fail(Op::Page, SyncError::Unauthorized);
    }
    h.advance(h.config.poll_every_push + Duration::from_secs(1));
    h.engine.run_until_idle().await.unwrap();
    assert_eq!(h.state(), Some(AccountState::Active));
    assert!(h.collection("cal1").unwrap().status.error.is_some());

    h.advance(Duration::from_secs(11));
    h.engine.run_until_idle().await.unwrap();
    assert_eq!(h.state(), Some(AccountState::ReauthRequired));
}
