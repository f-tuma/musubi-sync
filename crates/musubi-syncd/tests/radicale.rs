//! The whole musubi-syncd binary with a DAV account on the pinned Radicale
//! (plan K8): connect, first data in the API, and a change at Radicale in the
//! change feed within two active-level intervals, and a recurring task in
//! the range (K12d). Needs `MUSUBI_RADICALE`;
//! run by the CI job `conformance`.

mod common;

use std::time::{Duration, Instant};

use common::tasks;
use musubi_testkit::google::FakeGoogle;
use musubi_testkit::harness::{self, Api, Daemon, Sse};
use musubi_testkit::radicale::{PASSWORD, Radicale, USER};
use serde_json::{Value, json};

fn card(uid: &str, name: &str) -> String {
    format!("BEGIN:VCARD\r\nVERSION:3.0\r\nUID:{uid}\r\nFN:{name}\r\nN:{name};;;;\r\nEND:VCARD\r\n")
}

const TODO: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//musubi//test//EN\r\nBEGIN:VTODO\r\nUID:todo-1\r\nDTSTAMP:20261101T000000Z\r\nSUMMARY:buy milk\r\nEND:VTODO\r\nEND:VCALENDAR\r\n";

/// Plan 7.3: the active level polls at most every 60 s.
const ACTIVE_MAX: Duration = Duration::from_secs(60);

async fn ok(api: &Api, method: &str, path: &str, key: &str, body: Option<Value>) -> Value {
    let (s, v) = api
        .call(method, path, Some(key), body)
        .await
        .expect("request");
    assert!((200..300).contains(&s), "{method} {path}: {s} {v}");
    v
}

async fn names(api: &Api, read: &str) -> Vec<String> {
    let v = ok(api, "GET", "/v1/entities?kind=contact", read, None).await;
    v["entities"]
        .as_array()
        .expect("entities")
        .iter()
        .map(|e| e["canonical"].to_string())
        .collect()
}

/// The binary with a tenant whose DAV account is on `r`, and the tenant's
/// stream opened before the account.
struct Node {
    // Field order is drop order: the daemon dies before its directory goes.
    sse: Sse,
    daemon: Daemon,
    api: Api,
    read: String,
    _google: FakeGoogle,
    _dir: tempfile::TempDir,
}

async fn connected(r: &Radicale) -> Node {
    let g = FakeGoogle::start(Default::default()).await.expect("google");
    let dir = tempfile::tempdir().expect("tempdir");
    let listen = harness::free_addr();
    let mut config = harness::config(listen, &dir.path().join("data"), &g);
    config["dav"] = json!({ "allow_private": true });
    let path = dir.path().join("config.json");
    std::fs::write(&path, config.to_string()).expect("config");
    let daemon = Daemon::serve(env!("CARGO_BIN_EXE_musubi-syncd").as_ref(), &path, &[]);
    let api = Api::new(listen);
    let tenant = api.tenant().await;
    let (admin, read) = (
        api.key(&tenant, "admin").await,
        api.key(&tenant, "read").await,
    );
    let sse = Sse::open(&api, &read).await;

    let v = ok(
        &api,
        "POST",
        "/v1/accounts/dav",
        &admin,
        Some(json!({
            "profile": "generic", "server_url": r.url(),
            "username": USER, "password": PASSWORD,
        })),
    )
    .await;
    assert_eq!(v["username"], USER, "{v}");
    Node {
        sse,
        daemon,
        api,
        read,
        _google: g,
        _dir: dir,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_radicale_account_syncs_through_the_binary() {
    let r = Radicale::start().await;
    r.put(&r.calendar(), "todo-1.ics", TODO).await;
    r.put(&r.addressbook(), "bob.vcf", &card("bob", "Bob Builder"))
        .await;
    let n = connected(&r).await;
    let (daemon, api, read) = (&n.daemon, &n.api, n.read.as_str());

    // First data.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let tasks = ok(api, "GET", "/v1/entities?kind=task", read, None).await;
        let found = tasks["entities"].as_array().is_some_and(|t| t.len() == 1)
            && names(api, read).await.len() == 1;
        if found {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "no first data:\n{}",
            daemon.tail(40)
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let mut since = 0;
    loop {
        let page = ok(
            api,
            "GET",
            &format!("/v1/changes?since={since}"),
            read,
            None,
        )
        .await;
        since = page["next"].as_u64().expect("next");
        if page["more"] == false {
            break;
        }
    }

    // The user looks at the data: `touch` polls at once, then the active
    // level keeps polling; each change reaches the feed within two intervals.
    ok(api, "POST", "/v1/interest", read, None).await;
    for (uid, name) in [("carol", "Carol Singer"), ("dave", "Dave Diver")] {
        let start = Instant::now();
        r.put(&r.addressbook(), &format!("{uid}.vcf"), &card(uid, name))
            .await;
        loop {
            let page = ok(
                api,
                "GET",
                &format!("/v1/changes?since={since}"),
                read,
                None,
            )
            .await;
            since = page["next"].as_u64().expect("next");
            if !page["changes"].as_array().expect("changes").is_empty() {
                break;
            }
            assert!(
                start.elapsed() < 2 * ACTIVE_MAX,
                "{name}: no change within two active intervals:\n{}",
                daemon.tail(40)
            );
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        eprintln!("{name} in the feed after {:?}", start.elapsed());
    }
    assert!(
        names(api, read)
            .await
            .iter()
            .any(|n| n.contains("Dave Diver")),
        "{:?}",
        names(api, read).await
    );
}

/// Plan 12.7 and K12d: a recurring VTODO from Radicale is in the range with
/// its occurrences, the override moved through `DUE` and `done` only on the
/// completed override.
#[tokio::test(flavor = "multi_thread")]
async fn a_recurring_radicale_task_is_in_the_range_with_its_overrides() {
    let r = Radicale::start().await;
    r.put(&r.calendar(), "water.ics", tasks::RECURRING_TODO)
        .await;
    r.put(&r.calendar(), "standup.ics", tasks::EVENT).await;
    let mut n = connected(&r).await;

    let range = |q: &str| format!("/v1/entities?{}&{q}", tasks::NOVEMBER);
    let (api, read) = (&n.api, n.read.as_str());
    let all = n
        .sse
        .until("task and event", Duration::from_secs(30), async || {
            let v = ok(api, "GET", &range("kind=event,task"), read, None).await;
            (v["occurrences"].as_array().map(Vec::len) == Some(5)).then_some(v)
        })
        .await;
    let open = ok(
        &n.api,
        "GET",
        &range("kind=event,task&done=false"),
        &n.read,
        None,
    )
    .await;
    tasks::check_ranges(&all, &open);
}
