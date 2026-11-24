//! Benchmarks B1–B7 (and B7n) of plan 6.5, D1, D2 and D4 of phase 2 plan 11.2
//! and B3t of phase 2 plan 12.7: the
//! release `musubi-syncd` binary as a child process over the fake Google or
//! DAV server, measured from the client side (HTTP over loopback included).
//! B8/B9 and D3 are crate benches (`musubi-google`, `musubi-recurrence`,
//! `musubi-ical`).
//!
//! `bench [--smoke] [--syncd PATH] [--only B1,D2,...]`
//!
//! `--smoke` runs small and fails only on an order-of-magnitude miss (10× a
//! time budget); without it the scale and budgets are plan 6.5's. Prints a
//! Markdown table; exits 1 when a budget (or smoke guard) is missed.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use jiff::SignedDuration;
use musubi_testkit::dav::{self, Config as DavConfig, FakeDav, Kind, Quirk};
use musubi_testkit::google::{self, FakeGoogle, Seed};
use musubi_testkit::harness::{Api, Daemon, Rng, Sse, config, free_addr};
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
        Self::start_with(syncd, fake, |_| {}).await
    }

    /// [`Node::start`] with the configuration changed by `f`.
    async fn start_with(syncd: &Path, fake: google::Config, f: impl FnOnce(&mut Value)) -> Self {
        let fake = FakeGoogle::start(fake).await.expect("fake google");
        let dir = tempfile::tempdir().expect("tempdir");
        let addr = free_addr();
        let cfg = dir.path().join("config.json");
        let mut c = config(addr, &dir.path().join("data"), &fake);
        f(&mut c);
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

/// B1: from following the OAuth callback (token exchange included) to the
/// first event in `/changes`, 150 ms per fake request.
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
        let callback = n.api.consent(&t.admin).await;
        let t0 = Instant::now();
        n.api
            .http
            .get(&callback)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .expect("callback");
        let mut since = 0;
        // Discovery's collection changes come first; data is an entity.
        'wait: loop {
            let (_, v) = n.api.changes(&t.read, since, 500).await.expect("changes");
            for c in v["changes"].as_array().expect("changes") {
                if c["target"]["type"] == "entity" {
                    break 'wait;
                }
                since = c["seq"].as_u64().expect("seq");
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
    let what = format!(
        "first event after the OAuth callback, worst of {}",
        runs.len()
    );
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
    let mut sse = Sse::open(&n.api, &t.read).await;
    let mut last = sse.next_id().await;
    let primary = n.fake.primary();
    let mut out = Vec::new();
    for i in 0..iters {
        let t0 = Instant::now();
        n.fake
            .put_event(&primary, event(&format!("push {i}"), epoch(), None));
        last = sse.past(last).await;
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
    // 200 so the p99 is not simply the worst sample of a noisy runner.
    let v: Vec<_> = push_to_sse(o, Duration::ZERO, if o.smoke { 5 } else { 200 })
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

/// `reqs` paged range queries of `days` days from `start()` with the query
/// pairs `extra`, each timed over all its pages; the p99 and the mean number
/// of occurrences per query.
async fn ranges(
    n: &Node,
    t: &Tenant,
    reqs: usize,
    days: i64,
    extra: &[(&str, &str)],
    mut start: impl FnMut() -> jiff::Timestamp,
) -> (Duration, usize) {
    let mut lat = Vec::new();
    let mut occurrences = 0;
    for _ in 0..reqs {
        let start = start();
        let end = start + SignedDuration::from_hours(24 * days);
        let mut after: Option<String> = None;
        let t0 = Instant::now();
        loop {
            let mut q = vec![
                ("from", start.to_string()),
                ("to", end.to_string()),
                ("limit", "1000".into()),
            ];
            q.extend(extra.iter().map(|&(k, v)| (k, v.to_owned())));
            q.extend(after.take().map(|a| ("after", a)));
            let url = reqwest::Url::parse_with_params(&format!("{}/v1/entities", n.api.base), &q)
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
    (pct(&lat, 0.99), occurrences / reqs)
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
        let extra: &[(&str, &str)] = if ids_only {
            &[("kind", "event"), ("ids_only", "true")]
        } else {
            &[("kind", "event")]
        };
        let (p99, occurrences) = ranges(&n, &t, reqs, days, extra, || at(&mut rng)).await;
        let what = format!(
            "range {days} d{}, {singles}+{series} series, ~{occurrences} occurrences, p99 of {reqs}",
            if ids_only { " ids only" } else { "" },
        );
        rows.push(timed(o, "B3", what, p99, Duration::from_millis(10)));
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

/// B3t (phase 2 plan 12.7): B3's events over a fake DAV calendar plus
/// 5 000 tasks in the same calendar: 500 open recurring (half daily, half
/// weekly), 1 000 without a date, 1 000 completed and the rest open one-off,
/// dated as B3's events. Ranges of events alone (guarded like B3), of both
/// kinds and of open tasks, and the size of `main.db`.
async fn b3t(o: &Opts) -> Vec<Row> {
    let (singles, series, recurring, undated, completed, tasks, reqs) = if o.smoke {
        (495, 5, 50, 100, 100, 500, 50)
    } else {
        (4950, 50, 500, 1000, 1000, 5000, 500)
    };
    let dav = FakeDav::start(DavConfig::default())
        .await
        .expect("fake dav");
    let cal = dav.add_collection(Kind::Calendar(&["VEVENT", "VTODO"]), "home", "Home");
    let mut rng = Rng::new(3);
    let span_min = 5 * 365 * 24 * 60;
    let at = |rng: &mut Rng| {
        epoch() + SignedDuration::from_mins(rng.below(span_min) as i64 - span_min as i64 / 2)
    };
    for i in 0..singles {
        let uid = format!("single-{i}");
        dav.put(&cal, &format!("{uid}.ics"), &vevent(&uid, at(&mut rng)));
    }
    for i in 0..series {
        let uid = format!("series-{i}");
        let ics = vevent_rule(&uid, at(&mut rng), Some("FREQ=WEEKLY"));
        dav.put(&cal, &format!("{uid}.ics"), &ics);
    }
    for i in 0..tasks {
        let uid = format!("task-{i}");
        let start = at(&mut rng);
        let dated = format!(
            "DTSTART:{}\r\nDUE:{}\r\n",
            ics_time(start),
            ics_time(start + SignedDuration::from_hours(1))
        );
        let props = match i {
            i if i < recurring / 2 => format!("{dated}RRULE:FREQ=DAILY\r\n"),
            i if i < recurring => format!("{dated}RRULE:FREQ=WEEKLY\r\n"),
            i if i < recurring + undated => String::new(),
            i if i < recurring + undated + completed => format!("{dated}STATUS:COMPLETED\r\n"),
            _ => dated,
        };
        dav.put(&cal, &format!("{uid}.ics"), &vcal("VTODO", &uid, &props));
    }
    let (n, t) = dav_node(o, json!({})).await;
    connect_dav(&n, &t, &dav).await;
    n.full(&t, Duration::from_secs(600)).await;
    tokio::time::sleep(Duration::from_secs(1)).await;

    let data = format!("{singles}+{series} series, {tasks} tasks ({recurring} recurring)");
    let mut rows = Vec::new();
    // `ids_only` splits the cost of the rows from that of their payload.
    let queries: [(&str, &[(&str, &str)]); 4] = [
        ("kind=event", &[("kind", "event")]),
        ("kind=event,task", &[("kind", "event,task")]),
        (
            "kind=event,task ids only",
            &[("kind", "event,task"), ("ids_only", "true")],
        ),
        (
            "kind=task&done=false",
            &[("kind", "task"), ("done", "false")],
        ),
    ];
    for days in [7, 30] {
        for (name, extra) in queries {
            let (p99, occurrences) = ranges(&n, &t, reqs, days, extra, || at(&mut rng)).await;
            let what =
                format!("range {days} d {name}, {data}, ~{occurrences} occurrences, p99 of {reqs}");
            rows.push(if name == "kind=event" {
                timed(o, "B3t", what, p99, Duration::from_millis(10))
            } else {
                Row {
                    id: "B3t",
                    what,
                    value: ms(p99),
                    budget: "report".into(),
                    ok: true,
                }
            });
        }
    }
    let db = n
        .dir
        .path()
        .join("data/tenants")
        .join(&t.id)
        .join("main.db");
    let main = std::fs::metadata(&db).expect("main.db").len();
    // No WAL file is an empty WAL.
    let wal = std::fs::metadata(db.with_extension("db-wal")).map_or(0, |m| m.len());
    rows.push(Row {
        id: "B3t",
        what: format!("main.db size, {data} (WAL {:.1} MB)", wal as f64 / 1e6),
        value: format!("{:.1} MB", main as f64 / 1e6),
        budget: "report".into(),
        ok: true,
    });
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

/// Most threads the node may have at any moment of B7 and B7n, whatever the
/// account count: runtime workers, the blocking pool in use (store and control
/// calls of at most `sync_concurrency + background_concurrency` jobs plus the
/// API) and the fixed threads (checkpointer, signals).
const THREAD_CEILING: u64 = 128;

/// B7: RSS per account and the node's threads, one tenant per account.
/// Without `normal` the accounts sleep. A tenant sleeps 7 days after its last
/// interest; the run puts the engine's sleeping bounds (plan 7.2) in place of
/// the normal ones instead of waiting, as D2 does. B7n (`normal`) polls every
/// account at the floor of the normal bounds, every 120 s (smoke 2 s), and
/// each poll writes its tenant's collection state: the worst case for writer
/// threads, which grew with the account count until writers shared the
/// runtime's blocking pool.
async fn b7(o: &Opts, normal: bool) -> Vec<Row> {
    let accounts: usize = if o.smoke { 20 } else { 10_000 };
    let id = if normal { "B7n" } else { "B7" };
    let every = if o.smoke { 2.0 } else { 120.0 };
    let bounds = if normal {
        json!({ "min": every, "max": every })
    } else {
        json!({ "min": 3600.0, "max": 21_600.0 })
    };
    let n = Arc::new(
        Node::start_with(&o.syncd, google::Config::default(), |c| {
            c["sync"] = json!({ "normal": bounds });
        })
        .await,
    );
    tokio::time::sleep(Duration::from_secs(1)).await;
    let (base_rss, base_threads) = (n.proc_status("VmRSS:"), n.proc_status("Threads:"));
    // The highest thread count over the whole run, not just at its end.
    let peak = Arc::new(AtomicU64::new(base_threads));
    let sampler = {
        let (n, peak) = (n.clone(), peak.clone());
        tokio::spawn(async move {
            loop {
                peak.fetch_max(n.proc_status("Threads:"), Ordering::Relaxed);
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
    };
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
    // Every account has synced and holds its channel.
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
    let polls = |n: &Node| {
        let r = n.fake.requests();
        r.iter().filter(|r| r.contains("/connections")).count()
    };
    let (mark, window) = (polls(&n), Instant::now());
    // Normal: two and a half poll intervals. Asleep: past the engine's writer
    // idle stop (30 s) and tokio's blocking-pool keep-alive (10 s).
    let wait = if normal { every * 2.5 } else { 40.0 };
    tokio::time::sleep(Duration::from_secs_f64(wait)).await;
    let rate = (polls(&n) - mark) as f64 / window.elapsed().as_secs_f64();
    let (rss, threads) = (n.proc_status("VmRSS:"), n.proc_status("Threads:"));
    sampler.abort();
    let peak = peak.load(Ordering::Relaxed).max(threads);
    let per = rss.saturating_sub(base_rss) as f64 / accounts as f64;
    // Smoke: 20 accounts make the per-account figure noise; 10× as elsewhere.
    let budget = RSS_BUDGET_KIB * if o.smoke { 10 } else { 1 };
    let mut names = std::collections::BTreeMap::<String, usize>::new();
    let tasks = std::fs::read_dir(format!("/proc/{}/task", n.daemon.pid())).expect("/proc");
    for t in tasks.flatten() {
        let comm = std::fs::read_to_string(t.path().join("comm")).unwrap_or_default();
        *names.entry(comm.trim().to_owned()).or_default() += 1;
    }
    let level = if normal {
        format!("polled every {every} s ({rate:.1} People polls/s)")
    } else {
        "sleeping".into()
    };
    vec![Row {
        id,
        what: format!(
            "RSS per account, {accounts} accounts {level} ({} → {} MiB, threads {base_threads} → {threads}, peak {peak}, connected in {:.0} s)",
            base_rss / 1024,
            rss / 1024,
            connect.as_secs_f64()
        ),
        value: format!("{per:.1} KiB"),
        budget: format!(
            "under {budget} KiB; threads at most {THREAD_CEILING} at any moment, now {names:?}"
        ),
        ok: per < budget as f64 && peak <= THREAD_CEILING,
    }]
}

/// Plan 6.5, B7: RSS per account.
const RSS_BUDGET_KIB: u64 = 512;

/// A node allowing private DAV servers, its `sync` section set to `sync`,
/// and a fresh tenant on it.
async fn dav_node(o: &Opts, sync: Value) -> (Node, Tenant) {
    let n = Node::start_with(&o.syncd, google::Config::default(), |c| {
        c["dav"] = json!({ "allow_private": true });
        c["sync"] = sync;
    })
    .await;
    let t = n.tenant().await;
    (n, t)
}

async fn connect_dav(n: &Node, t: &Tenant, dav: &FakeDav) {
    let body = json!({
        "profile": "generic", "server_url": dav.server_url(),
        "username": "alice", "password": "secret",
    });
    let (s, v) = n
        .api
        .call("POST", "/v1/accounts/dav", Some(&t.admin), Some(body))
        .await
        .expect("connect");
    assert_eq!(s, 200, "connect: {v}");
}

/// `t` as an iCalendar UTC date-time.
fn ics_time(t: jiff::Timestamp) -> String {
    t.strftime("%Y%m%dT%H%M%SZ").to_string()
}

/// A calendar object of one `component` with `UID` `uid` and the
/// CRLF-terminated property lines `props`.
fn vcal(component: &str, uid: &str, props: &str) -> String {
    format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//musubi//bench//EN\r\nBEGIN:{component}\r\n\
         UID:{uid}\r\nDTSTAMP:20260101T000000Z\r\nSUMMARY:{uid}\r\n{props}END:{component}\r\nEND:VCALENDAR\r\n"
    )
}

/// A 45-minute event, repeating by `rrule` if given.
fn vevent_rule(uid: &str, start: jiff::Timestamp, rrule: Option<&str>) -> String {
    let rule = rrule.map_or(String::new(), |r| format!("RRULE:{r}\r\n"));
    let end = start + SignedDuration::from_mins(45);
    let props = format!(
        "DTSTART:{}\r\nDTEND:{}\r\n{rule}",
        ics_time(start),
        ics_time(end)
    );
    vcal("VEVENT", uid, &props)
}

fn vevent(uid: &str, start: jiff::Timestamp) -> String {
    vevent_rule(uid, start, None)
}

/// The engine's first-impression window (`musubi_engine::Config` defaults):
/// 30 days back, 90 forward.
fn window(now: jiff::Timestamp) -> (jiff::Timestamp, jiff::Timestamp) {
    (
        now - SignedDuration::from_hours(30 * 24),
        now + SignedDuration::from_hours(90 * 24),
    )
}

/// D1: from `POST /v1/accounts/dav` to every event of the window around
/// today readable, 50 ms per fake request, events spread over ±2 years.
/// Measured, not guarded.
async fn d1(o: &Opts) -> Vec<Row> {
    let events = if o.smoke { 500 } else { 2000 };
    let dav = FakeDav::start(DavConfig {
        latency: Duration::from_millis(50),
        ..DavConfig::default()
    })
    .await
    .expect("fake dav");
    let cal = dav.add_collection(Kind::Calendar(&["VEVENT"]), "home", "Home");
    let now = jiff::Timestamp::now();
    let (from, to) = window(now);
    let mut rng = Rng::new(11);
    let mut in_window = 0;
    for i in 0..events {
        let start = now + SignedDuration::from_mins(rng.below(4 * 525_600) as i64 - 2 * 525_600);
        in_window += usize::from(start >= from && start < to);
        dav.put(
            &cal,
            &format!("d1-{i}.ics"),
            &vevent(&format!("d1-{i}"), start),
        );
    }
    let (n, t) = dav_node(o, json!({})).await;
    let t0 = Instant::now();
    connect_dav(&n, &t, &dav).await;
    let path = format!("/v1/entities?from={from}&to={to}&limit=1000&ids_only=true");
    let first_window = loop {
        let v = n.get(&path, &t.read).await;
        if v["occurrences"].as_array().map_or(0, Vec::len) >= in_window {
            break t0.elapsed();
        }
        assert!(t0.elapsed() < Duration::from_secs(120), "no window: {v}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    n.full(&t, Duration::from_secs(600)).await;
    let full = t0.elapsed();
    let report = |what: String, d: Duration, budget: &str| Row {
        id: "D1",
        what,
        value: ms(d),
        budget: budget.into(),
        ok: true,
    };
    vec![
        report(
            format!(
                "connect → window of {in_window} events around today readable, {events} events, 50 ms fake"
            ),
            first_window,
            "report (target < 2 s, not guarded)",
        ),
        report(
            format!("connect → full read of {events} events"),
            full,
            "report",
        ),
    ]
}

/// D2: requests per hour of an unchanged collection at each interest level.
/// Time runs `scale`× faster (every bound divided by it), so one hour is
/// `3600 / scale` s. Guarded: active ≤ 240, normal ≤ 30, sleeping ≤ 6, and a
/// cheap check is one request (every request in the hour is one).
async fn d2(o: &Opts) -> Vec<Row> {
    let scale = if o.smoke { 60.0 } else { 20.0 };
    // The engine's defaults (plan 7.2).
    let (active, normal, sleeping) = ((15.0, 60.0), (120.0, 900.0), (3600.0, 21_600.0));
    let b = |(min, max): (f64, f64)| json!({ "min": min / scale, "max": max / scale });
    // A tenant sleeps 7 days after its last interest; the run puts the
    // sleeping bounds in place of the normal ones instead of waiting.
    let levels = [
        (
            "active",
            json!({ "active": b(active), "normal": b(normal) }),
            240.0,
        ),
        ("normal", json!({ "normal": b(normal) }), 30.0),
        ("sleeping", json!({ "normal": b(sleeping) }), 6.0),
    ];
    let runs = levels.map(|(level, sync, budget)| async move {
        let dav = FakeDav::start(DavConfig::default()).await.expect("fake dav");
        let cal = dav.add_collection(Kind::Calendar(&["VEVENT"]), "sync", "Sync token");
        let book = dav.add_collection(Kind::AddressBook, "ctag", "Ctag only");
        dav.quirk(&book, Quirk::NoSync);
        let hashed = dav.add_collection(Kind::Calendar(&["VEVENT"]), "hashed", "No ETags");
        for q in [Quirk::NoSync, Quirk::NoCtag, Quirk::NoEtag] {
            dav.quirk(&hashed, q);
        }
        let now = jiff::Timestamp::now();
        for i in 0..20 {
            dav.put(&cal, &format!("e{i}.ics"), &vevent(&format!("e{i}"), now));
            dav.put(&hashed, &format!("h{i}.ics"), &vevent(&format!("h{i}"), now));
            let card = format!("BEGIN:VCARD\r\nVERSION:4.0\r\nUID:c{i}\r\nFN:C {i}\r\nEND:VCARD\r\n");
            dav.put(&book, &format!("c{i}.vcf"), &card);
        }
        let (n, t) = dav_node(o, sync).await;
        connect_dav(&n, &t, &dav).await;
        n.full(&t, Duration::from_secs(60)).await;
        let interest = || n.api.call("POST", "/v1/interest", Some(&t.read), None);
        let mark = dav.requests().len();
        let hour = Duration::from_secs_f64(3600.0 / scale);
        let t0 = Instant::now();
        while t0.elapsed() < hour {
            if level == "active" {
                let (s, _) = interest().await.expect("interest");
                assert_eq!(s, 204);
            }
            tokio::time::sleep(Duration::from_secs(30).min(hour.saturating_sub(t0.elapsed()))).await;
        }
        let reqs = dav.requests()[mark..].to_vec();
        let on = |col: &str| reqs.iter().filter(|r| r.path.starts_with(col)).collect::<Vec<_>>();
        let cheap = |r: &&dav::Request| {
            r.label() == "REPORT sync-collection"
                || (r.method == "PROPFIND" && r.depth.as_deref() == Some("0"))
        };
        let mut rows = Vec::new();
        for (col, what) in [(&cal, "sync token"), (&book, "ctag only")] {
            let rs = on(col);
            let all_cheap = rs.iter().all(cheap);
            rows.push(Row {
                id: "D2",
                what: format!("{level}: requests per hour, unchanged collection ({what}), time ×{scale}"),
                value: format!("{}{}", rs.len(), if all_cheap { ", each a cheap check" } else { ", NOT all cheap checks" }),
                budget: format!("≤ {budget}, 1 request per check"),
                ok: rs.len() as f64 <= budget && all_cheap,
            });
        }
        if level == "active" {
            let rs = on(&hashed);
            rows.push(Row {
                id: "D2",
                what: format!("{level}: requests per hour, unchanged collection without ETags or ctag (polled at normal bounds), time ×{scale}"),
                value: rs.len().to_string(),
                budget: "report".into(),
                ok: true,
            });
        }
        rows
    });
    let [a, b, c] = runs;
    let (a, b, c) = tokio::join!(a, b, c);
    [a, b, c].into_iter().flatten().collect()
}

/// D4: a DAV change → its SSE event while the tenant is active, at random
/// moments. Bounded by the level's interval plus one delta; the distribution
/// is reported, not guarded. Smoke runs 60× faster bounds.
async fn d4(o: &Opts) -> Vec<Row> {
    let (scale, samples) = if o.smoke { (60.0, 5) } else { (1.0, 15) };
    let (min, max) = (15.0 / scale, 60.0 / scale);
    let dav = FakeDav::start(DavConfig {
        latency: Duration::from_millis(50),
        ..DavConfig::default()
    })
    .await
    .expect("fake dav");
    let cal = dav.add_collection(Kind::Calendar(&["VEVENT"]), "home", "Home");
    let now = jiff::Timestamp::now();
    dav.put(&cal, "seed.ics", &vevent("seed", now));
    let (n, t) = dav_node(o, json!({ "active": { "min": min, "max": max } })).await;
    connect_dav(&n, &t, &dav).await;
    n.full(&t, Duration::from_secs(60)).await;
    let n = Arc::new(n);
    let keep = {
        let (n, key) = (n.clone(), t.read.clone());
        tokio::spawn(async move {
            loop {
                let _ = n.api.call("POST", "/v1/interest", Some(&key), None).await;
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
        })
    };
    let mut sse = Sse::open(&n.api, &t.read).await;
    let mut last = n.head(&t).await;
    let mut rng = Rng::new(4);
    let mut lat = Vec::new();
    for i in 0..samples {
        let gap = Duration::from_secs_f64(max * 1.5 * rng.below(1000) as f64 / 1000.0);
        tokio::time::sleep(gap).await;
        let t0 = Instant::now();
        dav.put(
            &cal,
            &format!("d4-{i}.ics"),
            &vevent(&format!("d4-{i}"), now),
        );
        last = sse.past(last).await;
        lat.push(t0.elapsed());
    }
    keep.abort();
    vec![Row {
        id: "D4",
        what: format!(
            "DAV change → SSE, active level ({min}–{max} s), 50 ms fake, {samples} changes at random moments: p50 {}, p90 {}",
            ms(pct(&lat, 0.5)),
            ms(pct(&lat, 0.9))
        ),
        value: ms(pct(&lat, 1.0)),
        budget: format!("report (bound: {max} s interval + one delta)"),
        ok: true,
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
    if want("B3t") {
        rows.extend(b3t(&o).await);
    }
    if want("B5") {
        rows.extend(b5(&o).await);
    }
    if want("B6") {
        rows.extend(b6(&o).await);
    }
    if want("B7") {
        rows.extend(b7(&o, false).await);
    }
    if want("B7n") {
        rows.extend(b7(&o, true).await);
    }
    if want("D1") {
        rows.extend(d1(&o).await);
    }
    if want("D2") {
        rows.extend(d2(&o).await);
    }
    if want("D4") {
        rows.extend(d4(&o).await);
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
