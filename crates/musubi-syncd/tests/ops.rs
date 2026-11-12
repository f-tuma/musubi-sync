//! `musubi-syncd backup` against a running node and `restore` into a stopped
//! one, and the stop signals of `serve`, through the binary.

mod common;

use std::path::Path;
use std::process::{Child, Command, Output};
use std::time::Duration;

use common::{ADMIN_KEY, config_json, event};
use musubi_model::{AccountId, Storage, TenantId, TenantStore};
use musubi_store_sqlite::SqliteStorage;
use musubi_testkit::google::FakeGoogle;
use serde_json::{Value, json};

const BIN: &str = env!("CARGO_BIN_EXE_musubi-syncd");

struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn run(args: &[&str]) -> Output {
    Command::new(BIN)
        .args(args)
        .output()
        .expect("run musubi-syncd")
}

async fn serve(config: &Path, addr: &str) -> Server {
    let s = Server(
        Command::new(BIN)
            .args(["serve", "--config"])
            .arg(config)
            .spawn()
            .expect("spawn"),
    );
    for _ in 0..200 {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            return s;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("the node did not start listening on {addr}");
}

async fn call(
    http: &reqwest::Client,
    method: &str,
    url: String,
    auth: &str,
    body: Option<Value>,
) -> (u16, Value) {
    let mut r = http
        .request(method.parse().expect("method"), url)
        .bearer_auth(auth);
    if let Some(b) = body {
        r = r.json(&b);
    }
    let resp = r.send().await.expect("send");
    let s = resp.status().as_u16();
    (s, resp.json().await.unwrap_or(Value::Null))
}

#[tokio::test(flavor = "multi_thread")]
async fn backup_of_a_running_node_restores_into_a_stopped_one() {
    let google = FakeGoogle::start(Default::default())
        .await
        .expect("fake google");
    let dir = tempfile::tempdir().expect("tempdir");
    let addr = std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .expect("free port")
        .to_string();
    let data = dir.path().join("data");
    // A looser existing data directory is tightened at startup.
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        std::fs::DirBuilder::new()
            .mode(0o755)
            .create(&data)
            .expect("data dir");
    }
    let config = dir.path().join("config.json");
    std::fs::write(&config, config_json(&addr, &data, &google).to_string()).expect("config");
    let cfg = config.to_str().expect("utf8");
    let base = format!("http://{addr}");
    let _ = rustls::crypto::ring::default_provider().install_default();
    let http = reqwest::Client::new();

    let node = serve(&config, &addr).await;
    let (s, v) = call(
        &http,
        "POST",
        format!("{base}/v1/admin/tenants"),
        ADMIN_KEY,
        None,
    )
    .await;
    assert_eq!(s, 201, "{v}");
    let t: TenantId = v["id"].as_str().expect("id").parse().expect("tenant");
    let (s, v) = call(
        &http,
        "POST",
        format!("{base}/v1/admin/tenants/{t}/keys"),
        ADMIN_KEY,
        Some(json!({ "scope": "read" })),
    )
    .await;
    assert_eq!(s, 201, "{v}");
    let key = v["key"].as_str().expect("key").to_owned();
    // Data the engine would have written; SQLite shares the files between processes.
    let (col, head) = common::write(
        &SqliteStorage::new(&data, 1).expect("storage"),
        t,
        AccountId::from_u128(1),
        "cal",
        vec![event("e1", "1", 1)],
    );

    let backup = dir.path().join("backups/backup");
    let out = run(&[
        "backup",
        "--config",
        cfg,
        "--to",
        backup.to_str().expect("utf8"),
    ]);
    assert!(
        out.status.success(),
        "backup: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = |p: &Path| std::fs::metadata(p).expect("stat").permissions().mode() & 0o777;
        assert_eq!(mode(&backup), 0o700, "a backup holds secrets");
        assert_eq!(mode(backup.parent().expect("parent")), 0o700);
        assert_eq!(mode(&backup.join("control.db")), 0o600);
        assert_eq!(mode(&data), 0o700);
    }
    let out = run(&[
        "backup",
        "--config",
        cfg,
        "--to",
        backup.to_str().expect("utf8"),
    ]);
    assert!(!out.status.success(), "a backup never overwrites another");

    let out = run(&[
        "restore",
        "--config",
        cfg,
        "--from",
        backup.to_str().expect("utf8"),
    ]);
    assert!(
        !out.status.success(),
        "restore refuses a running node's data directory"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("running node"),
        "{out:?}"
    );

    drop(node);
    std::fs::remove_dir_all(&data).expect("lose the data");
    let out = run(&[
        "restore",
        "--config",
        cfg,
        "--from",
        backup.to_str().expect("utf8"),
    ]);
    assert!(
        out.status.success(),
        "restore: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = run(&[
        "restore",
        "--config",
        cfg,
        "--from",
        backup.to_str().expect("utf8"),
    ]);
    assert!(
        !out.status.success(),
        "restore refuses a non-empty data directory"
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(data.join("control.db"))
            .expect("stat")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "restored files stay private");
    }
    let _node = serve(&config, &addr).await;
    let (s, v) = call(&http, "GET", format!("{base}/v1/collections"), &key, None).await;
    assert_eq!(s, 200, "the key survived: {v}");
    assert_eq!(v["collections"][0]["id"], col.to_string(), "{v}");
    let (s, v) = call(
        &http,
        "GET",
        format!("{base}/v1/changes?since=0"),
        &key,
        None,
    )
    .await;
    assert_eq!((s, &v["next"]), (200, &json!(head.0)), "{v}");
    let stored = SqliteStorage::new(&data, 1).expect("storage");
    assert_eq!(stored.open(t).expect("tenant").head().expect("head"), head);
}

