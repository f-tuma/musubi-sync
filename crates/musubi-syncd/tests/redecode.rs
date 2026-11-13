//! K5: re-decoding stored Google items after a codec's `MAPPING_VERSION`
//! changed rewrites them offline and, with the same canonical form, journals
//! nothing (plan ch. 10, item 1).

use std::sync::Arc;

use jiff::{SignedDuration, Timestamp};
use musubi_engine::{Codecs, Config, DecodeFn, Engine, Providers};
use musubi_google::{GoogleCodec, GoogleConfig, GoogleProvider};
use musubi_model::capability::ProviderId;
use musubi_model::control::{AccountRecord, AccountState, AccountTokens, Credentials};
use musubi_model::store::RangeQuery;
use musubi_model::{
    AccountId, CalendarEvent, Codec, ControlStore, Decoded, Event, Raw, Storage, TenantId,
    TenantStore, TimeWindow, TokenVault,
};
use musubi_store_sqlite::{SqliteControl, SqliteStorage};
use musubi_testkit::google::FakeGoogle;
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
