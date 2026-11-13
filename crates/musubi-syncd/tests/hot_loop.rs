//! K8a: a subscribed holiday calendar sent the engine into a full-read loop
//! (about one per second, live, 12. 11. 2026). Each test reproduces one
//! suspected cause in the fake Google server and runs the engine with the
//! real Google adapter over ten minutes of simulated time, counting the
//! calendar's requests.

use std::sync::Arc;
use std::time::Duration;

use jiff::{SignedDuration, Timestamp};
use musubi_engine::{Config, Engine};
use musubi_google::{GoogleCodec, GoogleConfig, GoogleProvider};
use musubi_model::capability::ProviderId;
use musubi_model::control::{AccountRecord, AccountState, AccountTokens, Credentials};
use musubi_model::store::{CollectionState, Coverage};
use musubi_model::{AccountId, ControlStore, Storage, TenantId, TenantStore, TokenVault};
use musubi_store_sqlite::{SqliteControl, SqliteStorage};
use musubi_testkit::google::{Change, FakeGoogle, Quirk};
use musubi_testkit::{ManualClock, MemoryVault};
use secrecy::SecretString;
use serde_json::json;

const TENANT: TenantId = TenantId::from_u128(0x8a);
const ACCOUNT: AccountId = AccountId::from_u128(0xacc);
const HOLIDAY: &str = "cs.czech#holiday@group.v.calendar.google.com";
/// How `HOLIDAY` appears in a request path.
const HOLIDAY_PATH: &str = "/calendars/cs.czech%23holiday@group.v.calendar.google.com/events";
const STEP: Duration = Duration::from_secs(10);
const SPAN: Duration = Duration::from_secs(600);

struct H {
    g: FakeGoogle,
    clock: ManualClock,
    storage: Arc<SqliteStorage>,
    engine: Engine<SqliteStorage, GoogleCodec>,
    control: Arc<SqliteControl>,
    _dir: tempfile::TempDir,
}

/// Requests to the holiday calendar by kind.
#[derive(Debug, Default)]
struct Counts {
    full: usize,
    delta: usize,
    watch: usize,
}

impl H {
    async fn new(quirks: &[Quirk]) -> Self {
        let g = FakeGoogle::start(Default::default()).await.expect("fake");
        g.apply(&Change::AddCalendar {
            entry: json!({ "id": HOLIDAY, "summary": "Státní svátky", "accessRole": "reader" }),
        });
        g.put_event(
            HOLIDAY,
            json!({
                "summary": "Den boje za svobodu a demokracii",
                "start": { "date": "2026-11-17" },
                "end": { "date": "2026-11-18" },
            }),
        );
        for q in quirks {
            g.quirk(HOLIDAY, *q);
        }

        let dir = tempfile::tempdir().expect("tempdir");
        let storage =
            Arc::new(SqliteStorage::new(&dir.path().join("tenants"), 4).expect("storage"));
        storage.create_tenant(TENANT).expect("tenant");
        let control =
            Arc::new(SqliteControl::open(&dir.path().join("control.db")).expect("control"));
        control.insert_tenant(TENANT).expect("tenant");
        control
            .upsert_account(&AccountRecord {
                id: ACCOUNT,
                tenant: TENANT,
                provider: ProviderId("google".into()),
                subject: "fake".into(),
                state: AccountState::Connecting,
            })
            .expect("account");
        let vault = Arc::new(MemoryVault::default());
        vault
            .store(
                ACCOUNT,
                &Credentials::OAuth(AccountTokens {
                    refresh: SecretString::from(g.refresh_token()),
                    access: SecretString::from(g.access_token()),
                    access_until: Timestamp::now() + SignedDuration::from_hours(24 * 365),
                }),
            )
            .expect("tokens");
        // The job queue compares leases with the wall clock: start there.
        let clock = ManualClock::new(Timestamp::now());
        let provider = GoogleProvider::new(
            GoogleConfig {
                client_id: "musubi-test.apps.example".into(),
                client_secret: SecretString::from("test-secret"),
                api_base: g.calendar_api().parse().expect("url"),
                token_url: g.token_url().parse().expect("url"),
                tasks_base: g.tasks_api().parse().expect("url"),
                people_base: g.people_api().parse().expect("url"),
            },
            Arc::new(clock.clone()),
        )
        .expect("provider");
        let engine = Engine::new(
            storage.clone(),
            Arc::new(provider),
            control.clone(),
            vault,
            Arc::new(clock.clone()),
            Arc::new(|_, _| {}),
            Config {
                // Nothing listens there; the fake's deliveries just fail.
                callback: Some(
                    "http://127.0.0.1:9/v1/webhooks/google"
                        .parse()
                        .expect("uri"),
                ),
                ..Config::default()
            },
        );
        Self {
            g,
            clock,
            storage,
            engine,
            control,
            _dir: dir,
        }
    }

