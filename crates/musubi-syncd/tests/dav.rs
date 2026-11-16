//! DAV accounts through the API against the fake DAV server (plan K8):
//! connect, its 400 reasons, reconnect, `POST /v1/interest`, and the Google
//! kinds the registry now reads. Nothing leaves the machine.

mod common;

use std::time::{Duration, Instant};

use common::Node;
use musubi_model::capability::ProviderId;
use musubi_model::control::{AccountRecord, AccountState, AccountTokens};
use musubi_model::{AccountId, ControlStore, Credentials, TenantId, TokenVault};
use musubi_testkit::dav::{Config as DavConfig, FakeDav, Fault, Host, Kind, Profile, Req};
use musubi_testkit::google::{Endpoint, Fault as GFault};
use secrecy::{ExposeSecret, SecretString};
use serde_json::{Value, json};

const TODO: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//musubi//test//EN\r\nBEGIN:VTODO\r\nUID:todo-1\r\nDTSTAMP:20260101T000000Z\r\nSUMMARY:buy milk\r\nEND:VTODO\r\nEND:VCALENDAR\r\n";
const CARD: &str = "BEGIN:VCARD\r\nVERSION:4.0\r\nUID:card-1\r\nFN:Bob Builder\r\nEND:VCARD\r\n";

async fn until<T>(what: &str, mut f: impl AsyncFnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(v) = f().await {
            return v;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// A node that may reach loopback DAV servers.
async fn private_node() -> Node {
    Node::start_with(|c| c["dav"] = json!({ "allow_private": true })).await
}

/// A fake DAV server with one task list calendar and one address book.
async fn dav(config: DavConfig) -> FakeDav {
    let d = FakeDav::start(config).await.expect("fake dav");
    let cal = d.add_collection(Kind::Calendar(&["VEVENT", "VTODO"]), "home", "Home");
    d.put(&cal, "todo-1.ics", TODO);
    let book = d.add_collection(Kind::AddressBook, "contacts", "Contacts");
    d.put(&book, "card-1.vcf", CARD);
    d
}

fn outside(d: &FakeDav) -> usize {
    d.requests()
        .iter()
        .filter(|r| r.host == Host::Outside)
        .count()
}

struct Tenant {
    id: TenantId,
    admin: String,
    read: String,
}

async fn tenant(n: &Node) -> Tenant {
    let id = n.tenant().await;
    let (admin, _) = n.key(id, "admin").await;
    let (read, _) = n.key(id, "read").await;
    Tenant { id, admin, read }
}

async fn connect(n: &Node, t: &Tenant, body: Value) -> (u16, Value) {
    n.call("POST", "/v1/accounts/dav", Some(&t.admin), Some(body))
        .await
}

fn generic(d: &FakeDav, password: &str) -> Value {
    json!({
        "profile": "generic", "server_url": d.server_url(),
        "username": "alice", "password": password,
    })
}

/// Canonical bodies of `GET /v1/entities?kind=<kind>`.
async fn listed(n: &Node, t: &Tenant, kind: &str) -> Vec<Value> {
    let (s, v) = n.get(&format!("/v1/entities?kind={kind}"), &t.read).await;
    assert_eq!(s, 200, "{v}");
    v["entities"]
        .as_array()
        .expect("entities")
        .iter()
        .map(|e| e["canonical"].clone())
        .collect()
}

async fn account(n: &Node, t: &Tenant, id: &str) -> Value {
    let (s, v) = n.get(&format!("/v1/accounts/{id}"), &t.read).await;
    assert_eq!(s, 200, "{v}");
    v
}

async fn collections(n: &Node, t: &Tenant) -> Vec<Value> {
    let (s, v) = n.get("/v1/collections", &t.read).await;
    assert_eq!(s, 200, "{v}");
    v["collections"].as_array().expect("collections").clone()
}

/// Connects `d` and waits until its task and contact are readable.
async fn connected(n: &Node, t: &Tenant, d: &FakeDav) -> String {
    let (s, v) = connect(n, t, generic(d, "secret")).await;
    assert_eq!(s, 200, "{v}");
    let id = v["id"].as_str().expect("id").to_owned();
    until("the task and the contact", async || {
        let tasks = listed(n, t, "task").await;
        let cards = listed(n, t, "contact").await;
        (tasks.len() == 1 && cards.len() == 1).then_some(())
    })
    .await;
    id
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dav_account_connects_and_its_view_has_no_password() {
    let n = private_node().await;
    let _engine = n.app.engine().start();
    let d = dav(DavConfig::default()).await;
    let t = tenant(&n).await;

    let (s, v) = connect(&n, &t, generic(&d, "secret")).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["provider"], "dav", "{v}");
    assert_eq!(v["profile"], "generic", "{v}");
    assert_eq!(v["username"], "alice", "{v}");
    assert_eq!(v["origins"], json!([d.url(Host::Caldav)]), "{v}");
    assert_eq!(v["missing"], json!([]), "{v}");
    assert!(v.get("reconsent").is_none(), "{v}");
    assert_eq!(
        v["subject"],
        format!("{}{}", d.url(Host::Caldav), d.principal())
    );
    assert!(!v.to_string().contains("secret"), "{v}");
    let id = v["id"].as_str().expect("id").to_owned();

    until("the task and the contact", async || {
        let tasks = listed(&n, &t, "task").await;
        let cards = listed(&n, &t, "contact").await;
        (tasks.len() == 1 && cards.len() == 1).then_some((tasks, cards))
    })
    .await;
    let tasks = listed(&n, &t, "task").await.remove(0).to_string();
    assert!(tasks.contains("buy milk"), "{tasks}");
    let cards = listed(&n, &t, "contact").await.remove(0).to_string();
    assert!(cards.contains("Bob Builder"), "{cards}");
    let (s, v) = n.get("/v1/entities?kind=event", &t.read).await;
    assert_eq!(s, 400, "events are read by range: {v}");

    let v = until("the account active", async || {
        let v = account(&n, &t, &id).await;
        (v["state"] == "active").then_some(v)
    })
    .await;
    assert_eq!(v["username"], "alice", "{v}");
    assert!(!v.to_string().contains("secret"), "{v}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_connect_answers_its_reason_and_stores_nothing() {
    let n = private_node().await;
    let d = dav(DavConfig::default()).await;
    let t = tenant(&n).await;
    let nothing_stored = || {
        let subject = format!("{}{}", d.url(Host::Caldav), d.principal());
        let acc = n
            .app
            .control()
            .account_by_subject(t.id, &ProviderId("dav".into()), &subject)
            .expect("read");
        assert!(acc.is_none(), "{acc:?}");
    };

    let (s, v) = connect(&n, &t, generic(&d, "wrong")).await;
    assert_eq!((s, &v["error"]), (400, &json!("unauthorized")), "{v}");
    assert!(!v.to_string().contains("wrong"), "{v}");
    nothing_stored();
    assert!(collections(&n, &t).await.is_empty());

    // A redirect to another origin is refused before anything is sent there.
    d.fail(
        Req::Any,
        Fault::Redirect {
            status: 301,
            to: Host::Outside,
        },
        1,
    );
    let (s, v) = connect(&n, &t, generic(&d, "secret")).await;
    assert_eq!(
        (s, &v["error"]),
        (400, &json!("cross_origin_redirect")),
        "{v}"
    );
    assert!(
        v["message"]
            .as_str()
            .expect("message")
            .contains(&d.url(Host::Outside)),
        "the message names the target: {v}"
    );
    assert_eq!(outside(&d), 0);
    nothing_stored();

    // `http` off loopback stays blocked even with `allow_private`.
    let (s, v) = connect(
        &n,
        &t,
        json!({
            "profile": "generic", "server_url": "http://192.0.2.1/",
            "username": "alice", "password": "secret",
        }),
    )
    .await;
    assert_eq!((s, &v["error"]), (400, &json!("blocked_address")), "{v}");

    // Without `allow_private`, loopback is blocked and the server sees nothing.
    let strict = Node::start().await;
    let t2 = tenant(&strict).await;
    d.clear_requests();
    let (s, v) = connect(&strict, &t2, generic(&d, "secret")).await;
    assert_eq!((s, &v["error"]), (400, &json!("blocked_address")), "{v}");
    assert!(d.requests().is_empty(), "{:?}", d.requests());
}

/// Plan 5.5: discovery gives up after 20 s.
#[tokio::test(flavor = "multi_thread")]
async fn a_server_that_does_not_answer_times_out() {
    let n = private_node().await;
    let d = dav(DavConfig::default()).await;
    d.fail(Req::Any, Fault::Delay(Duration::from_secs(60)), 1);
    let t = tenant(&n).await;
    let start = Instant::now();
    let (s, v) = connect(&n, &t, generic(&d, "secret")).await;
    assert_eq!((s, &v["error"]), (400, &json!("timeout")), "{v}");
    assert!(start.elapsed() < Duration::from_secs(30));
    assert!(collections(&n, &t).await.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_redirect_during_sync_requires_reauthorisation_and_reconnect_resumes() {
    let n = private_node().await;
    let _engine = n.app.engine().start();
    let d = dav(DavConfig::default()).await;
    let t = tenant(&n).await;
    let id = connected(&n, &t, &d).await;

    d.fail(
        Req::Any,
        Fault::Redirect {
            status: 307,
            to: Host::Outside,
        },
        1,
    );
    let (s, v) = n.call("POST", "/v1/interest", Some(&t.read), None).await;
    assert_eq!(s, 204, "{v}");
    until("reauth_required", async || {
        (account(&n, &t, &id).await["state"] == "reauth_required").then_some(())
    })
    .await;
    let errors: Vec<String> = collections(&n, &t)
        .await
        .iter()
        .filter_map(|c| c["error"]["message"].as_str().map(str::to_owned))
        .collect();
    assert!(errors.iter().any(|e| e == "endpoint_moved"), "{errors:?}");
    assert_eq!(outside(&d), 0);

    // The user confirms the password again: the same account, active.
    let mut body = generic(&d, "secret");
    body["account"] = json!(id);
    let (s, v) = connect(&n, &t, body).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(
        (&v["id"], &v["state"]),
        (&json!(id), &json!("active")),
        "{v}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reconnect_to_another_principal_is_refused_and_changes_nothing() {
    let n = private_node().await;
    let (a, b) = (
        dav(DavConfig::default()).await,
        dav(DavConfig::default()).await,
    );
    let t = tenant(&n).await;
    let (s, v) = connect(&n, &t, generic(&a, "secret")).await;
    assert_eq!(s, 200, "{v}");
    let id = v["id"].as_str().expect("id").to_owned();
    let before = account(&n, &t, &id).await;

    let mut body = generic(&b, "secret");
    body["account"] = json!(id);
    let (s, v) = connect(&n, &t, body).await;
    assert_eq!((s, &v["error"]), (400, &json!("subject_mismatch")), "{v}");
    assert_eq!(account(&n, &t, &id).await, before);

    // Split servers: CalDAV matches, only the CardDAV principal differs.
    let split = || DavConfig {
        split: true,
        ..DavConfig::default()
    };
    let (a, b) = (dav(split()).await, dav(split()).await);
    let entries = |cal: &FakeDav, card: &FakeDav| {
        json!({
            "profile": "generic", "caldav_url": cal.server_url(),
            "carddav_url": card.carddav_url(), "username": "alice", "password": "secret",
        })
    };
    let (s, v) = connect(&n, &t, entries(&a, &a)).await;
    assert_eq!(s, 200, "{v}");
    let id = v["id"].as_str().expect("id").to_owned();
    let before = account(&n, &t, &id).await;
    let mut body = entries(&a, &b);
    body["account"] = json!(id);
    let (s, v) = connect(&n, &t, body).await;
    assert_eq!((s, &v["error"]), (400, &json!("subject_mismatch")), "{v}");
    assert_eq!(account(&n, &t, &id).await, before);
    // The same entries again are the same account.
    let mut body = entries(&a, &a);
    body["account"] = json!(id);
    let (s, v) = connect(&n, &t, body).await;
    assert_eq!((s, &v["id"]), (200, &json!(id)), "{v}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_connect_cut_short_before_its_endpoint_is_finished_by_the_next() {
    let n = private_node().await;
    let d = dav(DavConfig::default()).await;
    let t = tenant(&n).await;
    // What a failure after the account row and before the endpoint leaves.
    let id = AccountId::from_u128(7);
    n.app
        .control()
        .upsert_account(&AccountRecord {
            id,
            tenant: t.id,
            provider: ProviderId("dav".into()),
            subject: format!("{}{}", d.url(Host::Caldav), d.principal()),
            state: AccountState::Connecting,
        })
        .expect("row");

    let (s, v) = connect(&n, &t, generic(&d, "secret")).await;
    assert_eq!((s, &v["id"]), (200, &json!(id)), "{v}");
    assert_eq!(v["username"], "alice", "{v}");
    assert!(n.app.control().endpoint(id).expect("read").is_some());

    // Without its endpoint, another principal is still refused.
    let other = dav(DavConfig::default()).await;
    let id = AccountId::from_u128(8);
    n.app
        .control()
        .upsert_account(&AccountRecord {
            id,
            tenant: t.id,
            provider: ProviderId("dav".into()),
            subject: "https://elsewhere.example/principal/".into(),
            state: AccountState::Connecting,
        })
        .expect("row");
    let mut body = generic(&other, "secret");
    body["account"] = json!(id);
    let (s, v) = connect(&n, &t, body).await;
    assert_eq!((s, &v["error"]), (400, &json!("subject_mismatch")), "{v}");
}

/// How long a test holds a job's request; the reconnect must end well before.
const HOLD: Duration = Duration::from_secs(3);

/// Holds a discovery of `id` on its first request while `body` reconnects
/// the account, then runs every job left.
async fn reconnect_during_discovery(n: &Node, t: &Tenant, d: &FakeDav, id: &str, mut body: Value) {
    let before = d.count("PROPFIND");
    d.fail(Req::Method("PROPFIND"), Fault::Delay(HOLD), 1);
    let account: AccountId = id.parse().expect("id");
    n.app.engine().connect(account).await.expect("connect");
    let app = n.app.clone();
    let run = tokio::spawn(async move { app.engine().run_until_idle().await });
    until("the held discovery", async || {
        (d.count("PROPFIND") > before).then_some(())
    })
    .await;
    let held = Instant::now();
    body["account"] = json!(id);
    let (s, v) = connect(n, t, body).await;
    assert_eq!(s, 200, "{v}");
    assert!(
        held.elapsed() < HOLD / 2,
        "the reconnect ended after the hold"
    );
    run.await.expect("join").expect("jobs");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_discovery_held_over_a_reconnect_stores_no_endpoint_over_it() {
    let n = private_node().await;
    let d = dav(DavConfig::default()).await;
    let t = tenant(&n).await;
    let (s, v) = connect(&n, &t, generic(&d, "secret")).await;
    assert_eq!(s, 200, "{v}");
    let id = v["id"].as_str().expect("id").to_owned();
    n.app.engine().run_until_idle().await.expect("jobs");

    // The reconnect allows one more origin; the held discovery has the old list.
    let mut body = generic(&d, "secret");
    body["allow_origins"] = json!(["https://extra.example"]);
    reconnect_during_discovery(&n, &t, &d, &id, body).await;
    let endpoint = n
        .app
        .control()
        .endpoint(id.parse().expect("id"))
        .expect("read")
        .expect("endpoint");
    assert!(
        endpoint.state["origins"]
            .as_array()
            .expect("origins")
            .contains(&json!("https://extra.example")),
        "{}",
        endpoint.state
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_discovery_held_over_a_reconnect_stores_no_reauth_over_it() {
    let n = private_node().await;
    let d = dav(DavConfig::default()).await;
    let t = tenant(&n).await;
    let (s, v) = connect(&n, &t, generic(&d, "secret")).await;
    assert_eq!(s, 200, "{v}");
    let id = v["id"].as_str().expect("id").to_owned();
    n.app.engine().run_until_idle().await.expect("jobs");

    // The password changed: the held discovery, with the old one, gets `401`
    // after the user reconnected with the new one.
    d.set_password("changed");
    reconnect_during_discovery(&n, &t, &d, &id, generic(&d, "changed")).await;
    assert_eq!(account(&n, &t, &id).await["state"], "active");
}

#[tokio::test(flavor = "multi_thread")]
async fn simultaneous_first_connects_make_one_account() {
    let n = private_node().await;
    let d = dav(DavConfig {
        latency: Duration::from_millis(20),
        ..DavConfig::default()
    })
    .await;
    // Each round a new tenant, so every connect may be the first; the
    // `deleting` guard in the handler serialises them (without it: 409).
    for _ in 0..10 {
        let t = tenant(&n).await;
        let all =
            futures_util::future::join_all((0..4).map(|_| connect(&n, &t, generic(&d, "secret"))))
                .await;
        for (s, v) in &all {
            assert_eq!((*s, &v["id"]), (200, &all[0].1["id"]), "{v}");
        }
        let accounts = n.app.control().accounts(t.id).expect("accounts");
        assert_eq!(accounts.len(), 1, "{accounts:?}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn only_post_interest_raises_the_interest_level() {
    let n = private_node().await;
    let _engine = n.app.engine().start();
    let d = dav(DavConfig::default()).await;
    let t = tenant(&n).await;
    connected(&n, &t, &d).await;
    // Wait out discovery and the first reads.
    until("a quiet server", async || {
        d.clear_requests();
        tokio::time::sleep(Duration::from_millis(500)).await;
        d.requests().is_empty().then_some(())
    })
    .await;

    let mut stream = n
        .http
        .get(n.url("/v1/stream"))
        .bearer_auth(&t.read)
        .send()
        .await
        .expect("stream");
    assert_eq!(stream.status(), 200);
    let (s, v) = n.get("/v1/changes?since=0", &t.read).await;
    assert_eq!(s, 200, "{v}");
    let (s, v) = n.get("/v1/entities?kind=task", &t.read).await;
    assert_eq!(s, 200, "{v}");
    let entity = v["entities"][0]["id"].as_str().expect("entity").to_owned();
    let (s, v) = n
        .get(
            &format!("/v1/entities/{entity}?include=placements"),
            &t.read,
        )
        .await;
    assert_eq!(s, 200, "{v}");
    let item = v["placements"][0]["item"]
        .as_str()
        .expect("item")
        .to_owned();
    let (s, v) = n.get(&format!("/v1/items/{item}"), &t.read).await;
    assert_eq!(s, 200, "{v}");
    let _ = tokio::time::timeout(Duration::from_millis(1500), stream.chunk()).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(d.requests().is_empty(), "{:?}", d.requests());

    let (s, v) = n.call("POST", "/v1/interest", Some(&t.read), None).await;
    assert_eq!(s, 204, "{v}");
    until("a poll after interest", async || {
        (!d.requests().is_empty()).then_some(())
    })
    .await;
}

/// Google account through the callback; returns its id.
async fn google_account(n: &Node, t: &Tenant) -> String {
    let (s, v) = n
        .call(
            "POST",
            "/v1/accounts",
            Some(&t.admin),
            Some(json!({ "provider": "google" })),
        )
        .await;
    assert_eq!(s, 201, "{v}");
    let consent = n
        .http
        .get(v["authorization_url"].as_str().expect("url"))
        .send()
        .await
        .expect("consent");
    let callback = consent.headers()["location"].to_str().expect("ascii");
    let (s, v) = n.call("GET", callback, None, None).await;
    assert_eq!(s, 200, "{v}");
    v["account"].as_str().expect("account").to_owned()
}

#[tokio::test(flavor = "multi_thread")]
async fn google_tasks_and_contacts_are_read() {
    let n = Node::start().await;
    let _engine = n.app.engine().start();
    let list = n.google.put_task_list("Errands");
    n.google.put_task(
        &list,
        json!({ "title": "buy milk", "status": "needsAction" }),
    );
    n.google
        .put_contact(json!({ "names": [{ "displayName": "Bob Builder" }] }));
    let t = tenant(&n).await;
    let id = google_account(&n, &t).await;

    until("the task and the contact", async || {
        let tasks = listed(&n, &t, "task").await;
        let cards = listed(&n, &t, "contact").await;
        (tasks.len() == 1 && cards.len() == 1).then_some(())
    })
    .await;
    assert!(
        listed(&n, &t, "task").await[0]
            .to_string()
            .contains("buy milk")
    );
    assert_eq!(account(&n, &t, &id).await["missing"], json!([]));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_phase_one_google_account_keeps_its_calendars_and_reports_missing() {
    let n = Node::start().await;
    let _engine = n.app.engine().start();
    // Tokens granted before phase 2: only the calendar scope.
    n.google
        .fail(Endpoint::TaskLists, GFault::scope_insufficient(), u32::MAX);
    n.google.fail(
        Endpoint::PeopleConnections,
        GFault::insufficient_permissions(),
        u32::MAX,
    );
    let now = jiff::Timestamp::now();
    let at = |h: i64| now + jiff::SignedDuration::from_hours(h);
    let when = |h: i64| json!({ "dateTime": at(h).to_string(), "timeZone": "UTC" });
    n.google.put_event(
        &n.google.primary(),
        json!({ "summary": "standup", "start": when(24), "end": when(25) }),
    );
    let t = tenant(&n).await;
    let id = google_account(&n, &t).await;

    let range = format!("/v1/entities?from={}&to={}", at(0), at(48));
    let v = until("the event and missing", async || {
        let (s, events) = n.get(&range, &t.read).await;
        assert_eq!(s, 200, "{events}");
        let v = account(&n, &t, &id).await;
        let has_event = events["occurrences"]
            .as_array()
            .is_some_and(|o| o.len() == 1);
        (has_event && v["state"] == "active" && v["missing"] != json!([])).then_some(v)
    })
    .await;
    let missing = v["missing"].as_array().expect("missing");
    assert!(missing.contains(&json!("task")), "{v}");
    assert!(missing.contains(&json!("contact")), "{v}");
    assert_eq!(
        v["reconsent"],
        json!({ "method": "POST", "path": "/v1/accounts",
                "body": { "provider": "google", "account": id } }),
    );
    assert!(listed(&n, &t, "task").await.is_empty());
}

/// A Google account whose stored access token has expired, with `refresh`
/// as its refresh token; every job of the connect has run.
async fn expired_google_account(n: &Node, t: &Tenant, refresh: &str) -> AccountId {
    let id: AccountId = google_account(n, t).await.parse().expect("id");
    n.app.engine().run_until_idle().await.expect("jobs");
    let expired = AccountTokens {
        refresh: SecretString::from(refresh),
        access: SecretString::from("expired"),
        access_until: jiff::Timestamp::now(),
    };
    n.app
        .vault()
        .store(id, &Credentials::OAuth(expired))
        .expect("store");
    id
}

/// Holds the token refresh of a read of `id` while the OAuth callback
/// reconnects the account, then runs every job left. Returns the tokens the
/// reconnect stored.
async fn reconnect_during_refresh(n: &Node, t: &Tenant, id: AccountId) -> AccountTokens {
    let refreshes = || {
        n.google
            .requests()
            .iter()
            .filter(|r| r.starts_with("POST /token"))
            .count()
    };
    let before = refreshes();
    n.google.set_latency(HOLD);
    let (s, v) = n.call("POST", "/v1/interest", Some(&t.read), None).await;
    assert_eq!(s, 204, "{v}");
    let app = n.app.clone();
    let run = tokio::spawn(async move { app.engine().run_until_idle().await });
    until("the held refresh", async || {
        (refreshes() > before).then_some(())
    })
    .await;
    let held = Instant::now();
    n.google.set_latency(Duration::ZERO);

    let (s, v) = n
        .call(
            "POST",
            "/v1/accounts",
            Some(&t.admin),
            Some(json!({ "provider": "google", "account": id })),
        )
        .await;
    assert_eq!(s, 201, "{v}");
    let consent = n
        .http
        .get(v["authorization_url"].as_str().expect("url"))
        .send()
        .await
        .expect("consent");
    let callback = consent.headers()["location"].to_str().expect("ascii");
    let (s, v) = n.call("GET", callback, None, None).await;
    assert_eq!((s, &v["account"]), (200, &json!(id)), "{v}");
    let stored = tokens(n, id);
    assert!(
        held.elapsed() < HOLD / 2,
        "the reconnect ended after the hold"
    );
    run.await.expect("join").expect("jobs");
    stored
}

fn tokens(n: &Node, id: AccountId) -> AccountTokens {
    match n.app.vault().load(id).expect("load") {
        Some(Credentials::OAuth(t)) => t,
        other => panic!("{other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_read_held_over_a_reconnect_stores_no_tokens_over_it() {
    let n = Node::start().await;
    let t = tenant(&n).await;
    let id = expired_google_account(&n, &t, &n.google.refresh_token()).await;

    let stored = reconnect_during_refresh(&n, &t, id).await;
    let now = tokens(&n, id);
    assert_eq!(
        (now.access.expose_secret(), now.access_until),
        (stored.access.expose_secret(), stored.access_until)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_read_held_over_a_reconnect_stores_no_reauth_over_it() {
    let n = Node::start().await;
    let t = tenant(&n).await;
    // The held refresh gets `invalid_grant` after the user signed in again.
    let id = expired_google_account(&n, &t, "1//revoked").await;

    reconnect_during_refresh(&n, &t, id).await;
    assert_eq!(account(&n, &t, &id.to_string()).await["state"], "active");
}

#[tokio::test(flavor = "multi_thread")]
async fn passwords_reach_no_response_and_no_trace_log() {
    passwords_stay_secret(Profile::Generic).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn icloud_passwords_reach_no_response_and_no_trace_log() {
    passwords_stay_secret(Profile::Icloud).await;
}

async fn passwords_stay_secret(profile: Profile) {
    use base64::Engine as _;
    use musubi_testkit::google::FakeGoogle;
    use musubi_testkit::harness::{self, Api, Daemon};

    let (first, wrong, second) = ("first-Pa55word", "wrong-Pa55word", "second-Pa55word");
    let config = DavConfig {
        password: first.into(),
        ..DavConfig::default()
    };
    let d = match profile {
        Profile::Generic => dav(config).await,
        Profile::Icloud => apple(config).await,
    };
    let g = FakeGoogle::start(Default::default()).await.expect("google");
    let dir = tempfile::tempdir().expect("tempdir");
    let listen = harness::free_addr();
    let mut config = harness::config(listen, &dir.path().join("data"), &g);
    config["dav"] = match profile {
        Profile::Generic => json!({ "allow_private": true }),
        Profile::Icloud => fake_icloud(&d),
    };
    let path = dir.path().join("config.json");
    std::fs::write(&path, config.to_string()).expect("config");
    let daemon = Daemon::serve(
        env!("CARGO_BIN_EXE_musubi-syncd").as_ref(),
        &path,
        &[("MUSUBI_LOG", "trace".into())],
    );
    let api = Api::new(listen);
    let tenant = api.tenant().await;
    let (admin, read) = (
        api.key(&tenant, "admin").await,
        api.key(&tenant, "read").await,
    );
    let mut responses = Vec::new();
    let mut call = async |method: &str, path: &str, key: &str, body: Option<Value>| {
        let (s, v) = api.call(method, path, Some(key), body).await.expect("call");
        responses.push(v.to_string());
        (s, v)
    };
    let body = |password: &str, account: Option<&str>| {
        let mut b = match profile {
            Profile::Generic => generic(&d, password),
            Profile::Icloud => icloud(password),
        };
        b["account"] = json!(account);
        b
    };

    let (s, v) = call("POST", "/v1/accounts/dav", &admin, Some(body(wrong, None))).await;
    assert_eq!(s, 400, "{v}");
    let (s, v) = call("POST", "/v1/accounts/dav", &admin, Some(body(first, None))).await;
    assert_eq!(s, 200, "{v}");
    let id = v["id"].as_str().expect("id").to_owned();
    let account = format!("/v1/accounts/{id}");
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let (_, v) = call("GET", "/v1/entities?kind=contact", &read, None).await;
        if v["entities"].as_array().is_some_and(|e| e.len() == 1) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "no contact:\n{}",
            daemon.tail(40)
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // The password changes at the server: sync stops, the user enters the new one.
    d.set_password(second);
    call("POST", "/v1/interest", &read, None).await;
    loop {
        let (_, v) = call("GET", &account, &read, None).await;
        if v["state"] == "reauth_required" {
            break;
        }
        assert!(Instant::now() < deadline, "no reauth:\n{}", daemon.tail(40));
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let (s, v) = call(
        "POST",
        "/v1/accounts/dav",
        &admin,
        Some(body(second, Some(&id))),
    )
    .await;
    assert_eq!((s, &v["state"]), (200, &json!("active")), "{v}");
    call("GET", &account, &read, None).await;

    let log = daemon.tail(usize::MAX);
    assert!(log.contains("TRACE"), "the log is at trace level");
    for pw in [first, wrong, second] {
        let basic = base64::engine::general_purpose::STANDARD.encode(format!("alice:{pw}"));
        for secret in [pw.to_owned(), basic] {
            assert!(!log.contains(&secret), "{secret} in the log");
            for r in &responses {
                assert!(!r.contains(&secret), "{secret} in {r}");
            }
        }
    }
}

/// The entries `POST /v1/accounts/dav` stores: `:443` and host case are not
/// part of the origin (principals pass the same normalisation in musubi-dav).
#[test]
fn an_explicit_default_port_is_the_same_origin() {
    let endpoint = |url: &str| {
        musubi_dav::DavSetup {
            server_url: Some(url.into()),
            ..Default::default()
        }
        .endpoint()
        .expect("endpoint")
    };
    let plain = endpoint("https://dav.example/remote/");
    assert_eq!(endpoint("https://DAV.example:443/remote/"), plain);
    assert_eq!(plain.state["origins"], json!(["https://dav.example"]));
    assert_ne!(endpoint("https://dav.example:8443/remote/"), plain);
}

// Profile `icloud` (plan 5.4) against the fake DAV in its `icloud` mode. The
// fake listens on `http://127.0.0.1`, so the node points profile `icloud` at
// it through the test-only `dav.icloud_test_entries`, with the fake's
// partition hosts as allowed origins (as musubi-dav's K7 tests do).

const EVENT: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//musubi//test//EN\r\nBEGIN:VEVENT\r\nUID:event-1\r\nDTSTAMP:20261101T000000Z\r\nDTSTART:20261113T100000Z\r\nDTEND:20261113T110000Z\r\nSUMMARY:standup\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
const CARD3: &str = "BEGIN:VCARD\r\nVERSION:3.0\r\nUID:card-1\r\nFN:Bob Builder\r\nN:Builder;Bob;;;\r\nEND:VCARD\r\n";
/// An Apple contact group: vCard 3.0 with `X-ADDRESSBOOKSERVER-KIND:group`.
const GROUP3: &str = "BEGIN:VCARD\r\nVERSION:3.0\r\nUID:group-1\r\nFN:Family\r\nN:Family;;;;\r\nX-ADDRESSBOOKSERVER-KIND:group\r\nX-ADDRESSBOOKSERVER-MEMBER:urn:uuid:card-1\r\nEND:VCARD\r\n";
const RANGE: &str = "/v1/entities?from=2026-11-01T00:00:00Z&to=2026-12-01T00:00:00Z";

/// A fake iCloud: an event calendar, a reminders list (VTODO only, not
/// synced) and an address book with a contact and a group.
async fn apple(config: DavConfig) -> FakeDav {
    let d = FakeDav::start(DavConfig {
        profile: Profile::Icloud,
        ..config
    })
    .await
    .expect("fake dav");
    let cal = d.add_collection(Kind::Calendar(&["VEVENT"]), "home", "Home");
    d.put(&cal, "event-1.ics", EVENT);
    let reminders = d.add_collection(Kind::Calendar(&["VTODO"]), "tasks", "Reminders");
    d.put(&reminders, "todo-1.ics", TODO);
    let book = d.add_collection(Kind::AddressBook, "card", "Contacts");
    d.put(&book, "card-1.vcf", CARD3);
    d.put(&book, "group-1.vcf", GROUP3);
    d
}

/// The node's `dav` settings pointing profile `icloud` at `d`.
fn fake_icloud(d: &FakeDav) -> Value {
    json!({
        "allow_private": true,
        "icloud_test_entries": {
            "caldav": d.server_url(), "carddav": d.carddav_url(),
            "allow_origins": [d.url(Host::CaldavPartition), d.url(Host::CarddavPartition)],
        },
    })
}

async fn icloud_node(d: &FakeDav) -> Node {
    Node::start_with(|c| c["dav"] = fake_icloud(d)).await
}

/// The Apple ID and the app-specific password: all profile `icloud` takes.
fn icloud(password: &str) -> Value {
    json!({ "profile": "icloud", "username": "alice", "password": password })
}

async fn first_item(n: &Node, t: &Tenant, entity: &str) -> Value {
    let (s, v) = n
        .get(
            &format!("/v1/entities/{entity}?include=placements"),
            &t.read,
        )
        .await;
    assert_eq!(s, 200, "{v}");
    let item = v["placements"][0]["item"]
        .as_str()
        .expect("item")
        .to_owned();
    let (s, v) = n.get(&format!("/v1/items/{item}"), &t.read).await;
    assert_eq!(s, 200, "{v}");
    v
}

#[tokio::test(flavor = "multi_thread")]
async fn an_icloud_account_connects_with_only_the_apple_id_and_reads_its_data() {
    let d = apple(DavConfig::default()).await;
    let n = icloud_node(&d).await;
    let _engine = n.app.engine().start();
    let t = tenant(&n).await;

    let (s, v) = connect(&n, &t, icloud("secret")).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(
        (&v["provider"], &v["profile"]),
        (&json!("icloud"), &json!("icloud")),
        "{v}"
    );
    assert_eq!(v["username"], "alice", "{v}");
    // The entries and the confirmed partition hosts.
    let mut origins = [
        Host::Caldav,
        Host::Carddav,
        Host::CaldavPartition,
        Host::CarddavPartition,
    ]
    .map(|h| d.url(h));
    origins.sort();
    assert_eq!(v["origins"], json!(origins), "{v}");
    assert!(v.get("hint").is_none(), "{v}");
    assert!(!v.to_string().contains("secret"), "{v}");
    let id = v["id"].as_str().expect("id").to_owned();

    let (events, cards, groups) = until("the event, the contact and the group", async || {
        let (s, events) = n.get(RANGE, &t.read).await;
        assert_eq!(s, 200, "{events}");
        let events = events["occurrences"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let cards = listed(&n, &t, "contact").await;
        let groups = listed(&n, &t, "contact_group").await;
        (events.len() == 1 && cards.len() == 1 && groups.len() == 1)
            .then_some((events, cards, groups))
    })
    .await;
    assert!(
        events[0]["canonical"].to_string().contains("standup"),
        "{events:?}"
    );
    assert!(cards[0].to_string().contains("Bob Builder"), "{cards:?}");
    assert_eq!(groups[0]["name"], "Family", "{groups:?}");
    // iCloud's reminders are not in v1: no task collection, no tasks.
    assert!(listed(&n, &t, "task").await.is_empty());
    assert_eq!(collections(&n, &t).await.len(), 2);

    let entity = |v: &Value| v["entity"].as_str().expect("entity").to_owned();
    let item = first_item(&n, &t, &entity(&events[0])).await;
    assert_eq!(item["kind"], "event", "{item}");
    let (s, v) = n.get("/v1/entities?kind=contact", &t.read).await;
    assert_eq!(s, 200, "{v}");
    let card = v["entities"][0]["id"].as_str().expect("id").to_owned();
    let item = first_item(&n, &t, &card).await;
    assert_eq!(item["kind"], "contact", "{item}");
    let (s, v) = n.get("/v1/entities?kind=contact_group", &t.read).await;
    assert_eq!(s, 200, "{v}");
    let group = v["entities"][0]["id"].as_str().expect("id").to_owned();
    let item = first_item(&n, &t, &group).await;
    assert_eq!(item["kind"], "contact_group", "{item}");

    // The stored account is the `icloud` instance's.
    let rec = n
        .app
        .control()
        .account(id.parse().expect("account id"))
        .expect("read")
        .expect("account");
    assert_eq!(rec.provider, ProviderId("icloud".into()));
}

#[tokio::test(flavor = "multi_thread")]
async fn an_icloud_connect_refuses_addresses_and_wrong_passwords_and_stores_nothing() {
    let d = apple(DavConfig::default()).await;
    let n = icloud_node(&d).await;
    let t = tenant(&n).await;
    d.clear_requests();

    for (field, value) in [
        ("server_url", json!(d.server_url())),
        ("caldav_url", json!(d.server_url())),
        ("carddav_url", json!(d.carddav_url())),
        ("allow_origins", json!([d.url(Host::Outside)])),
    ] {
        let mut body = icloud("secret");
        body[field] = value;
        let (s, v) = connect(&n, &t, body).await;
        assert_eq!(
            (s, &v["error"]),
            (400, &json!("bad_request")),
            "{field}: {v}"
        );
    }
    assert!(d.requests().is_empty(), "{:?}", d.requests());

    let (s, v) = connect(&n, &t, icloud("wrong")).await;
    assert_eq!((s, &v["error"]), (400, &json!("unauthorized")), "{v}");
    let hint = musubi_dav::Profile::Icloud.reauth_hint().expect("hint");
    assert_eq!(v["hint"], hint, "{v}");
    assert!(!v.to_string().contains("wrong"), "{v}");
    assert!(collections(&n, &t).await.is_empty());
    let subject = format!("{}{}", d.url(Host::Caldav), d.principal());
    let acc = n
        .app
        .control()
        .account_by_subject(t.id, &ProviderId("icloud".into()), &subject)
        .expect("read");
    assert!(acc.is_none(), "{acc:?}");

    // A generic account answers `unauthorized` without iCloud's hint.
    let g = dav(DavConfig::default()).await;
    let (s, v) = connect(&n, &t, generic(&g, "wrong")).await;
    assert_eq!((s, &v["error"]), (400, &json!("unauthorized")), "{v}");
    assert!(v.get("hint").is_none(), "{v}");

    // A redirect off iCloud names the target but asks for no `allow_origins`,
    // which profile icloud refuses.
    d.fail(
        Req::Any,
        Fault::Redirect {
            status: 301,
            to: Host::Outside,
        },
        1,
    );
    let (s, v) = connect(&n, &t, icloud("secret")).await;
    assert_eq!(
        (s, &v["error"]),
        (400, &json!("cross_origin_redirect")),
        "{v}"
    );
    let message = v["message"].as_str().expect("message");
    assert!(message.contains(&d.url(Host::Outside)), "{v}");
    assert!(!message.contains("allow_origins"), "{v}");
    assert!(v.get("hint").is_none(), "{v}");
    assert_eq!(outside(&d), 0);
    assert!(collections(&n, &t).await.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rejected_icloud_password_asks_for_a_new_app_specific_password() {
    let d = apple(DavConfig::default()).await;
    let n = icloud_node(&d).await;
    let _engine = n.app.engine().start();
    let t = tenant(&n).await;
    let (s, v) = connect(&n, &t, icloud("secret")).await;
    assert_eq!(s, 200, "{v}");
    let id = v["id"].as_str().expect("id").to_owned();
    until("the contact", async || {
        (listed(&n, &t, "contact").await.len() == 1).then_some(())
    })
    .await;

    // The user changed the Apple ID password: every app-specific one is void.
    d.set_password("changed");
    let (s, v) = n.call("POST", "/v1/interest", Some(&t.read), None).await;
    assert_eq!(s, 204, "{v}");
    // The engine marks the account before it records the reason.
    let v = until("rejected", async || {
        let rejected = collections(&n, &t)
            .await
            .iter()
            .all(|c| c["error"]["message"] == "rejected");
        let v = account(&n, &t, &id).await;
        (rejected && v["state"] == "reauth_required").then_some(v)
    })
    .await;
    let hint = musubi_dav::Profile::Icloud.reauth_hint().expect("hint");
    assert_eq!(v["hint"], hint, "{v}");

    // A new app-specific password: active again, no hint.
    let mut body = icloud("changed");
    body["account"] = json!(id);
    let (s, v) = connect(&n, &t, body).await;
    assert_eq!((s, &v["state"]), (200, &json!("active")), "{v}");
    assert!(v.get("hint").is_none(), "{v}");

    // A redirect off iCloud is not about the password: no hint.
    d.fail(
        Req::Any,
        Fault::Redirect {
            status: 307,
            to: Host::Outside,
        },
        1,
    );
    let (s, v) = n.call("POST", "/v1/interest", Some(&t.read), None).await;
    assert_eq!(s, 204, "{v}");
    // The engine marks the account before the collections.
    let v = until("endpoint_moved", async || {
        let moved = collections(&n, &t)
            .await
            .iter()
            .any(|c| c["error"]["message"] == "endpoint_moved");
        let v = account(&n, &t, &id).await;
        (moved && v["state"] == "reauth_required").then_some(v)
    })
    .await;
    assert!(v.get("hint").is_none(), "{v}");
}

/// A reconnect names an account of its own instance: a `dav` account under
/// profile `icloud`, or an `icloud` one under `generic`, is not found.
#[tokio::test(flavor = "multi_thread")]
async fn a_reconnect_under_the_other_profile_is_not_found_and_changes_nothing() {
    let a = apple(DavConfig::default()).await;
    let g = dav(DavConfig::default()).await;
    let n = icloud_node(&a).await;
    let _engine = n.app.engine().start();
    let t = tenant(&n).await;
    let mut ids = Vec::new();
    for body in [generic(&g, "secret"), icloud("secret")] {
        let (s, v) = connect(&n, &t, body).await;
        assert_eq!(s, 200, "{v}");
        ids.push(v["id"].as_str().expect("id").to_owned());
    }
    let (dav_id, icloud_id) = (&ids[0], &ids[1]);
    let mut before = Vec::new();
    for id in &ids {
        before.push(
            until("active", async || {
                let v = account(&n, &t, id).await;
                (v["state"] == "active").then_some(v)
            })
            .await,
        );
    }
    let cols = collections(&n, &t).await.len();
    let seen = (a.requests().len(), g.requests().len());

    let mut on_icloud = icloud("secret");
    on_icloud["account"] = json!(dav_id);
    let mut on_generic = generic(&g, "secret");
    on_generic["account"] = json!(icloud_id);
    for body in [on_icloud, on_generic] {
        let (s, v) = connect(&n, &t, body.clone()).await;
        assert_eq!((s, &v["error"]), (404, &json!("not_found")), "{body}: {v}");
    }
    assert_eq!((a.requests().len(), g.requests().len()), seen);
    for (id, b) in ids.iter().zip(&before) {
        assert_eq!(&account(&n, &t, id).await, b);
    }
    assert_eq!(collections(&n, &t).await.len(), cols);
    let accounts = n.app.control().accounts(t.id).expect("accounts");
    assert_eq!(accounts.len(), 2, "{accounts:?}");
}

/// Another DSID: the node is restarted with its iCloud entries at another
/// fake account (another server, so the principal differs in its origin
/// too), and the reconnect is refused.
#[tokio::test(flavor = "multi_thread")]
async fn an_icloud_reconnect_to_another_dsid_is_refused_and_changes_nothing() {
    use musubi_testkit::google::FakeGoogle;
    use musubi_testkit::harness::{self, Api, Daemon};

    let a = apple(DavConfig::default()).await;
    let b = apple(DavConfig::default()).await;
    let g = FakeGoogle::start(Default::default()).await.expect("google");
    let dir = tempfile::tempdir().expect("tempdir");
    let listen = harness::free_addr();
    let path = dir.path().join("config.json");
    let serve = |d: &FakeDav| {
        let mut config = harness::config(listen, &dir.path().join("data"), &g);
        config["dav"] = fake_icloud(d);
        std::fs::write(&path, config.to_string()).expect("config");
        let daemon = Daemon::serve(env!("CARGO_BIN_EXE_musubi-syncd").as_ref(), &path, &[]);
        assert!(
            daemon
                .wait_line("dav.icloud_test_entries is set", Duration::from_secs(5))
                .is_some(),
            "no startup warning:\n{}",
            daemon.tail(40)
        );
        daemon
    };
    let api = Api::new(listen);
    let call = async |method: &str, path: &str, key: &str, body: Option<Value>| {
        api.call(method, path, Some(key), body).await.expect("call")
    };

    let mut daemon = serve(&a);
    let tenant = api.tenant().await;
    let (admin, read) = (
        api.key(&tenant, "admin").await,
        api.key(&tenant, "read").await,
    );
    let (s, v) = call("POST", "/v1/accounts/dav", &admin, Some(icloud("secret"))).await;
    assert_eq!(s, 200, "{v}");
    let id = v["id"].as_str().expect("id").to_owned();
    let account = format!("/v1/accounts/{id}");
    let before = until("active", async || {
        let (_, v) = call("GET", &account, &read, None).await;
        (v["state"] == "active").then_some(v)
    })
    .await;
    daemon.kill();

    let _daemon = serve(&b);
    let mut body = icloud("secret");
    body["account"] = json!(id);
    let (s, v) = call("POST", "/v1/accounts/dav", &admin, Some(body)).await;
    assert_eq!((s, &v["error"]), (400, &json!("subject_mismatch")), "{v}");
    let (_, after) = call("GET", &account, &read, None).await;
    assert_eq!(after, before);
}

#[tokio::test]
async fn icloud_test_entries_need_allow_private() {
    let g = musubi_testkit::google::FakeGoogle::start(Default::default())
        .await
        .expect("google");
    let dir = tempfile::tempdir().expect("tempdir");
    let mut c = common::config_json("127.0.0.1:1", &dir.path().join("data"), &g);
    c["dav"] = json!({ "icloud_test_entries": { "caldav": "http://127.0.0.1:1/", "carddav": "http://127.0.0.1:2/" } });
    let config: musubi_syncd::config::Config = serde_json::from_value(c).expect("config");
    let e = musubi_syncd::App::open(&config).err().expect("refused");
    assert!(e.contains("dav.allow_private"), "{e}");
}
