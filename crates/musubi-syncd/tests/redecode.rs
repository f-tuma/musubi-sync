//! K5: re-decoding stored Google items after a codec's `MAPPING_VERSION`
//! changed rewrites them offline and, with the same canonical form, journals
//! nothing (plan ch. 10, item 1). K12d: a tenant file of the binary before
//! task occurrences (`MAIN_V2`) gets them when the new binary starts.

mod common;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use common::tasks;
use jiff::{SignedDuration, Timestamp};
use musubi_engine::{Codecs, Config, DecodeFn, Engine, Providers};
use musubi_google::{GoogleCodec, GoogleConfig, GoogleProvider};
use musubi_model::capability::ProviderId;
use musubi_model::control::{AccountRecord, AccountState, AccountTokens, Credentials};
use musubi_model::store::RangeQuery;
use musubi_model::{
    AccountId, CalendarEvent, Codec, ControlStore, Decoded, EntityKind, Event, Raw, Storage,
    TenantId, TenantStore, TimeWindow, TokenVault,
};
use musubi_store_sqlite::{SqliteControl, SqliteStorage};
use musubi_testkit::dav::{Config as DavConfig, FakeDav, Kind};
use musubi_testkit::google::FakeGoogle;
use musubi_testkit::harness::{self, Api, Daemon};
use musubi_testkit::{ManualClock, MemoryVault};
use secrecy::SecretString;
use serde_json::json;

const TENANT: TenantId = TenantId::from_u128(0x5e);
const ACCOUNT: AccountId = AccountId::from_u128(0xacc);

/// [`GoogleCodec`] as one mapping version before.
struct OlderGoogle;

impl Codec<Event> for OlderGoogle {
    const MAPPING_VERSION: u32 = GoogleCodec::MAPPING_VERSION - 1;