    /// Connects and runs `span` of simulated time in `STEP`s.
    async fn run(&self, span: Duration) {
        self.engine.connect(ACCOUNT).await.expect("connect");
        self.advance(span, STEP).await;
    }

    /// Runs every due job, then moves the clock by `step`, until `span` passed.
    async fn advance(&self, span: Duration, step: Duration) {
        let mut left = span;
        loop {
            self.engine.run_until_idle().await.expect("run");
            if left.is_zero() {
                break;
            }
            let step = step.min(left);
            self.clock
                .advance(SignedDuration::try_from(step).expect("step"));
            left -= step;
        }
    }

    fn counts(&self) -> Counts {
        let mut c = Counts::default();
        for r in self.g.requests() {
            let Some((_, rest)) = r.split_once(HOLIDAY_PATH) else {
                continue;
            };
            if rest.starts_with("/watch") {
                c.watch += 1;
            } else if rest.contains("syncToken=") {
                c.delta += 1;
            } else if !rest.contains("timeMin=") && !rest.contains("pageToken=") {
                c.full += 1;
            }
        }
        c
    }

    fn holiday(&self) -> CollectionState {
        let s = self.storage.open(TENANT).expect("open");
        s.collections()
            .expect("collections")
            .into_iter()
            .find(|c| c.remote.remote.0 == HOLIDAY)
            .expect("holiday collection")
    }
}

/// (a) `events.watch` refused: the calendar is polled at the normal interval,
/// and the watch is asked for again only after a day.
#[tokio::test(flavor = "multi_thread")]
async fn refused_watch_polls_without_asking_again() {
    let h = H::new(&[Quirk::RefuseWatch]).await;
    h.run(SPAN).await;
    let c = h.counts();
    assert_eq!(c.watch, 1, "watch retried every cycle: {c:?}");
    assert_eq!(c.full, 1, "{c:?}");
    // Polls every `poll_every` (5 min): the delta after the full read, then two.
    assert!((2..=4).contains(&c.delta), "{c:?}");
    let col = h.holiday();
    assert_eq!(col.status.coverage, Coverage::Full);
    assert!(col.status.error.is_none(), "{:?}", col.status.error);

    h.advance(Duration::from_secs(86_400), Duration::from_secs(3600))
        .await;
    assert_eq!(
        h.counts().watch,
        2,
        "the watch is retried rarely, not never"
    );
}

/// (b) The live holiday calendar: its fresh sync token answers `410` and
/// `events.watch` is refused. After two such tokens the collection is read
/// in full only, at the poll interval, without an error.
#[tokio::test(flavor = "multi_thread")]
async fn delta_always_gone_reads_in_full_at_the_poll_interval() {
    let h = H::new(&[Quirk::ExpireEverySyncToken, Quirk::RefuseWatch]).await;
    h.run(SPAN).await;
    let c = h.counts();
    // Full, delta 410, full, delta 410, full; then one every 5 min.
    assert_eq!(c.delta, 2, "no delta once full-read-only: {c:?}");
    assert!((4..=6).contains(&c.full), "{c:?}");
    let col = h.holiday();
    assert!(col.status.error.is_none(), "{:?}", col.status.error);
    assert_eq!(col.status.coverage, Coverage::Full);

    h.advance(Duration::from_secs(3600), STEP).await;
    let more = h.counts();
    assert_eq!(more.delta, 2, "{more:?}");
    assert!((11..=13).contains(&(more.full - c.full)), "{more:?}");
    assert!(h.holiday().status.error.is_none());
}