/// SIGINT and SIGTERM stop `serve` within a bounded time, also with a
/// `/v1/stream` open (it never ends by itself).
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn stop_signal_ends_serve_with_a_stream_open() {
    let google = FakeGoogle::start(Default::default())
        .await
        .expect("fake google");
    let _ = rustls::crypto::ring::default_provider().install_default();
    let http = reqwest::Client::new();
    for sig in ["-INT", "-TERM"] {
        let dir = tempfile::tempdir().expect("tempdir");
        let addr = std::net::TcpListener::bind("127.0.0.1:0")
            .and_then(|l| l.local_addr())
            .expect("free port")
            .to_string();
        let config = dir.path().join("config.json");
        let c = config_json(&addr, &dir.path().join("data"), &google);
        std::fs::write(&config, c.to_string()).expect("config");
        let base = format!("http://{addr}");
        let mut node = serve(&config, &addr).await;

        let (_, v) = call(
            &http,
            "POST",
            format!("{base}/v1/admin/tenants"),
            ADMIN_KEY,
            None,
        )
        .await;
        let t = v["id"].as_str().expect("id").to_owned();
        let body = json!({ "scope": "read" });
        let url = format!("{base}/v1/admin/tenants/{t}/keys");
        let (_, v) = call(&http, "POST", url, ADMIN_KEY, Some(body)).await;
        let key = v["key"].as_str().expect("key");
        let stream = http
            .get(format!("{base}/v1/stream"))
            .bearer_auth(key)
            .send()
            .await
            .expect("stream");
        assert_eq!(stream.status(), 200);

        let pid = node.0.id().to_string();
        let kill = Command::new("kill").args([sig, &pid]).status();
        assert!(kill.expect("kill").success());
        let started = std::time::Instant::now();
        let status = loop {
            if let Some(s) = node.0.try_wait().expect("wait") {
                break s;
            }
            assert!(
                started.elapsed() < Duration::from_secs(15),
                "{sig}: still running"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        assert!(status.success(), "{sig}: {status:?}");
        drop(stream);
    }
}
