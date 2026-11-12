//! Benchmarks B1–B7 of plan 6.5: the release `musubi-syncd` binary as a child
//! process over the fake Google server, measured from the client side (HTTP
//! over loopback included). B8/B9 are crate benches (`musubi-google`,
//! `musubi-recurrence`).
//!
//! `bench [--smoke] [--syncd PATH] [--only B1,B3,...]`
//!
//! `--smoke` runs small and fails only on an order-of-magnitude miss (10× a
//! time budget); without it the scale and budgets are plan 6.5's. Prints a
//! Markdown table; exits 1 when a budget (or smoke guard) is missed.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use jiff::SignedDuration;
use musubi_testkit::google::{self, FakeGoogle, Seed};
use musubi_testkit::harness::{Api, Daemon, Rng, config, free_addr};
use serde_json::{Value, json};
use tokio::sync::Mutex;

/// The engine's push coalescing window (`musubi_engine::COALESCE`).
const COALESCE: Duration = Duration::from_millis(150);

struct Opts {
    smoke: bool,
    syncd: PathBuf,
    only: Option<Vec<String>>,
}

struct Row {
    id: &'static str,
    what: String,
    value: String,
    budget: String,
    ok: bool,
}

struct Node {
    fake: FakeGoogle,
    dir: tempfile::TempDir,
    api: Api,
    daemon: Daemon,
}

struct Tenant {
    id: String,
    admin: String,
    read: String,
}

impl Node {
    async fn start(syncd: &Path, fake: google::Config) -> Self {
        let fake = FakeGoogle::start(fake).await.expect("fake google");
        let dir = tempfile::tempdir().expect("tempdir");
        let addr = free_addr();
        let cfg = dir.path().join("config.json");
        let c = config(addr, &dir.path().join("data"), &fake);
        std::fs::write(&cfg, c.to_string()).expect("config");
        let daemon = tokio::task::block_in_place(|| Daemon::serve(syncd, &cfg, &[]));
        Self {
            fake,
            dir,
            api: Api::new(addr),
            daemon,
        }
    }

    async fn tenant(&self) -> Tenant {
        let id = self.api.tenant().await;
        let admin = self.api.key(&id, "admin").await;
        let read = self.api.key(&id, "read").await;
        Tenant { id, admin, read }
    }

    async fn get(&self, path: &str, key: &str) -> Value {
        let (s, v) = self
            .api
            .call("GET", path, Some(key), None)
            .await
            .expect("request");
        assert_eq!(s, 200, "GET {path}: {v}");
        v
    }

