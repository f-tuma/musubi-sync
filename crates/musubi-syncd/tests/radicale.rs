//! The whole musubi-syncd binary with a DAV account on the pinned Radicale
//! (plan K8): connect, first data in the API, and a change at Radicale in the
//! change feed within two active-level intervals. Needs `MUSUBI_RADICALE`;
//! run by the CI job `conformance`.

use std::time::{Duration, Instant};

use musubi_testkit::google::FakeGoogle;
use musubi_testkit::harness::{self, Api, Daemon};
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

#[tokio::test(flavor = "multi_thread")]
async fn a_radicale_account_syncs_through_the_binary() {
    let r = Radicale::start().await;
    r.put(&r.calendar(), "todo-1.ics", TODO).await;
    r.put(&r.addressbook(), "bob.vcf", &card("bob", "Bob Builder"))
        .await;

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

    // First data.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let tasks = ok(&api, "GET", "/v1/entities?kind=task", &read, None).await;
        let found = tasks["entities"].as_array().is_some_and(|t| t.len() == 1)
            && names(&api, &read).await.len() == 1;
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
            &api,
            "GET",
            &format!("/v1/changes?since={since}"),
            &read,
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
    ok(&api, "POST", "/v1/interest", &read, None).await;
    for (uid, name) in [("carol", "Carol Singer"), ("dave", "Dave Diver")] {
        let start = Instant::now();
        r.put(&r.addressbook(), &format!("{uid}.vcf"), &card(uid, name))
            .await;
        loop {
            let page = ok(
                &api,
                "GET",
                &format!("/v1/changes?since={since}"),
                &read,
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
        names(&api, &read)
            .await
            .iter()
            .any(|n| n.contains("Dave Diver")),
        "{:?}",
        names(&api, &read).await
    );
}
