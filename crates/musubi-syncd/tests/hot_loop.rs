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
use musubi_model::control::{AccountRecord, AccountState, AccountTokens};
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
    async fn new(quirk: Quirk) -> Self {
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
        g.quirk(HOLIDAY, quirk);

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
                &AccountTokens {
                    refresh: SecretString::from(g.refresh_token()),
                    access: SecretString::from(g.access_token()),
                    access_until: Timestamp::now() + SignedDuration::from_hours(24 * 365),
                },
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
            },
            Arc::new(clock.clone()),
        )
        .expect("provider");
        let engine = Engine::new(
            storage.clone(),
            Arc::new(provider),
            control,
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
    let h = H::new(Quirk::RefuseWatch).await;
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

/// (b) Every delta answers `410`: full reads back off exponentially and the
/// collection carries a (transient) error.
#[tokio::test(flavor = "multi_thread")]
async fn delta_always_gone_backs_off_full_reads() {
    let h = H::new(Quirk::ExpireEverySyncToken).await;
    h.run(SPAN).await;
    let c = h.counts();
    assert!(c.full <= 4, "full reads spin: {c:?}");
    assert!(c.delta <= 4, "{c:?}");
    let col = h.holiday();
    let err = col.status.error.expect("backoff is a collection error");
    assert!(!err.permanent);
    assert!(
        err.message.contains("no usable delta cursor"),
        "{}",
        err.message
    );
    assert_eq!(col.status.coverage, Coverage::Full);
}

/// (c) A full read without `nextSyncToken`: the read still completes, the
/// next sync is a full read again, and those back off like (b).
#[tokio::test(flavor = "multi_thread")]
async fn full_read_without_sync_token_completes_and_backs_off() {
    let h = H::new(Quirk::NoSyncToken).await;
    h.run(SPAN).await;
    let c = h.counts();
    assert!((2..=4).contains(&c.full), "{c:?}");
    assert_eq!(c.delta, 0, "no sync token, no delta request: {c:?}");
    let col = h.holiday();
    assert_eq!(col.status.coverage, Coverage::Full);
    let err = col.status.error.expect("backoff is a collection error");
    assert!(!err.permanent);
}
