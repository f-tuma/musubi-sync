//! One Google account end to end: OAuth, the engine and its push channel
//! against the fake Google server (K6 step 2). The fake delivers its webhook
//! notifications to the node's own listener; nothing leaves the machine.

mod common;

use std::time::{Duration, Instant};

use common::{ADMIN_KEY, Node, next_event};
use jiff::{SignedDuration, Timestamp};
use musubi_model::control::AccountState;
use musubi_model::{AccountId, ControlStore, TokenVault};
use serde_json::{Value, json};

/// Polls `f` until it gives a value.
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

/// Occurrence starts of a `GET /v1/entities` body.
fn starts(v: &Value) -> Vec<Timestamp> {
    let mut s: Vec<Timestamp> = v["occurrences"]
        .as_array()
        .expect("occurrences")
        .iter()
        .map(|o| o["start"].as_str().expect("start").parse().expect("ts"))
        .collect();
    s.sort();
    s
}

#[tokio::test(flavor = "multi_thread")]
async fn google_account_syncs_end_to_end_and_removal_stops_its_channel() {
    let n = Node::start().await;
    let _engine = n.app.engine().start();
    let now = Timestamp::from_second(Timestamp::now().as_second()).expect("now");
    let at = |h: i64| now + SignedDuration::from_hours(h);
    let when = |h: i64| json!({ "dateTime": at(h).to_string(), "timeZone": "UTC" });
    let cal = n.google.primary();
    let single = n.google.put_event(
        &cal,
        json!({ "summary": "single", "start": when(24), "end": when(25) }),
    );
    n.google.put_event(
        &cal,
        json!({
            "summary": "weekly",
            "start": when(48),
            "end": when(49),
            "recurrence": ["RRULE:FREQ=WEEKLY"],
        }),
    );

    // Connect: consent, callback, then the engine discovers and reads the window.
    let t = n.tenant().await;
    let (admin, _) = n.key(t, "admin").await;
    let (read, _) = n.key(t, "read").await;
    let (s, v) = n
        .call(
            "POST",
            "/v1/accounts",
            Some(&admin),
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
    assert_eq!((s, &v["state"]), (200, &json!("connecting")), "{v}");
    let account: AccountId = v["account"].as_str().expect("id").parse().expect("id");

    let range = async |from: i64, to: i64| {
        let (s, v) = n
            .get(
                &format!("/v1/entities?from={}&to={}", at(from), at(to)),
                &read,
            )
            .await;
        assert_eq!(s, 200, "{v}");
        v
    };
    until("the window read and the channel", async || {
        let v = range(0, 72).await;
        (starts(&v) == [at(24), at(48)] && n.google.channels().len() == 1).then_some(())
    })
    .await;
    let rec = n
        .app
        .control()
        .account(account)
        .expect("read")
        .expect("account");
    assert_eq!(rec.state, AccountState::Active);
    // Past the index horizon (5 years) the series is listed, not expanded.
    let far = range(24 * 2000, 24 * 2100).await;
    assert!(starts(&far).is_empty(), "{far}");
    let unindexed = far["unindexed"].as_array().expect("unindexed");
    assert_eq!(unindexed.len(), 1, "{far}");
    assert!(unindexed[0]["indexed_until"].is_string(), "{far}");

    // The stream's cursor, then a change at Google arrives by push.
    let mut stream = n
        .http
        .get(n.url("/v1/stream"))
        .bearer_auth(&read)
        .send()
        .await
        .expect("stream");
    let mut buf = String::new();
    let wait = Duration::from_secs(10);
    let head = next_event(&mut stream, &mut buf, wait).await.expect("head")["cursor"]
        .as_u64()
        .expect("cursor");
    n.google.put_event(
        &cal,
        json!({ "id": single, "summary": "moved", "start": when(30), "end": when(31) }),
    );
    until("the delta", async || {
        (starts(&range(0, 72).await) == [at(30), at(48)]).then_some(())
    })
    .await;
    assert!(
        n.google
            .deliveries()
            .iter()
            .any(|d| d.state == "exists" && d.result == Ok(200)),
        "the fake's notification reached the webhook: {:?}",
        n.google.deliveries()
    );
    let moved = until("the stream event", async || {
        let c = next_event(&mut stream, &mut buf, wait).await?["cursor"].as_u64()?;
        (c > head).then_some(c)
    })
    .await;
    let (s, v) = n.get(&format!("/v1/changes?since={head}"), &read).await;
    assert_eq!((s, &v["next"]), (200, &json!(moved)), "{v}");

    // Deleting the tenant stops the channel at Google (which needs the
    // account's tokens) before the tokens go.
    let (s, v) = n
        .call(
            "DELETE",
            &format!("/v1/admin/tenants/{t}"),
            Some(ADMIN_KEY),
            None,
        )
        .await;
    assert_eq!(s, 204, "{v}");
    assert!(n.google.channels().is_empty(), "channel still live");
    assert!(
        n.google
            .requests()
            .iter()
            .any(|r| r.starts_with("POST /calendar/v3/channels/stop")),
        "{:?}",
        n.google.requests()
    );
    assert!(n.app.control().account(account).expect("read").is_none());
    assert!(n.app.vault().load(account).expect("load").is_none());
    // The tenant's stream ends with its commit channel.
    tokio::time::timeout(wait, async {
        while stream.chunk().await.expect("chunk").is_some() {}
    })
    .await
    .expect("the stream ends");
}

/// Starts a sign-in and consents at the fake; returns the callback URL.
async fn consent(n: &Node, admin: &str) -> String {
    let (s, v) = n
        .call(
            "POST",
            "/v1/accounts",
            Some(admin),
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
    consent.headers()["location"]
        .to_str()
        .expect("ascii")
        .to_owned()
}

#[tokio::test(flavor = "multi_thread")]
async fn tenant_deletion_outlives_its_request_and_refuses_new_sign_ins() {
    let n = Node::start().await;
    let _engine = n.app.engine().start();
    n.google
        .apply(&musubi_testkit::google::Change::AddCalendar {
            entry: json!({ "id": "second" }),
        });
    let t = n.tenant().await;
    let (admin, _) = n.key(t, "admin").await;
    let callback = consent(&n, &admin).await;
    let (s, v) = n.call("GET", &callback, None, None).await;
    assert_eq!(s, 200, "{v}");
    until("both channels", async || {
        (n.google.channels().len() == 2).then_some(())
    })
    .await;
    let late = consent(&n, &admin).await;

    // Each Google call now takes a second: the deletion stops two channels
    // one after the other, the late callback redeems its code meanwhile.
    n.google.set_latency(Duration::from_secs(1));
    let started = Instant::now();
    let gave_up = n
        .http
        .delete(n.url(&format!("/v1/admin/tenants/{t}")))
        .bearer_auth(ADMIN_KEY)
        .timeout(Duration::from_millis(300))
        .send()
        .await;
    assert!(gave_up.is_err(), "the client gives up first: {gave_up:?}");
    let (s, v) = n.call("GET", &late, None, None).await;
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the callback finished during the deletion"
    );
    assert_eq!(s, 404, "a tenant being deleted gets no account: {v}");

    until("the deletion", async || {
        let (s, _) = n
            .call(
                "GET",
                &format!("/v1/admin/tenants/{t}/stats"),
                Some(ADMIN_KEY),
                None,
            )
            .await;
        (s == 404).then_some(())
    })
    .await;
    assert!(n.google.channels().is_empty(), "channels still live");
    assert!(n.app.control().accounts(t).expect("accounts").is_empty());
}