    /// Waits until every collection of the tenant is fully read.
    async fn full(&self, t: &Tenant, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        loop {
            let v = self.get("/v1/collections", &t.read).await;
            let cols = v["collections"].as_array().cloned().unwrap_or_default();
            if !cols.is_empty() && cols.iter().all(|c| c["coverage"]["type"] == "full") {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "no full read in {timeout:?}: {v}\n{}",
                self.daemon.tail(20)
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    async fn head(&self, t: &Tenant) -> u64 {
        let mut since = 0;
        loop {
            let (_, v) = self
                .api
                .changes(&t.read, since, 10_000)
                .await
                .expect("changes");
            if let Some(s) = v["changes"]
                .as_array()
                .and_then(|c| c.last())
                .and_then(|c| c["seq"].as_u64())
            {
                since = s;
            }
            if v["more"] != json!(true) {
                return since;
            }
        }
    }

    fn proc_status(&self, field: &str) -> u64 {
        let s =
            std::fs::read_to_string(format!("/proc/{}/status", self.daemon.pid())).expect("/proc");
        let line = s.lines().find(|l| l.starts_with(field)).expect(field);
        line.split_whitespace()
            .nth(1)
            .and_then(|v| v.parse().ok())
            .expect(field)
    }
}

fn pct(v: &[Duration], p: f64) -> Duration {
    let mut v = v.to_vec();
    v.sort();
    v[((v.len() as f64 * p).ceil() as usize).clamp(1, v.len()) - 1]
}

fn ms(d: Duration) -> String {
    format!("{:.1} ms", d.as_secs_f64() * 1000.0)
}

fn epoch() -> jiff::Timestamp {
    google::Config::default().epoch
}

fn event(summary: &str, start: jiff::Timestamp, recurrence: Option<&str>) -> Value {
    let end = start + SignedDuration::from_mins(45);
    let mut e = json!({
        "summary": summary,
        "start": { "dateTime": start.to_string(), "timeZone": "UTC" },
        "end": { "dateTime": end.to_string(), "timeZone": "UTC" },
    });
    if let Some(r) = recurrence {
        e["recurrence"] = json!([r]);
    }
    e
}

/// A time row: `limit` is the budget, ×10 in smoke mode.
fn timed(o: &Opts, id: &'static str, what: String, value: Duration, budget: Duration) -> Row {
    let limit = if o.smoke { budget * 10 } else { budget };
    Row {
        id,
        what,
        value: ms(value),
        budget: format!(
            "< {}{}",
            ms(budget),
            if o.smoke { " (smoke guard ×10)" } else { "" }
        ),
        ok: value < limit,
    }
}

/// B1: from the end of the OAuth callback to the first `/changes` entry,
/// 150 ms per fake request.
async fn b1(o: &Opts) -> Vec<Row> {
    let fake = google::Config {
        seed: Seed {
            seed: 1,
            calendars: 1,
            events: 50,
        },
        latency: Duration::from_millis(150),
        ..google::Config::default()
    };
    let n = Node::start(&o.syncd, fake).await;
    let mut runs = Vec::new();
    for _ in 0..if o.smoke { 1 } else { 5 } {
        let t = n.tenant().await;
        n.api.connect(&t.admin).await;
        let t0 = Instant::now();
        loop {
            let (_, v) = n.api.changes(&t.read, 0, 1).await.expect("changes");
            if v["changes"].as_array().is_some_and(|c| !c.is_empty()) {
                break;
            }
            assert!(
                t0.elapsed() < Duration::from_secs(30),
                "no data: {}",
                n.daemon.tail(20)
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        runs.push(t0.elapsed());
    }
    let what = format!("first data after connect, worst of {}", runs.len());
    vec![timed(
        o,
        "B1",
        what,
        pct(&runs, 1.0),
        Duration::from_secs(1),
    )]
}

/// Notification → SSE event, `iters` times, at `latency` per fake request.
async fn push_to_sse(o: &Opts, latency: Duration, iters: usize) -> Vec<Duration> {
    let fake = google::Config {
        seed: Seed {
            seed: 2,
            calendars: 0,
            events: 50,
        },
        latency,
        ..google::Config::default()
    };
    let n = Node::start(&o.syncd, fake).await;
    let t = n.tenant().await;
    n.api.connect(&t.admin).await;
    n.full(&t, Duration::from_secs(60)).await;
    while n.fake.channels().is_empty() {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // The delta right after the full read.
    tokio::time::sleep(COALESCE + latency * 2 + Duration::from_millis(200)).await;
    let mut sse = n
        .api
        .http
        .get(format!("{}/v1/stream", n.api.base))
        .bearer_auth(&t.read)
        .send()
        .await
        .expect("stream");
    assert_eq!(sse.status(), 200);
    let mut buf = String::new();
    let mut next_id = async |sse: &mut reqwest::Response| -> u64 {
        loop {
            if let Some(i) = buf.find("\n\n") {
                let ev: String = buf.drain(..i + 2).collect();
                if let Some(id) = ev.lines().find_map(|l| l.strip_prefix("id: ")) {
                    return id.trim().parse().expect("seq");
                }
                continue;
            }
            let chunk = sse.chunk().await.expect("stream").expect("stream open");
            buf.push_str(&String::from_utf8_lossy(&chunk));
        }
    };
    let mut last = next_id(&mut sse).await;
    let primary = n.fake.primary();
    let mut out = Vec::new();
    for i in 0..iters {
        let t0 = Instant::now();
        n.fake
            .put_event(&primary, event(&format!("push {i}"), epoch(), None));
        loop {
            let id = tokio::time::timeout(Duration::from_secs(30), next_id(&mut sse))
                .await
                .expect("no SSE event");
            if id > last {
                last = id;
                break;
            }
        }
        out.push(t0.elapsed());
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    out
}

/// B2: notification → SSE at 150 ms per fake request.
async fn b2(o: &Opts) -> Vec<Row> {
    let v = push_to_sse(o, Duration::from_millis(150), if o.smoke { 5 } else { 20 }).await;
    let what = format!(
        "notification → SSE, 150 ms fake, worst of {} (p50 {})",
        v.len(),
        ms(pct(&v, 0.5))
    );
    vec![timed(
        o,
        "B2",
        what,
        pct(&v, 1.0),
        Duration::from_millis(500),
    )]
}

/// B5: notification → SSE with an instant fake, minus the coalescing window:
/// the engine's own cost of one increment (the fake's own time included).
async fn b5(o: &Opts) -> Vec<Row> {
    let v: Vec<_> = push_to_sse(o, Duration::ZERO, if o.smoke { 5 } else { 20 })
        .await
        .into_iter()
        .map(|d| d.saturating_sub(COALESCE))
        .collect();
    let what = format!(
        "increment overhead (SSE time − 150 ms coalescing), p99 of {}, p50 {}",
        v.len(),
        ms(pct(&v, 0.5))
    );
    vec![timed(
        o,
        "B5",
        what,
        pct(&v, 0.99),
        Duration::from_millis(20),
    )]
}

/// B3 and B4 over one tenant: single events over ±2.5 years and open weekly
/// series.
async fn b3_b4(o: &Opts) -> Vec<Row> {
    let (singles, series, reqs) = if o.smoke {
        (495, 5, 50)
    } else {
        (4950, 50, 500)
    };
    let n = Node::start(&o.syncd, google::Config::default()).await;
    let primary = n.fake.primary();
    let mut rng = Rng::new(3);
    let span_min = 5 * 365 * 24 * 60;
    let at = |rng: &mut Rng| {
        epoch() + SignedDuration::from_mins(rng.below(span_min) as i64 - span_min as i64 / 2)
    };
    for i in 0..singles {
        n.fake
            .put_event(&primary, event(&format!("single {i}"), at(&mut rng), None));
    }
    for i in 0..series {
        n.fake.put_event(
            &primary,
            event(
                &format!("series {i}"),
                at(&mut rng),
                Some("RRULE:FREQ=WEEKLY"),
            ),
        );
    }
    let t = n.tenant().await;
    n.api.connect(&t.admin).await;
    n.full(&t, Duration::from_secs(120)).await;
    tokio::time::sleep(Duration::from_secs(1)).await;

    let mut rows = Vec::new();
    for (days, ids_only) in [(7, false), (7, true), (30, false), (30, true)] {
        let mut lat = Vec::new();
        let mut occurrences = 0;
        for _ in 0..reqs {
            let start = at(&mut rng);
            let end = start + SignedDuration::from_hours(24 * days);
            let mut after: Option<String> = None;
            let t0 = Instant::now();
            loop {
                let mut q = vec![
                    ("from", start.to_string()),
                    ("to", end.to_string()),
                    ("limit", "1000".into()),
                ];
                if ids_only {
                    q.push(("ids_only", "true".into()));
                }
                q.extend(after.take().map(|a| ("after", a)));
                let url =
                    reqwest::Url::parse_with_params(&format!("{}/v1/entities", n.api.base), &q)
                        .expect("url");
                let v = n.get(url.as_str(), &t.read).await;
                occurrences += v["occurrences"].as_array().map_or(0, Vec::len);
                match v["next"].as_str() {
                    Some(a) => after = Some(a.to_owned()),
                    None => break,
                }
            }
            lat.push(t0.elapsed());
        }
        let what = format!(
            "range {days} d{}, {singles}+{series} series, ~{} occurrences, p99 of {reqs}",
            if ids_only { " ids only" } else { "" },
            occurrences / reqs
        );
        rows.push(timed(
            o,
            "B3",
            what,
            pct(&lat, 0.99),
            Duration::from_millis(10),
        ));
    }

    let head = n.head(&t).await;
    let mut lat = Vec::new();
    for _ in 0..reqs {
        let since = rng.below(head.saturating_sub(500).max(1));
        let t0 = Instant::now();
        let (s, _) = n.api.changes(&t.read, since, 500).await.expect("changes");
        assert_eq!(s, 200);
        lat.push(t0.elapsed());
    }
    let what = format!("/changes by 500 of {head}, p99 of {reqs}");
    rows.push(timed(
        o,
        "B4",
        what,
        pct(&lat, 0.99),
        Duration::from_millis(10),
    ));
    rows
}

/// B6: the largest WAL while a big tenant imports and two clients read.
async fn b6(o: &Opts) -> Vec<Row> {
    let per_cal = if o.smoke { 200 } else { 5000 };
    let fake = google::Config {
        seed: Seed {
            seed: 6,
            calendars: 9,
            events: per_cal,
        },
        ..google::Config::default()
    };
    let n = Arc::new(Node::start(&o.syncd, fake).await);
    let t = Arc::new(n.tenant().await);
    let stop = Arc::new(AtomicBool::new(false));
    let wal_max = Arc::new(AtomicU64::new(0));
    let t0 = Instant::now();
    n.api.connect(&t.admin).await;
    let wal = n
        .dir
        .path()
        .join("data/tenants")
        .join(&t.id)
        .join("main.db-wal");
    let watcher = {
        let (stop, wal_max) = (stop.clone(), wal_max.clone());
        tokio::spawn(async move {
            while !stop.load(Ordering::Relaxed) {
                if let Ok(m) = std::fs::metadata(&wal) {
                    wal_max.fetch_max(m.len(), Ordering::Relaxed);
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
    };
    let reads = Arc::new(Mutex::new(Vec::new()));
    let mut readers = Vec::new();
    for r in 0..2 {
        let (n, t, stop, reads) = (n.clone(), t.clone(), stop.clone(), reads.clone());
        readers.push(tokio::spawn(async move {
            let week = (
                epoch().to_string(),
                (epoch() + SignedDuration::from_hours(24 * 7)).to_string(),
            );
            while !stop.load(Ordering::Relaxed) {
                let t1 = Instant::now();
                if r == 0 {
                    let path = format!(
                        "/v1/entities?from={}&to={}&limit=1000&ids_only=true",
                        week.0, week.1
                    );
                    n.get(&path, &t.read).await;
                } else {
                    n.api.changes(&t.read, 0, 500).await.expect("changes");
                }
                reads.lock().await.push(t1.elapsed());
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }));
    }
    n.full(&t, Duration::from_secs(1800)).await;
    let import = t0.elapsed();
    stop.store(true, Ordering::Relaxed);
    watcher.await.expect("watcher");
    for r in readers {
        r.await.expect("reader");
    }
    let reads = reads.lock().await;
    let wal_mb = wal_max.load(Ordering::Relaxed) as f64 / (1024.0 * 1024.0);
    let events = 10 * per_cal;
    vec![
        Row {
            id: "B6",
            what: format!("largest WAL, import of {events} events with 2 readers"),
            value: format!("{wal_mb:.1} MB"),
            budget: "< 256 MB".into(),
            ok: wal_mb < 256.0,
        },
        Row {
            id: "B6",
            what: format!(
                "import time; reader p99 {} / max {} over {} reads",
                ms(pct(&reads, 0.99)),
                ms(pct(&reads, 1.0)),
                reads.len()
            ),
            value: format!("{:.1} s", import.as_secs_f64()),
            budget: "report".into(),
            ok: true,
        },
    ]
}

/// B7: RSS per sleeping account, one tenant per account.
async fn b7(o: &Opts) -> Vec<Row> {
    let accounts: usize = if o.smoke { 20 } else { 10_000 };
    let n = Arc::new(Node::start(&o.syncd, google::Config::default()).await);
    tokio::time::sleep(Duration::from_secs(1)).await;
    let (base_rss, base_threads) = (n.proc_status("VmRSS:"), n.proc_status("Threads:"));
    let t0 = Instant::now();
    let mut set = tokio::task::JoinSet::new();
    for _ in 0..accounts {
        if set.len() >= 32 {
            set.join_next().await.expect("task").expect("connect");
        }
        let n = n.clone();
        set.spawn(async move {
            let t = n.tenant().await;
            n.api.connect(&t.admin).await;
        });
    }
    while let Some(r) = set.join_next().await {
        r.expect("connect");
    }
    // Asleep: every account has synced and holds its channel.
    let deadline = Instant::now() + Duration::from_secs(1800);
    while n.fake.channels().len() < accounts {
        assert!(
            Instant::now() < deadline,
            "{} of {accounts} channels",
            n.fake.channels().len()
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let connect = t0.elapsed();
    // Asleep for real: past the engine's writer idle stop (30 s) and tokio's
    // blocking-pool keep-alive (10 s).
    tokio::time::sleep(Duration::from_secs(40)).await;
    let (rss, threads) = (n.proc_status("VmRSS:"), n.proc_status("Threads:"));
    let per = rss.saturating_sub(base_rss) as f64 / accounts as f64;
    // Sleeping tenants hold no writer thread; the rest is the runtime's
    // workers and blocking pool (at most 512), whatever the account count.
    let mut names = std::collections::BTreeMap::<String, usize>::new();
    let tasks = std::fs::read_dir(format!("/proc/{}/task", n.daemon.pid())).expect("/proc");
    for t in tasks.flatten() {
        let comm = std::fs::read_to_string(t.path().join("comm")).unwrap_or_default();
        *names.entry(comm.trim().to_owned()).or_default() += 1;
    }
    let writers: usize = names
        .iter()
        .filter(|(k, _)| k.starts_with("musubi-writer"))
        .map(|(_, v)| v)
        .sum();
    vec![Row {
        id: "B7",
        what: format!(
            "RSS per sleeping account, {accounts} accounts ({} → {} MiB, threads {base_threads} → {threads}, connected in {:.0} s)",
            base_rss / 1024,
            rss / 1024,
            connect.as_secs_f64()
        ),
        value: format!("{per:.1} KiB"),
        budget: format!("report; no writer thread left (found {writers}), threads {names:?}"),
        ok: writers == 0,
    }]
}

fn opts() -> Opts {
    let mut o = Opts {
        smoke: false,
        syncd: std::env::current_exe()
            .expect("exe")
            .with_file_name("musubi-syncd"),
        only: None,
    };
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--smoke" => o.smoke = true,
            "--syncd" => o.syncd = args.next().expect("--syncd PATH").into(),
            "--only" => {
                o.only = Some(
                    args.next()
                        .expect("--only B1,B2")
                        .split(',')
                        .map(str::to_owned)
                        .collect(),
                )
            }
            _ => panic!("usage: bench [--smoke] [--syncd PATH] [--only B1,B3,...]"),
        }
    }
    assert!(
        o.syncd.exists(),
        "{} not found; build it or pass --syncd",
        o.syncd.display()
    );
    o
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    let o = opts();
    let want = |id: &str| o.only.as_ref().is_none_or(|v| v.iter().any(|x| x == id));
    let mut rows = Vec::new();
    if want("B1") {
        rows.extend(b1(&o).await);
    }
    if want("B2") {
        rows.extend(b2(&o).await);
    }
    if want("B3") || want("B4") {
        rows.extend(b3_b4(&o).await);
    }
    if want("B5") {
        rows.extend(b5(&o).await);
    }
    if want("B6") {
        rows.extend(b6(&o).await);
    }
    if want("B7") {
        rows.extend(b7(&o).await);
    }
    println!("| # | measured | value | budget | |\n| --- | --- | --- | --- | --- |");
    for r in &rows {
        let mark = if r.ok { "ok" } else { "MISSED" };
        println!(
            "| {} | {} | {} | {} | {mark} |",
            r.id, r.what, r.value, r.budget
        );
    }
    if rows.iter().any(|r| !r.ok) {
        std::process::exit(1);
    }
}
