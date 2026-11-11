//! Plan 6.4: `musubi-syncd backup` of the whole binary while a sync is in
//! flight, the node stopped, its data directory lost, `restore`, and the
//! node back with the state of the backup, then catching up with the fake.

mod common;

use std::process::Command;
use std::time::Duration;

use common::whole::{BIN, World, check_store, head};
use jiff::SignedDuration;
use musubi_testkit::google::{self, Seed};
use serde_json::{Value, json};

/// The whole journal and every occurrence within 400 days of the fake's
/// epoch, through the API.
async fn state(w: &World) -> (Vec<Value>, Vec<Value>) {
    let mut changes = Vec::new();
    let mut since = 0;
    loop {
        let (s, v) = w.api.changes(&w.read, since, 1000).await.expect("changes");
        assert_eq!(s, 200, "{v}");
        let page = v["changes"].as_array().expect("changes").clone();
        if let Some(last) = page.last() {
            since = last["seq"].as_u64().expect("seq");
        }
        changes.extend(page);
        if v["more"] != json!(true) {
            break;
        }
    }
    let epoch = google::Config::default().epoch;
    let days = SignedDuration::from_hours(24 * 400);
    let (from, to) = ((epoch - days).to_string(), (epoch + days).to_string());
    let mut range = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let mut q = vec![
            ("from", from.clone()),
            ("to", to.clone()),
            ("limit", "500".into()),
        ];
        q.extend(after.take().map(|a| ("after", a)));
        let url = reqwest::Url::parse_with_params(&format!("{}/v1/entities", w.api.base), &q)
            .expect("url");
        let (s, v) = w
            .api
            .call("GET", url.as_str(), Some(&w.read), None)
            .await
            .expect("range");
        assert_eq!(s, 200, "{v}");
        range.push(v["unindexed"].clone());
        range.extend(
            v["occurrences"]
                .as_array()
                .expect("occurrences")
                .iter()
                .cloned(),
        );
        match v["next"].as_str() {
            Some(n) => after = Some(n.to_owned()),
            None => break,
        }
    }
    (changes, range)
}

fn new_event(summary: &str) -> Value {
    let start = google::Config::default().epoch + SignedDuration::from_hours(5);
    let end = start + SignedDuration::from_mins(30);
    json!({
        "summary": summary,
        "start": { "dateTime": start.to_string(), "timeZone": "UTC" },
        "end": { "dateTime": end.to_string(), "timeZone": "UTC" },
    })
}

fn run(args: &[&str]) {
    let out = tokio::task::block_in_place(|| Command::new(BIN).args(args).output())
        .expect("run musubi-syncd");
    assert!(
        out.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn backup_during_sync_restores_its_state_and_catches_up_without_duplicates() {
    let fake = google::Config {
        seed: Seed {
            seed: 64,
            calendars: 1,
            events: 100,
        },
        max_page_size: 25,
        ..google::Config::default()
    };
    let mut w = World::start(fake, &[]).await;
    w.converge(Duration::from_secs(60), |w| w.fake.channels().len() == 2)
        .await;

    // A change whose delta stays in flight while the backup is taken.
    let latency = Duration::from_secs(5);
    w.fake.set_latency(latency);
    let cal = w.fake.primary();
    let requests = w.fake.requests().len();
    w.fake.put_event(&cal, new_event("during the backup"));
    let started = std::time::Instant::now();
    while w.fake.requests().len() == requests {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the notification started no sync"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let before = state(&w).await;
    let backup = w.dir.path().join("backup");
    let cfg = w.config.to_str().expect("utf8").to_owned();
    run(&[
        "backup",
        "--config",
        &cfg,
        "--to",
        backup.to_str().expect("utf8"),
    ]);
    assert_eq!(state(&w).await, before, "a commit landed during the backup");
    assert!(
        started.elapsed() < latency,
        "the sync was not in flight for the whole backup"
    );

    // main.db is the only tenant file in phase 1; `Storage::backup` copies it
    // before outbox.db (phase 3) so a restored outbox never runs ahead of it.
    let files: Vec<_> = std::fs::read_dir(backup.join("tenants").join(&w.tenant))
        .expect("tenant backup")
        .map(|e| e.expect("entry").file_name().into_string().expect("utf8"))
        .collect();
    assert_eq!(files, ["main.db"]);

    w.kill();
    std::fs::remove_dir_all(w.data()).expect("lose the data");
    run(&[
        "restore",
        "--config",
        &cfg,
        "--from",
        backup.to_str().expect("utf8"),
    ]);
    // Still slow: the startup catch-up commits nothing before the comparison.
    w.serve(&[]);
    assert_eq!(
        state(&w).await,
        before,
        "the restored node answers as at the backup"
    );

    w.fake.set_latency(Duration::ZERO);
    w.fake.put_event(&cal, new_event("after the restore"));
    w.converge(Duration::from_secs(60), |_| true).await;
    check_store(&w);
    let after = state(&w).await;
    assert_eq!(
        after.0[..before.0.len()],
        before.0[..],
        "the journal of the backup is kept"
    );
    let journal = w.journal();
    assert!(
        head(&journal)
            > before
                .0
                .last()
                .and_then(|c| c["seq"].as_u64())
                .expect("head")
    );
}