    fn decode(raw: &Raw) -> Decoded<CalendarEvent> {
        GoogleCodec::decode(raw)
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn google_items_redecode_canonically_identical_with_nothing_journaled() {
    let g = FakeGoogle::start(Default::default()).await.expect("fake");
    let primary = g.primary();
    let dt = |t: &str| json!({ "dateTime": t, "timeZone": "Europe/Prague" });
    let master = g.put_event(
        &primary,
        json!({ "summary": "Standup", "start": dt("2027-01-04T09:00:00+01:00"),
            "end": dt("2027-01-04T09:30:00+01:00"), "recurrence": ["RRULE:FREQ=DAILY;COUNT=5"] }),
    );
    g.put_event(
        &primary,
        json!({ "id": format!("{master}_20270105T080000Z"), "summary": "Standup",
            "recurringEventId": master, "originalStartTime": dt("2027-01-05T09:00:00+01:00"),
            "start": dt("2027-01-05T11:00:00+01:00"), "end": dt("2027-01-05T11:30:00+01:00") }),
    );
    g.put_event(
        &primary,
        json!({ "summary": "Holiday", "start": { "date": "2027-01-06" }, "end": { "date": "2027-01-07" } }),
    );

    let dir = tempfile::tempdir().expect("tempdir");
    let storage = Arc::new(SqliteStorage::new(&dir.path().join("tenants"), 4).expect("storage"));
    storage.create_tenant(TENANT).expect("tenant");
    let control = Arc::new(SqliteControl::open(&dir.path().join("control.db")).expect("control"));
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
    let clock = ManualClock::new(Timestamp::now());
    let engine = |event: DecodeFn<Event>| {
        let provider = GoogleProvider::new(
            GoogleConfig {
                client_id: "musubi-test.apps.example".into(),
                client_secret: SecretString::from("test-secret"),
                api_base: g.calendar_api().parse().expect("url"),
                token_url: g.token_url().parse().expect("url"),
                tasks_base: g.tasks_api().parse().expect("url"),
                people_base: g.people_api().parse().expect("url"),
                discover_tasks_people: false,
            },
            Arc::new(clock.clone()),
        )
        .expect("provider");
        Engine::new(
            storage.clone(),
            Providers::new().with(
                Arc::new(provider),
                Codecs {
                    event: Some(event),
                    ..Codecs::default()
                },
            ),
            control.clone(),
            vault.clone(),
            Arc::new(clock.clone()),
            Arc::new(|_, _| {}),
            Config {
                callback: Some(
                    "http://127.0.0.1:9/v1/webhooks/google"
                        .parse()
                        .expect("uri"),
                ),
                ..Config::default()
            },
        )
    };

    let old = engine(DecodeFn::of::<OlderGoogle>());
    old.connect(ACCOUNT).await.expect("connect");
    old.run_until_idle().await.expect("sync");
    let store = storage.open(TENANT).expect("open");
    let range = || {
        store
            .range(&RangeQuery {
                window: TimeWindow::new(
                    "2027-01-01T00:00:00Z".parse().unwrap(),
                    "2027-02-01T00:00:00Z".parse().unwrap(),
                )
                .unwrap(),
                collections: None,
                kinds: vec![EntityKind::Event],
                done: None,
                zone: jiff::tz::TimeZone::UTC,
                after: None,
                limit: 100,
                ids_only: false,
            })
            .expect("range")
            .occurrences
            .iter()
            .map(|o| (o.entity, o.start))
            .collect::<Vec<_>>()
    };
    let before = range();
    assert_eq!(before.len(), 6, "five standups and the holiday");
    let head = store.head().expect("head");
    let requests = g.requests().len();

    let new = engine(DecodeFn::of::<GoogleCodec>());
    assert_eq!(new.redecode(TENANT).await.expect("redecode"), 3);
    assert_eq!(g.requests().len(), requests, "no network call");
    assert_eq!(store.head().expect("head"), head, "nothing journaled");
    assert_eq!(store.changes(head, 10).expect("changes").changes, []);
    assert_eq!(range(), before);
    assert_eq!(new.redecode(TENANT).await.expect("again"), 0);
}

/// Plan 12.7: a tenant of the K11 binary holds tasks without occurrence rows;
/// the new binary migrates it to `MAIN_V3` and re-decodes them on start,
/// without the DAV server, so `kind=task` ranges return their occurrences.
#[tokio::test(flavor = "multi_thread")]
async fn tasks_of_a_k11_tenant_are_in_the_range_after_the_new_binary_starts() {
    let d = FakeDav::start(DavConfig::default())
        .await
        .expect("fake dav");
    let cal = d.add_collection(Kind::Calendar(&["VEVENT", "VTODO"]), "home", "Home");
    d.put(&cal, "water.ics", tasks::RECURRING_TODO);
    d.put(&cal, "standup.ics", tasks::EVENT);
    let g = FakeGoogle::start(Default::default()).await.expect("google");
    let dir = tempfile::tempdir().expect("tempdir");
    let listen = harness::free_addr();
    let mut config = harness::config(listen, &dir.path().join("data"), &g);
    config["dav"] = json!({ "allow_private": true });
    let path = dir.path().join("config.json");
    std::fs::write(&path, config.to_string()).expect("config");
    let bin: &Path = env!("CARGO_BIN_EXE_musubi-syncd").as_ref();
    let mut daemon = tokio::task::block_in_place(|| Daemon::serve(bin, &path, &[]));
    let api = Api::new(listen);
    let tenant = api.tenant().await;
    let (admin, read) = (
        api.key(&tenant, "admin").await,
        api.key(&tenant, "read").await,
    );
    let body = json!({
        "profile": "generic", "server_url": d.server_url(),
        "username": "alice", "password": "secret",
    });
    let (s, v) = api
        .call("POST", "/v1/accounts/dav", Some(&admin), Some(body))
        .await
        .expect("connect");
    assert_eq!(s, 200, "{v}");
    let tasks_in_range = async || {
        let q = format!("/v1/entities?{}&kind=task", tasks::NOVEMBER);
        let (s, v) = api.call("GET", &q, Some(&read), None).await.expect("range");
        assert_eq!(s, 200, "{v}");
        v
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while tasks::occurrences(&tasks_in_range().await) != tasks::expected_tasks() {
        assert!(
            std::time::Instant::now() < deadline,
            "no task occurrences:\n{}",
            daemon.tail(40)
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // The file as the K11 binary left it, and no DAV server to read from.
    daemon.kill();
    daemon.wait().expect("wait");
    let main_db = dir
        .path()
        .join("data/tenants")
        .join(&tenant)
        .join("main.db");
    musubi_testkit::store::downgrade_to_main_v2(&main_db);
    drop(d);
    // Plain log lines to match `items=1`.
    let plain = [("NO_COLOR", "1".to_owned())];
    let daemon = tokio::task::block_in_place(|| Daemon::serve(bin, &path, &plain));
    let line = tokio::task::block_in_place(|| {
        daemon.wait_line("stored items re-decoded", Duration::from_secs(30))
    });
    assert!(
        line.as_deref().is_some_and(|l| l.contains("items=1")),
        "no re-decode of the task: {line:?}\n{}",
        daemon.tail(40)
    );
    assert_eq!(
        tasks::occurrences(&tasks_in_range().await),
        tasks::expected_tasks()
    );
}