/// (c) A full read without `nextSyncToken`: the read still completes and the
/// collection is read in full only, like (b), without an error.
#[tokio::test(flavor = "multi_thread")]
async fn full_read_without_sync_token_reads_in_full_only() {
    let h = H::new(&[Quirk::NoSyncToken]).await;
    h.run(SPAN).await;
    let c = h.counts();
    assert!((2..=4).contains(&c.full), "{c:?}");
    assert_eq!(c.delta, 0, "no sync token, no delta request: {c:?}");
    let col = h.holiday();
    assert_eq!(col.status.coverage, Coverage::Full);
    assert!(col.status.error.is_none(), "{:?}", col.status.error);
}

/// (d) A renewal Google refuses leaves no channel row behind: the collection
/// is polled like after a first refusal.
#[tokio::test(flavor = "multi_thread")]
async fn refused_renewal_drops_the_channel_and_polls() {
    let h = H::new(&[]).await;
    h.run(SPAN).await;
    let id = h.holiday().id;
    let mine = |h: &H| {
        h.control
            .channels(TENANT, Some(ACCOUNT))
            .expect("channels")
            .into_iter()
            .filter(|c| c.collection == id)
            .count()
    };
    assert_eq!(mine(&h), 1, "a channel at first");
    assert_eq!(h.counts().watch, 1);

    h.g.quirk(HOLIDAY, Quirk::RefuseWatch);
    // The renewal runs at two thirds of the 7-day channel.
    h.advance(Duration::from_secs(5 * 86_400), Duration::from_secs(3600))
        .await;
    assert_eq!(h.counts().watch, 2, "the renewal was asked for");
    assert_eq!(mine(&h), 0, "the refused channel's row is gone");

    let before = h.counts();
    h.advance(Duration::from_secs(3600), STEP).await;
    let after = h.counts();
    assert!(
        (11..=13).contains(&(after.delta - before.delta)),
        "polled every 5 min: {before:?} -> {after:?}"
    );
    assert_eq!(after.watch, 2, "no new watch right away");
    assert!(h.holiday().status.error.is_none());
}

/// (e) A collection taken for full-read-only (two token invalidations in a
/// row) tries a delta again after a day; once it works, deltas resume.
#[tokio::test(flavor = "multi_thread")]
async fn full_read_only_probes_a_delta_after_a_day() {
    let h = H::new(&[Quirk::ExpireEverySyncToken]).await;
    h.run(SPAN).await;
    let c = h.counts();
    assert_eq!(c.delta, 2, "full-read-only after two expired tokens: {c:?}");

    h.g.clear_quirks(HOLIDAY);
    let hour = Duration::from_secs(3600);
    h.advance(22 * hour, hour).await;
    let still = h.counts();
    assert_eq!(still.delta, 2, "no delta before the day is up: {still:?}");
    assert!(still.full > c.full, "{still:?}");

    h.advance(3 * hour, hour).await;
    let probed = h.counts();
    assert!(probed.delta > 2, "the delta is tried again: {probed:?}");
    h.advance(3 * hour, hour).await;
    let after = h.counts();
    assert_eq!(after.full, probed.full, "deltas only again: {after:?}");
    assert!(after.delta > probed.delta, "{after:?}");
    assert!(h.holiday().status.error.is_none());
}

/// (b) over more than a day: the delta probed after `DELTA_RETRY` fails like
/// every other one and re-arms full-read-only, without an error and without
/// reads beyond one full read per poll interval and one probe per day.
#[tokio::test(flavor = "multi_thread")]
async fn failed_delta_probe_rearms_full_read_only() {
    let h = H::new(&[Quirk::ExpireEverySyncToken, Quirk::RefuseWatch]).await;
    h.run(SPAN).await;
    let c = h.counts();
    assert_eq!(c.delta, 2, "{c:?}");

    let day = Duration::from_secs(86_400);
    let mut last = c;
    for _ in 0..2 {
        h.advance(day, Duration::from_secs(3600)).await;
        let now = h.counts();
        assert!(
            h.holiday().status.error.is_none(),
            "{:?}",
            h.holiday().status.error
        );
        assert_eq!(now.delta - last.delta, 1, "one probe a day: {now:?}");
        // One full read per hour step (the 5-minute poll is always due),
        // plus the probe's.
        assert!(
            (23..=26).contains(&(now.full - last.full)),
            "{last:?} -> {now:?}"
        );
        last = now;
    }
}
