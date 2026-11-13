//! DAV accounts through the API against the fake DAV server (plan K8):
//! connect, its 400 reasons, reconnect, `POST /v1/interest`, and the Google
//! kinds the registry now reads. Nothing leaves the machine.

mod common;

use std::time::{Duration, Instant};

use common::Node;
use musubi_model::capability::ProviderId;
use musubi_model::{ControlStore, TenantId};
use musubi_testkit::dav::{Config as DavConfig, FakeDav, Fault, Host, Kind, Req};
use musubi_testkit::google::{Endpoint, Fault as GFault};
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
    assert!(
        errors.iter().any(|e| e.contains("EndpointMoved")),
        "{errors:?}"
    );
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

#[tokio::test(flavor = "multi_thread")]
async fn passwords_reach_no_response_and_no_trace_log() {
    use base64::Engine as _;
    use musubi_testkit::google::FakeGoogle;
    use musubi_testkit::harness::{self, Api, Daemon};

    let (first, wrong, second) = ("first-Pa55word", "wrong-Pa55word", "second-Pa55word");
    let d = dav(DavConfig {
        password: first.into(),
        ..DavConfig::default()
    })
    .await;
    let g = FakeGoogle::start(Default::default()).await.expect("google");
    let dir = tempfile::tempdir().expect("tempdir");
    let listen = harness::free_addr();
    let mut config = harness::config(listen, &dir.path().join("data"), &g);
    config["dav"] = json!({ "allow_private": true });
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
        json!({
            "profile": "generic", "server_url": d.server_url(),
            "username": "alice", "password": password, "account": account,
        })
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
