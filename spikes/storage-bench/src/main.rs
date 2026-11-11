//! musubi-sync phase 0: SQLite-per-tenant vs shared PostgreSQL. See README.md.

mod datagen;
mod pg;
mod sqlite;

use datagen::{DAY, NCOLL, NOW, Page, Rng, WEEK};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

struct Cfg {
    data: PathBuf,
    pg_url: String,
    tenants: u64,
    events: u64,
    large: u64,
    conc: usize,
    cold: u64,
    queries: usize,
    mixed_secs: u64,
}

fn env<T: std::str::FromStr>(k: &str, d: T) -> T {
    std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d)
}

/// Tenant id of the large tenant loaded with 2,500-item pages (used for reads).
const LARGE: u64 = 100_001;
/// Tenant id of the large tenant loaded with 250-item pages.
const LARGE_250: u64 = 100_000;
/// Incremental-sync transactions measured per backend (300 gave an unstable p99).
const INCR: u32 = 3000;

fn main() {
    // A/B switch: the build sets SQLITE_DEFAULT_MEMSTATUS=0 (.cargo/config.toml); SQLITE_MEMSTATUS=1
    // turns the global malloc statistics mutex back on. Must run before SQLite initialises.
    if let Ok(v) = std::env::var("SQLITE_MEMSTATUS") {
        let on: std::os::raw::c_int = v.parse().expect("SQLITE_MEMSTATUS=0|1");
        // SAFETY: called before any other SQLite API, as sqlite3_config requires.
        let rc = unsafe { rusqlite::ffi::sqlite3_config(rusqlite::ffi::SQLITE_CONFIG_MEMSTATUS, on) };
        assert_eq!(rc, rusqlite::ffi::SQLITE_OK);
        println!("<!-- SQLITE_MEMSTATUS={on} -->");
    }
    for k in ["SQLITE_CKPT", "SQLITE_WAL_HARD_MB"] {
        if let Ok(v) = std::env::var(k) {
            println!("<!-- {k}={v} -->");
        }
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cfg = Cfg {
        data: PathBuf::from(env("BENCH_DATA", "data".to_string())),
        pg_url: env("PG_URL", "host=127.0.0.1 port=55432 user=bench password=bench dbname=bench".to_string()),
        tenants: env("TENANTS", 200),
        events: env("EVENTS", 5000),
        large: env("LARGE", 100_000),
        conc: env("CONC", 16),
        cold: env("COLD", 10_000),
        queries: env("QUERIES", 2000),
        mixed_secs: env("MIXED_SECS", 20),
    };
    let (backend, phases) = match args.split_first() {
        Some((b, p)) if !p.is_empty() => (b.as_str(), p),
        _ => {
            eprintln!("usage: storage-bench sqlite|pg <load|read|mixed|outbox|size|coldopen|connect> (both: acceptmix; sqlite also: ingest-large)...");
            std::process::exit(2);
        }
    };
    println!("| backend | test | n | p50 ms | p99 ms | max ms | notes |\n|---|---|---|---|---|---|---|");
    for ph in phases {
        match backend {
            "sqlite" => sqlite_phase(&cfg, ph),
            "pg" => tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap().block_on(pg_phase(&cfg, ph)),
            _ => panic!("unknown backend {backend}"),
        }
    }
}

// ---------- shared helpers ----------

fn row(b: &str, test: &str, v: &mut [f64], notes: &str) {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let q = |p: f64| if v.is_empty() { f64::NAN } else { v[((v.len() - 1) as f64 * p).round() as usize] };
    let small = if v.len() < 100 { " (n < 100: p99 is the max, not a stable percentile)" } else { "" };
    println!("| {b} | {test} | {} | {:.3} | {:.3} | {:.3} | {notes}{small} |", v.len(), q(0.5), q(0.99), q(1.0));
}

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}

fn gen_many(tenants: &[u64], n: u64, page: usize, version: u32) -> Vec<Vec<Page>> {
    let out = Mutex::new(vec![]);
    let next = AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..16 {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= tenants.len() {
                        break;
                    }
                    let p = datagen::tenant_pages(tenants[i], n, page, version);
                    out.lock().unwrap().push((i, p));
                }
            });
        }
    });
    let mut v = out.into_inner().unwrap();
    v.sort_by_key(|(i, _)| *i);
    v.into_iter().map(|(_, p)| p).collect()
}

fn items(p: &[Vec<Page>]) -> usize {
    p.iter().flatten().map(|p| p.items.len()).sum()
}

struct Query {
    tenant: u64,
    colls: Vec<i64>,
    from: i64,
    to: i64,
}

fn queries(r: &mut Rng, tenants: &[u64], k: usize, span: i64, n: usize) -> Vec<Query> {
    (0..n)
        .map(|_| {
            let mut colls: Vec<i64> = (0..NCOLL).collect();
            for i in 0..k {
                let j = i + r.below((NCOLL as usize - i) as u64) as usize;
                colls.swap(i, j);
            }
            colls.truncate(k);
            let from = r.range(NOW - 26 * WEEK, NOW + 8 * WEEK);
            Query { tenant: tenants[r.below(tenants.len() as u64) as usize], colls, from, to: from + span }
        })
        .collect()
}

fn regular(cfg: &Cfg) -> Vec<u64> {
    (0..cfg.tenants).collect()
}

/// (name, tenants, collections, span, payload)
fn read_matrix(cfg: &Cfg) -> Vec<(String, Vec<u64>, usize, i64, bool)> {
    let mut m = vec![];
    for (tn, ts) in [("regular", regular(cfg)), ("large", vec![LARGE])] {
        for (wn, span) in [("week", WEEK), ("month", 30 * DAY)] {
            for k in [5, 20] {
                m.push((format!("range {tn} {wn} {k} coll"), ts.clone(), k, span, true));
            }
        }
        m.push((format!("range {tn} month 20 coll ids-only"), ts.clone(), 20, 30 * DAY, false));
    }
    m
}

fn rss_mb() -> f64 {
    let s = std::fs::read_to_string("/proc/self/statm").unwrap_or_default();
    let pages: f64 = s.split_whitespace().nth(1).and_then(|x| x.parse().ok()).unwrap_or(0.0);
    pages * 4096.0 / 1e6
}

fn dir_size(p: &Path) -> u64 {
    std::fs::read_dir(p)
        .map(|rd| rd.flatten().filter_map(|e| e.metadata().ok()).filter(|m| m.is_file()).map(|m| m.len()).sum())
        .unwrap_or(0)
}

fn drop_page_cache(p: &Path) {
    use std::os::fd::AsRawFd;
    if let Ok(f) = std::fs::File::open(p) {
        // SAFETY: valid fd for the lifetime of `f`.
        unsafe { libc::posix_fadvise(f.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) };
    }
}

// ---------- SQLite ----------

fn sqlite_phase(cfg: &Cfg, ph: &str) {
    let dir = cfg.data.join("sqlite");
    std::fs::create_dir_all(&dir).unwrap();
    let b = "sqlite";
    match ph.as_ref() {
        "ingest-large" => sqlite_ingest_large(cfg, &dir),
        "load" => {
            sqlite_ingest_large(cfg, &dir);
            let reg = regular(cfg);
            let half = reg.len() / 2;
            for (ts, page) in [(&reg[..half], 250), (&reg[half..], 2500)] {
                let p = gen_many(ts, cfg.events, page, 0);
                let (mut lat, wall, ck) = sqlite_ingest(&dir, ts, &p, cfg.conc);
                let notes = format!("{} tenants x {} writers: {:.0} items/s aggregate; {ck}", ts.len(), cfg.conc, items(&p) as f64 / wall);
                row(b, &format!("ingest concurrent page={page}"), &mut lat, &notes);
            }
            // Same tenants once more, one at a time, for the "without concurrency" comparison.
            for page in [250usize, 2500] {
                let ts: Vec<u64> = (0..8).map(|i| 200_000 + i * 10 + page as u64 / 250).collect();
                let p = gen_many(&ts, cfg.events, page, 0);
                let (mut lat, wall, ck) = sqlite_ingest(&dir, &ts, &p, 1);
                row(b, &format!("ingest solo regular tenants page={page}"), &mut lat, &format!("{:.0} items/s; {ck}", items(&p) as f64 / wall));
            }
            let t = Instant::now();
            for tn in reg.iter().chain(&[LARGE, LARGE_250]) {
                sqlite::open(&sqlite::path(&dir, *tn)).unwrap().execute_batch("ANALYZE").unwrap();
            }
            println!("<!-- sqlite ANALYZE all tenants: {:.1} s -->", t.elapsed().as_secs_f64());
            // Incremental sync: page of 25 changed items + journal + cursor.
            let mut r = Rng::new(42);
            let incs: Vec<(u64, Page)> = (0..INCR)
                .map(|k| {
                    let t = reg[r.below(reg.len() as u64) as usize];
                    (t, datagen::incremental_page(t, cfg.events, r.below(3) as i64, 25, 1000 + k))
                })
                .collect();
            let ck = sqlite::Checkpointer::start(reg.iter().map(|t| sqlite::path(&dir, *t)).collect(), 2);
            let mut lat = vec![];
            let mut ws: HashMap<u64, sqlite::Writer> = HashMap::new();
            for (t, p) in &incs {
                let w = ws.entry(*t).or_insert_with(|| {
                    let mut w = sqlite::Writer::new(sqlite::open(&sqlite::path(&dir, *t)).unwrap()).unwrap();
                    // Warm-up (untimed): unchanged item -> no-op page that prepares all statements.
                    w.apply_page(&datagen::incremental_page(*t, cfg.events, 0, 1, 0)).unwrap();
                    w
                });
                let s = Instant::now();
                assert!(w.apply_page(p).unwrap() > 0);
                lat.push(ms(s));
            }
            row(b, "incremental sync page=25 (txn only)", &mut lat, &format!("writer connection already open; {}", ck.summary()));
        }
        "read" => {
            let mut r = Rng::new(7);
            let conns: HashMap<u64, rusqlite::Connection> =
                regular(cfg).into_iter().chain([LARGE]).map(|t| (t, sqlite::open(&sqlite::path(&dir, t)).unwrap())).collect();
            let conn = |t: u64| &conns[&t];
            for (name, ts, k, span, payload) in read_matrix(cfg) {
                let qs = queries(&mut r, &ts, k, span, cfg.queries);
                for q in qs.iter().take(50) {
                    sqlite::range(conn(q.tenant), &q.colls, q.from, q.to, payload).unwrap(); // warm-up
                }
                let (mut lat, mut rows) = (vec![], 0);
                for q in &qs {
                    let c = conn(q.tenant);
                    let s = Instant::now();
                    rows += sqlite::range(c, &q.colls, q.from, q.to, payload).unwrap();
                    lat.push(ms(s));
                }
                row(b, &name, &mut lat, &format!("avg {:.0} rows", rows as f64 / qs.len() as f64));
            }
            for (tn, ts) in [("regular", regular(cfg)), ("large", vec![LARGE])] {
                for (cn, back) in [("head-100", Some(100)), ("random", None)] {
                    let (mut lat, mut rows) = (vec![], 0);
                    for _ in 0..cfg.queries {
                        let t = ts[r.below(ts.len() as u64) as usize];
                        let c = conn(t);
                        let head = sqlite::head_seq(c).unwrap();
                        let since = match back {
                            Some(b) => head - b,
                            None => r.range(0, head),
                        };
                        let s = Instant::now();
                        rows += sqlite::changes(c, since, 500).unwrap();
                        lat.push(ms(s));
                    }
                    row(b, &format!("changes {tn} since {cn} limit 500"), &mut lat, &format!("avg {:.0} rows", rows as f64 / cfg.queries as f64));
                }
            }
            // Concurrent readers, no writers.
            let lat = Mutex::new(vec![]);
            let t0 = Instant::now();
            std::thread::scope(|s| {
                for th in 0..8u64 {
                    let (dir, lat) = (&dir, &lat);
                    let reg = regular(cfg);
                    s.spawn(move || {
                        let mut r = Rng::new(100 + th);
                        let mut conns: HashMap<u64, rusqlite::Connection> = HashMap::new();
                        let mut l = vec![];
                        for q in queries(&mut r, &reg, 20, 30 * DAY, cfg.queries) {
                            let c = conns.entry(q.tenant).or_insert_with(|| sqlite::open(&sqlite::path(dir, q.tenant)).unwrap());
                            let s = Instant::now();
                            sqlite::range(c, &q.colls, q.from, q.to, true).unwrap();
                            l.push(ms(s));
                        }
                        lat.lock().unwrap().extend(l);
                    });
                }
            });
            let mut lat = lat.into_inner().unwrap();
            let qps = lat.len() as f64 / t0.elapsed().as_secs_f64();
            row(b, "range regular month 20 coll, 8 reader threads", &mut lat, &format!("{qps:.0} queries/s"));
        }
        "mixed" => sqlite_mixed(cfg, &dir),
        "outbox" => {
            let ck = sqlite::Checkpointer::start((0..cfg.conc as u64).map(|t| sqlite::path(&dir, t)).collect(), 2);
            let mut w = sqlite::Writer::new(sqlite::open(&sqlite::path(&dir, 5)).unwrap()).unwrap();
            let mut lat = vec![];
            for k in 0..cfg.queries as i64 {
                let s = Instant::now();
                w.accept_mutation(1 + k % (cfg.events as i64 * 9 / 10), &format!("idem-seq-{}-{k}", std::process::id()), r#"{"title":"renamed"}"#).unwrap();
                lat.push(ms(s));
            }
            row(b, "outbox accept, synchronous=FULL, 1 writer", &mut lat, "mutation+entity rev+journal+outbox in one fsync'd txn");
            let lat = Mutex::new(vec![]);
            std::thread::scope(|s| {
                for t in 0..cfg.conc as u64 {
                    let (dir, lat, ne) = (&dir, &lat, cfg.events as i64 * 9 / 10);
                    s.spawn(move || {
                        let mut w = sqlite::Writer::new(sqlite::open(&sqlite::path(dir, t)).unwrap()).unwrap();
                        let mut l = vec![];
                        for k in 0..300i64 {
                            let s = Instant::now();
                            w.accept_mutation(1 + k * 7 % ne, &format!("idem-conc-{}-{t}-{k}", std::process::id()), r#"{"title":"renamed"}"#).unwrap();
                            l.push(ms(s));
                        }
                        lat.lock().unwrap().extend(l);
                    });
                }
            });
            row(b, &format!("outbox accept, synchronous=FULL, {} tenants concurrently", cfg.conc), &mut lat.into_inner().unwrap(), &ck.summary());
        }
        "size" => {
            let mut total = 0u64;
            let mut large = 0u64;
            for e in std::fs::read_dir(&dir).unwrap().flatten() {
                let p = e.path();
                if p.extension().is_some_and(|x| x == "db") {
                    sqlite::open(&p).unwrap().execute_batch("PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
                }
            }
            total += dir_size(&dir);
            large += std::fs::metadata(sqlite::path(&dir, LARGE)).unwrap().len();
            let one = std::fs::metadata(sqlite::path(&dir, 0)).unwrap().len();
            println!(
                "| sqlite | disk size | - | - | - | - | all tenant files {:.2} GB; large tenant (100k) {:.0} MB; regular tenant (5k) {:.1} MB |",
                total as f64 / 1e9,
                large as f64 / 1e6,
                one as f64 / 1e6
            );
        }
        "coldopen" => sqlite_coldopen(cfg),
        "acceptmix" => sqlite_acceptmix(cfg, &dir),
        _ => panic!("unknown sqlite phase {ph}"),
    }
}

fn sqlite_ingest_large(cfg: &Cfg, dir: &Path) {
    for (t, page) in [(LARGE_250, 250), (LARGE, 2500)] {
        let p = gen_many(&[t], cfg.large, page, 0);
        let (mut lat, wall, ck) = sqlite_ingest(dir, &[t], &p, 1);
        row("sqlite", &format!("ingest solo large tenant page={page}"), &mut lat, &format!("{:.0} items/s; {ck}", items(&p) as f64 / wall));
    }
}

/// Returns page latencies, wall time and the checkpointer summary.
fn sqlite_ingest(dir: &Path, tenants: &[u64], pages: &[Vec<Page>], conc: usize) -> (Vec<f64>, f64, String) {
    let paths: Vec<PathBuf> = tenants.iter().map(|t| sqlite::path(dir, *t)).collect();
    for p in &paths {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", p.display()));
        }
    }
    let ck = sqlite::Checkpointer::start(vec![], 4);
    let lat = Mutex::new(vec![]);
    let next = AtomicUsize::new(0);
    let t0 = Instant::now();
    std::thread::scope(|s| {
        for _ in 0..conc {
            s.spawn(|| {
                let mut l = vec![];
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= tenants.len() {
                        break;
                    }
                    let p = sqlite::path(dir, tenants[i]);
                    let mut w = sqlite::Writer::new(sqlite::create(&p).unwrap()).unwrap();
                    ck.add(p); // only now: schema and WAL mode are in place
                    for page in &pages[i] {
                        let s = Instant::now();
                        w.apply_page(page).unwrap();
                        l.push(ms(s));
                    }
                }
                lat.lock().unwrap().extend(l);
            });
        }
    });
    let wall = t0.elapsed().as_secs_f64();
    (lat.into_inner().unwrap(), wall, ck.summary())
}

/// Writers re-ingest full update pages (250 items) into 8 tenants while 8 readers query the same tenants.
fn sqlite_mixed(cfg: &Cfg, dir: &Path) {
    let ts: Vec<u64> = (0..8).collect();
    let upd: Vec<Vec<Vec<Page>>> = (1..=3).map(|v| gen_many(&ts, cfg.events, 250, v)).collect();
    for writers in [false, true] {
        // Fresh per pass so the WAL maximum belongs to this pass.
        let ck = sqlite::Checkpointer::start(ts.iter().map(|t| sqlite::path(dir, *t)).collect(), 2);
        let stop = AtomicBool::new(false);
        let (rlat, wlat) = (Mutex::new(vec![]), Mutex::new(vec![]));
        let pages = AtomicUsize::new(0);
        std::thread::scope(|s| {
            if writers {
                for (i, t) in ts.iter().enumerate() {
                    let (stop, wlat, upd, pages) = (&stop, &wlat, &upd, &pages);
                    s.spawn(move || {
                        let mut w = sqlite::Writer::new(sqlite::open(&sqlite::path(dir, *t)).unwrap()).unwrap();
                        let mut l = vec![];
                        'outer: for v in (0..).map(|k| k % 3) {
                            for p in &upd[v][i] {
                                if stop.load(Ordering::Relaxed) {
                                    break 'outer;
                                }
                                let st = Instant::now();
                                w.apply_page(p).unwrap();
                                l.push(ms(st));
                                pages.fetch_add(p.items.len(), Ordering::Relaxed);
                            }
                        }
                        wlat.lock().unwrap().extend(l);
                    });
                }
            }
            for th in 0..8u64 {
                let (stop, rlat, ts) = (&stop, &rlat, &ts);
                s.spawn(move || {
                    let mut r = Rng::new(500 + th);
                    let mut conns: HashMap<u64, rusqlite::Connection> = HashMap::new();
                    let mut l = vec![];
                    while !stop.load(Ordering::Relaxed) {
                        let q = queries(&mut r, ts, 20, 30 * DAY, 1).pop().unwrap();
                        let c = conns.entry(q.tenant).or_insert_with(|| sqlite::open(&sqlite::path(dir, q.tenant)).unwrap());
                        let st = Instant::now();
                        sqlite::range(c, &q.colls, q.from, q.to, true).unwrap();
                        let head = sqlite::head_seq(c).unwrap();
                        sqlite::changes(c, head - 100, 500).unwrap();
                        l.push(ms(st));
                    }
                    rlat.lock().unwrap().extend(l);
                });
            }
            std::thread::sleep(Duration::from_secs(cfg.mixed_secs));
            stop.store(true, Ordering::Relaxed);
        });
        let tag = if writers { "with 8 writers" } else { "baseline, no writers" };
        let wp = pages.load(Ordering::Relaxed);
        let notes = format!("8 readers, same 8 tenants; {}", ck.summary());
        row("sqlite", &format!("mixed read (month range + changes) {tag}"), &mut rlat.into_inner().unwrap(), &notes);
        if writers {
            let notes = format!("{:.0} items/s while reading", wp as f64 / cfg.mixed_secs as f64);
            row("sqlite", "mixed write page=250 updates", &mut wlat.into_inner().unwrap(), &notes);
        }
    }
}

/// Outbox accept on the tenant that is being ingested. Large tenant, page 250, background checkpointer, ~50 accepts/s.
/// Pass 2 of each mode adds a reader that holds a snapshot for 1.5 s, 0.2 s apart. Accept latency = enqueue -> commit.
/// Modes:
/// - `same`: accepts go through the tenant's single writer (actor), queued and run before the next ingest page;
/// - `split` (mitigation A): outbox + mutations in a separate file with its own writer thread; the tenant writer
///   projects entity rev + journal before its next page;
/// - `chunked` (mitigation B): like `same`, but every page is applied as 10 transactions of 25 items and queued
///   accepts run between them.
fn sqlite_acceptmix(cfg: &Cfg, dir: &Path) {
    const T: u64 = 300_000;
    const CHUNK: usize = 25;
    // Initial load, then two full update rounds: ~3x the large-tenant ingest, long enough for several reader cycles.
    let pages: Vec<Page> = (0..3).flat_map(|v| gen_many(&[T], cfg.large, 250, v).pop().unwrap()).collect();
    let pre = pages.len() / 30; // untimed: entities that the accepts mutate
    let n: usize = pages[pre..].iter().map(|p| p.items.len()).sum();
    for mode in ["same", "split", "chunked"] {
        // One unit = one ingest page = the transactions it is applied in.
        let units: Vec<Vec<Page>> = pages[pre..]
            .iter()
            .map(|p| match mode {
                // shortcut: every chunk sets the page's cursor; in production only the last one would advance it.
                "chunked" => p.items.chunks(CHUNK).map(|c| Page { coll: p.coll, cursor: p.cursor.clone(), items: c.to_vec() }).collect(),
                _ => vec![p.clone()],
            })
            .collect();
        for long_reader in [false, true] {
            let p = sqlite::path(dir, T);
            let op = sqlite::outbox_path(dir, T);
            for f in [&p, &op] {
                for suffix in ["", "-wal", "-shm"] {
                    let _ = std::fs::remove_file(format!("{}{suffix}", f.display()));
                }
            }
            let mut w = sqlite::Writer::new(sqlite::create(&p).unwrap()).unwrap();
            let mut ne = 0;
            for pg in &pages[..pre] {
                ne += w.apply_page(pg).unwrap();
            }
            let ent = move |k: i64| 1 + k * 7 % ne as i64;
            let idem = |k: i64| format!("idem-mix-{}-{mode}-{long_reader}-{k}", std::process::id());
            let mut files = vec![p.clone()];
            if mode == "split" {
                drop(sqlite::OutboxWriter::new(&op, &p).unwrap()); // create before the checkpointer samples it
                files.push(op.clone());
            }
            let ck = sqlite::Checkpointer::start(files, 1);
            let done = AtomicBool::new(false);
            let (tx, rx) = std::sync::mpsc::channel::<Instant>();
            let (ptx, prx) = std::sync::mpsc::channel::<(i64, i64)>(); // split: accepted (mutation id, entity) to project
            let (mut alat, mut ilat) = (vec![], vec![]);
            let t0 = Instant::now();
            std::thread::scope(|s| {
                let done = &done;
                s.spawn(move || {
                    // owns tx: the final drain ends when this thread exits
                    while !done.load(Ordering::Relaxed) {
                        tx.send(Instant::now()).unwrap();
                        std::thread::sleep(Duration::from_millis(20));
                    }
                });
                if long_reader {
                    let p = &p;
                    s.spawn(move || {
                        let c = sqlite::open(p).unwrap();
                        while !done.load(Ordering::Relaxed) {
                            c.execute_batch("BEGIN").unwrap();
                            let _: i64 = c.query_row("SELECT count(*) FROM changes", [], |r| r.get(0)).unwrap(); // takes the snapshot
                            std::thread::sleep(Duration::from_millis(1500));
                            c.execute_batch("COMMIT").unwrap();
                            std::thread::sleep(Duration::from_millis(200));
                        }
                    });
                }
                if mode == "split" {
                    let (p, op) = (&p, &op);
                    let acc = s.spawn(move || {
                        let mut ow = sqlite::OutboxWriter::new(op, p).unwrap();
                        let mut l = vec![];
                        for (k, enq) in rx.iter().enumerate() {
                            let k = k as i64;
                            let mid = ow.accept(ent(k), &idem(k), r#"{"title":"renamed"}"#).unwrap();
                            l.push(ms(enq));
                            ptx.send((mid, ent(k))).unwrap();
                        }
                        l
                    });
                    let project = |w: &mut sqlite::Writer, m: Vec<(i64, i64)>| if !m.is_empty() { w.project(&m).unwrap() };
                    for unit in &units {
                        project(&mut w, prx.try_iter().collect());
                        let st = Instant::now();
                        w.apply_page(&unit[0]).unwrap();
                        ilat.push(ms(st));
                    }
                    done.store(true, Ordering::Relaxed);
                    alat = acc.join().unwrap();
                    project(&mut w, prx.try_iter().collect());
                } else {
                    let mut k = 0i64;
                    let mut accept = |w: &mut sqlite::Writer, enq: Instant, alat: &mut Vec<f64>| {
                        w.accept_mutation(ent(k), &idem(k), r#"{"title":"renamed"}"#).unwrap();
                        alat.push(ms(enq));
                        k += 1;
                    };
                    for unit in &units {
                        let st = Instant::now();
                        for txn in unit {
                            while let Ok(enq) = rx.try_recv() {
                                accept(&mut w, enq, &mut alat);
                            }
                            w.apply_page(txn).unwrap();
                        }
                        ilat.push(ms(st));
                    }
                    done.store(true, Ordering::Relaxed);
                    while let Ok(enq) = rx.recv() {
                        accept(&mut w, enq, &mut alat);
                    }
                }
            });
            if mode == "split" {
                let (acc, proj): (i64, i64) = (
                    rusqlite::Connection::open(&op).unwrap().query_row("SELECT count(*) FROM mutations", [], |r| r.get(0)).unwrap(),
                    w.c.query_row("SELECT v FROM meta WHERE k='projected'", [], |r| r.get(0)).unwrap(),
                );
                assert_eq!(acc, proj, "every accepted mutation is projected (ids are 1..n)");
            }
            let wall = t0.elapsed().as_secs_f64();
            let tag = if long_reader { "long reader (1.5 s snapshot)" } else { "no reader" };
            let how = match mode {
                "same" => "same writer as ingest",
                "split" => "separate outbox file and writer, projected by the ingest writer",
                _ => "same writer, ingest page split into 25-item txns",
            };
            row("sqlite", &format!("acceptmix[{mode}] accept during ingest, {tag}"), &mut alat, &format!("enqueue->commit, synchronous=FULL, {how}; {}", ck.summary()));
            row("sqlite", &format!("acceptmix[{mode}] ingest page=250, {tag}"), &mut ilat, &format!("{:.0} items/s incl. accepts", n as f64 / wall));
        }
    }
}

fn sqlite_coldopen(cfg: &Cfg) {
    let dir = cfg.data.join("sqlite-cold");
    std::fs::create_dir_all(&dir).unwrap();
    let n = cfg.cold;
    let next = AtomicUsize::new(0);
    let t0 = Instant::now();
    std::thread::scope(|s| {
        for _ in 0..cfg.conc {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed) as u64;
                    if i >= n {
                        break;
                    }
                    let p = sqlite::path(&dir, i);
                    if p.exists() {
                        continue;
                    }
                    let mut w = sqlite::Writer::new(sqlite::create(&p).unwrap()).unwrap();
                    for page in datagen::tenant_pages(i, 50, 250, 0) {
                        w.apply_page(&page).unwrap();
                    }
                }
            });
        }
    });
    println!("<!-- created {n} small tenant files in {:.1} s, {:.0} MB -->", t0.elapsed().as_secs_f64(), dir_size(&dir) as f64 / 1e6);
    let mut r = Rng::new(99);
    let mut order: Vec<u64> = (0..n).collect();
    for i in (1..order.len()).rev() {
        order.swap(i, r.below(i as u64 + 1) as usize);
    }
    let probe = |c: &rusqlite::Connection| {
        let _: i64 = c.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        sqlite::range(c, &[0, 1, 2, 3, 4], NOW, NOW + WEEK, true).unwrap();
    };
    for cold in [true, false] {
        let (mut open, mut close) = (vec![], vec![]);
        let t = Instant::now();
        for &i in &order {
            let p = sqlite::path(&dir, i);
            if cold {
                drop_page_cache(&p);
            }
            let s = Instant::now();
            let c = sqlite::open(&p).unwrap();
            probe(&c);
            open.push(ms(s));
            let s = Instant::now();
            drop(c);
            close.push(ms(s));
        }
        let tag = if cold { "cold (page cache dropped)" } else { "warm page cache" };
        let total = format!("{n} tenants in {:.1} s", t.elapsed().as_secs_f64());
        row("sqlite", &format!("open + pragmas + first query, {tag}"), &mut open, &total);
        row("sqlite", &format!("close, {tag}"), &mut close, "");
    }
    // LRU of open handles under a Zipf-like access pattern.
    let cap = 1000usize;
    let ranks: Vec<f64> = (1..=n).map(|k| 1.0 / (k as f64).powf(1.1)).collect();
    let sum: f64 = ranks.iter().sum();
    let mut cdf = Vec::with_capacity(ranks.len());
    let mut acc = 0.0;
    for x in ranks {
        acc += x / sum;
        cdf.push(acc);
    }
    let rss0 = rss_mb();
    // shortcut: O(cap) eviction scan; a real LRU would use a linked map.
    let mut lru: HashMap<u64, (rusqlite::Connection, u64)> = HashMap::new();
    let (mut lat, mut hits) = (vec![], 0);
    let reqs = 100_000;
    for tick in 0..reqs as u64 {
        let u = (r.next() >> 11) as f64 / (1u64 << 53) as f64;
        let t = order[cdf.partition_point(|c| *c < u).min(n as usize - 1)];
        let s = Instant::now();
        if let Some(e) = lru.get_mut(&t) {
            e.1 = tick;
            hits += 1;
        } else {
            if lru.len() >= cap {
                let victim = *lru.iter().min_by_key(|(_, (_, at))| *at).unwrap().0;
                lru.remove(&victim);
            }
            lru.insert(t, (sqlite::open(&sqlite::path(&dir, t)).unwrap(), tick));
        }
        probe(&lru[&t].0);
        lat.push(ms(s));
    }
    let notes = format!(
        "LRU {cap} of {n}, Zipf s=1.1, hit rate {:.1} %, RSS +{:.0} MB with {} open",
        hits as f64 * 100.0 / reqs as f64,
        rss_mb() - rss0,
        lru.len()
    );
    row("sqlite", "request via LRU of open DBs (open on miss + query)", &mut lat, &notes);
    drop(lru);
    // Lazy schema migration cost, paid per tenant at open.
    let mut lat = vec![];
    for &i in &order {
        let s = Instant::now();
        let c = sqlite::open(&sqlite::path(&dir, i)).unwrap();
        let v: i64 = c.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        if v == 0 {
            c.execute_batch("BEGIN; ALTER TABLE items ADD COLUMN mapping_version INTEGER NOT NULL DEFAULT 1; PRAGMA user_version=1; COMMIT;").unwrap();
        }
        lat.push(ms(s));
    }
    row("sqlite", "schema migration at open (ADD COLUMN)", &mut lat, &format!("{n} tenant files"));
}

// ---------- PostgreSQL ----------

async fn pg_phase(cfg: &Cfg, ph: &str) {
    let b = "pg";
    let url = cfg.pg_url.as_str();
    match ph {
        "load" => {
            let c = pg::connect(url, false).await.unwrap();
            c.batch_execute(
                "DROP TABLE IF EXISTS tenants, collections, raw, items, entities, placements, occurrences, changes, mutations, outbox",
            )
            .await
            .unwrap();
            c.batch_execute(pg::SCHEMA).await.unwrap();
            for (t, page) in [(LARGE_250, 250), (LARGE, 2500)] {
                let p = Arc::new(gen_many(&[t], cfg.large, page, 0));
                let (mut lat, wall) = pg_ingest(url, &[t], p.clone(), 1).await;
                row(b, &format!("ingest solo large tenant page={page}"), &mut lat, &format!("{:.0} items/s", items(&p) as f64 / wall));
            }
            let reg = regular(cfg);
            let half = reg.len() / 2;
            for (ts, page) in [(&reg[..half], 250), (&reg[half..], 2500)] {
                let p = Arc::new(gen_many(ts, cfg.events, page, 0));
                let (mut lat, wall) = pg_ingest(url, ts, p.clone(), cfg.conc).await;
                let notes = format!("{} tenants x {} writers: {:.0} items/s aggregate", ts.len(), cfg.conc, items(&p) as f64 / wall);
                row(b, &format!("ingest concurrent page={page}"), &mut lat, &notes);
            }
            for page in [250usize, 2500] {
                let ts: Vec<u64> = (0..8).map(|i| 200_000 + i * 10 + page as u64 / 250).collect();
                let p = Arc::new(gen_many(&ts, cfg.events, page, 0));
                let (mut lat, wall) = pg_ingest(url, &ts, p.clone(), 1).await;
                row(b, &format!("ingest solo regular tenants page={page}"), &mut lat, &format!("{:.0} items/s", items(&p) as f64 / wall));
            }
            let t = Instant::now();
            c.batch_execute("VACUUM ANALYZE").await.unwrap();
            println!("<!-- pg VACUUM ANALYZE: {:.1} s -->", t.elapsed().as_secs_f64());
            let mut r = Rng::new(42);
            let incs: Vec<(u64, Page)> = (0..INCR)
                .map(|k| {
                    let t = reg[r.below(reg.len() as u64) as usize];
                    (t, datagen::incremental_page(t, cfg.events, r.below(3) as i64, 25, 1000 + k))
                })
                .collect();
            let mut w = pg::Writer::new(pg::connect(url, true).await.unwrap()).await.unwrap();
            let mut lat = vec![];
            for (t, p) in &incs {
                w.use_tenant(*t as i64).await.unwrap();
                let s = Instant::now();
                assert!(w.apply_page(p).await.unwrap() > 0);
                lat.push(ms(s));
            }
            row(b, "incremental sync page=25 (txn only)", &mut lat, "writer connection already open");
        }
        "read" => {
            let rd = pg::Reader::new(pg::connect(url, false).await.unwrap()).await.unwrap();
            let mut r = Rng::new(7);
            for (name, ts, k, span, payload) in read_matrix(cfg) {
                let qs = queries(&mut r, &ts, k, span, cfg.queries);
                for q in qs.iter().take(50) {
                    rd.range(q.tenant as i64, &q.colls, q.from, q.to, payload).await.unwrap();
                }
                let (mut lat, mut rows) = (vec![], 0);
                for q in &qs {
                    let s = Instant::now();
                    rows += rd.range(q.tenant as i64, &q.colls, q.from, q.to, payload).await.unwrap();
                    lat.push(ms(s));
                }
                row(b, &name, &mut lat, &format!("avg {:.0} rows", rows as f64 / qs.len() as f64));
            }
            for (tn, ts) in [("regular", regular(cfg)), ("large", vec![LARGE])] {
                for (cn, back) in [("head-100", Some(100)), ("random", None)] {
                    let (mut lat, mut rows) = (vec![], 0);
                    for _ in 0..cfg.queries {
                        let t = ts[r.below(ts.len() as u64) as usize] as i64;
                        let head = rd.head_seq(t).await.unwrap();
                        let since = match back {
                            Some(b) => head - b,
                            None => r.range(0, head),
                        };
                        let s = Instant::now();
                        rows += rd.changes(t, since, 500).await.unwrap();
                        lat.push(ms(s));
                    }
                    row(b, &format!("changes {tn} since {cn} limit 500"), &mut lat, &format!("avg {:.0} rows", rows as f64 / cfg.queries as f64));
                }
            }
            let t0 = Instant::now();
            let mut hs = vec![];
            for th in 0..8u64 {
                let (url, reg, n) = (url.to_string(), regular(cfg), cfg.queries);
                hs.push(tokio::spawn(async move {
                    let rd = pg::Reader::new(pg::connect(&url, false).await.unwrap()).await.unwrap();
                    let mut r = Rng::new(100 + th);
                    let mut l = vec![];
                    for q in queries(&mut r, &reg, 20, 30 * DAY, n) {
                        let s = Instant::now();
                        rd.range(q.tenant as i64, &q.colls, q.from, q.to, true).await.unwrap();
                        l.push(ms(s));
                    }
                    l
                }));
            }
            let mut lat = vec![];
            for h in hs {
                lat.extend(h.await.unwrap());
            }
            let qps = lat.len() as f64 / t0.elapsed().as_secs_f64();
            row(b, "range regular month 20 coll, 8 reader connections", &mut lat, &format!("{qps:.0} queries/s"));
        }
        "mixed" => pg_mixed(cfg).await,
        "acceptmix" => pg_acceptmix(cfg).await,
        "outbox" => {
            let ents = pg_entity_ids(url, 5).await;
            let mut w = pg::Writer::new(pg::connect(url, false).await.unwrap()).await.unwrap();
            w.use_tenant(5).await.unwrap();
            let mut lat = vec![];
            for k in 0..cfg.queries as i64 {
                let s = Instant::now();
                w.accept_mutation(ents[k as usize % ents.len()], &format!("idem-seq-{}-{k}", std::process::id()), r#"{"title":"renamed"}"#).await.unwrap();
                lat.push(ms(s));
            }
            row(b, "outbox accept, synchronous_commit=on, 1 writer", &mut lat, "mutation+entity rev+journal+outbox in one fsync'd txn");
            let mut hs = vec![];
            for t in 0..cfg.conc as i64 {
                let url = url.to_string();
                hs.push(tokio::spawn(async move {
                    let ents = pg_entity_ids(&url, t).await;
                    let mut w = pg::Writer::new(pg::connect(&url, false).await.unwrap()).await.unwrap();
                    w.use_tenant(t).await.unwrap();
                    let mut l = vec![];
                    for k in 0..300i64 {
                        let s = Instant::now();
                        w.accept_mutation(ents[k as usize * 7 % ents.len()], &format!("idem-conc-{}-{t}-{k}", std::process::id()), r#"{"title":"renamed"}"#).await.unwrap();
                        l.push(ms(s));
                    }
                    l
                }));
            }
            let mut lat = vec![];
            for h in hs {
                lat.extend(h.await.unwrap());
            }
            row(b, &format!("outbox accept, synchronous_commit=on, {} tenants concurrently", cfg.conc), &mut lat, "");
        }
        "size" => {
            let c = pg::connect(url, false).await.unwrap();
            c.batch_execute("CHECKPOINT").await.unwrap();
            let db: i64 = c.query_one("SELECT pg_database_size(current_database())", &[]).await.unwrap().get(0);
            let rows = c
                .query(
                    "SELECT relname::text, pg_total_relation_size(oid) FROM pg_class
                     WHERE relkind='r' AND relnamespace='public'::regnamespace ORDER BY 2 DESC",
                    &[],
                )
                .await
                .unwrap();
            let per: Vec<String> =
                rows.iter().map(|r| format!("{} {:.0} MB", r.get::<_, String>(0), r.get::<_, i64>(1) as f64 / 1e6)).collect();
            println!("| pg | disk size | - | - | - | - | database {:.2} GB (excl. WAL); {} |", db as f64 / 1e9, per.join(", "));
        }
        "connect" => {
            let mut lat = vec![];
            for i in 0..200 {
                let s = Instant::now();
                let rd = pg::Reader::new(pg::connect(url, false).await.unwrap()).await.unwrap();
                rd.range(i % cfg.tenants as i64, &[0, 1, 2, 3, 4], NOW, NOW + WEEK, true).await.unwrap();
                lat.push(ms(s));
            }
            row(b, "new connection + prepare + first query", &mut lat, "what a pool saves; compare with SQLite cold open");
        }
        _ => panic!("unknown pg phase {ph}"),
    }
}

/// PG allocates item/entity/placement ids from one per-tenant counter, so entity ids are not 1..n.
async fn pg_entity_ids(url: &str, tenant: i64) -> Vec<i64> {
    let c = pg::connect(url, false).await.unwrap();
    c.query("SELECT id FROM entities WHERE tenant_id=$1 ORDER BY id", &[&tenant]).await.unwrap().iter().map(|r| r.get(0)).collect()
}

async fn pg_ingest(url: &str, tenants: &[u64], pages: Arc<Vec<Vec<Page>>>, conc: usize) -> (Vec<f64>, f64) {
    let c = pg::connect(url, false).await.unwrap();
    for t in tenants {
        c.execute("DELETE FROM tenants WHERE id=$1", &[&(*t as i64)]).await.unwrap();
        pg::create_tenant(&c, *t as i64).await.unwrap();
    }
    let next = Arc::new(AtomicUsize::new(0));
    let tenants = Arc::new(tenants.to_vec());
    let t0 = Instant::now();
    let mut hs = vec![];
    for _ in 0..conc {
        let (url, next, tenants, pages) = (url.to_string(), next.clone(), tenants.clone(), pages.clone());
        hs.push(tokio::spawn(async move {
            let mut w = pg::Writer::new(pg::connect(&url, true).await.unwrap()).await.unwrap();
            let mut l = vec![];
            loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= tenants.len() {
                    break;
                }
                w.use_tenant(tenants[i] as i64).await.unwrap();
                for page in &pages[i] {
                    let s = Instant::now();
                    w.apply_page(page).await.unwrap();
                    l.push(ms(s));
                }
            }
            l
        }));
    }
    let mut lat = vec![];
    for h in hs {
        lat.extend(h.await.unwrap());
    }
    (lat, t0.elapsed().as_secs_f64())
}

/// PG counterpart of `sqlite_acceptmix` (mode `same` scenario): the same pages and ~50 accepts/s on one tenant, but
/// accept runs on its own connection (synchronous_commit=on) next to the ingest connection (off), so it waits only on
/// row locks the ingest transaction holds (tenants.seq, overlapping entities). Pass 2: a REPEATABLE READ snapshot
/// held 1.5 s, 0.2 s apart. Each pass uses a fresh tenant id.
async fn pg_acceptmix(cfg: &Cfg) {
    const T: u64 = 300_000;
    let url = cfg.pg_url.clone();
    let pages: Arc<Vec<Page>> = Arc::new((0..3).flat_map(|v| gen_many(&[T], cfg.large, 250, v).pop().unwrap()).collect());
    let pre = pages.len() / 30;
    let n: usize = pages[pre..].iter().map(|p| p.items.len()).sum();
    let c = pg::connect(&url, false).await.unwrap();
    c.batch_execute(pg::SCHEMA).await.unwrap();
    for (i, long_reader) in [false, true].into_iter().enumerate() {
        let t = (T + i as u64) as i64;
        c.execute("DELETE FROM tenants WHERE id=$1", &[&t]).await.unwrap();
        for tbl in ["collections", "raw", "items", "entities", "placements", "occurrences", "changes", "mutations", "outbox"] {
            c.execute(&format!("DELETE FROM {tbl} WHERE tenant_id=$1"), &[&t]).await.unwrap();
        }
        pg::create_tenant(&c, t).await.unwrap();
        let mut w = pg::Writer::new(pg::connect(&url, true).await.unwrap()).await.unwrap();
        w.use_tenant(t).await.unwrap();
        for p in &pages[..pre] {
            w.apply_page(p).await.unwrap();
        }
        let ents = pg_entity_ids(&url, t).await;
        let mut a = pg::Writer::new(pg::connect(&url, false).await.unwrap()).await.unwrap();
        a.use_tenant(t).await.unwrap();
        // shortcut: disjoint seq range instead of one per-tenant allocator; a real design needs a commit-order-safe cursor.
        a.seq += 1 << 40;
        let done = Arc::new(AtomicBool::new(false));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Instant>();
        let d = done.clone();
        let generator = tokio::spawn(async move {
            while !d.load(Ordering::Relaxed) {
                tx.send(Instant::now()).unwrap();
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        });
        let pid = std::process::id();
        let acc = tokio::spawn(async move {
            let mut l = vec![];
            let mut k = 0usize;
            while let Some(enq) = rx.recv().await {
                a.accept_mutation(ents[k * 7 % ents.len()], &format!("idem-mix-{pid}-{t}-{k}"), r#"{"title":"renamed"}"#).await.unwrap();
                l.push(ms(enq));
                k += 1;
            }
            l
        });
        let reader = long_reader.then(|| {
            let (url, d) = (url.clone(), done.clone());
            tokio::spawn(async move {
                let r = pg::connect(&url, false).await.unwrap();
                while !d.load(Ordering::Relaxed) {
                    r.batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ").await.unwrap();
                    r.query_one("SELECT count(*) FROM changes WHERE tenant_id=$1", &[&t]).await.unwrap(); // takes the snapshot
                    tokio::time::sleep(Duration::from_millis(1500)).await;
                    r.batch_execute("COMMIT").await.unwrap();
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            })
        });
        let t0 = Instant::now();
        let mut ilat = vec![];
        for p in &pages[pre..] {
            let st = Instant::now();
            w.apply_page(p).await.unwrap();
            ilat.push(ms(st));
        }
        done.store(true, Ordering::Relaxed);
        generator.await.unwrap();
        let mut alat = acc.await.unwrap();
        let wall = t0.elapsed().as_secs_f64();
        if let Some(r) = reader {
            r.await.unwrap();
        }
        let tag = if long_reader { "long reader (1.5 s snapshot)" } else { "no reader" };
        row("pg", &format!("acceptmix accept during ingest, {tag}"), &mut alat, "enqueue->commit, synchronous_commit=on, own connection next to the ingest one");
        row("pg", &format!("acceptmix ingest page=250, {tag}"), &mut ilat, &format!("{:.0} items/s incl. accepts, synchronous_commit=off", n as f64 / wall));
    }
}

async fn pg_mixed(cfg: &Cfg) {
    let ts: Vec<u64> = (0..8).collect();
    let upd: Arc<Vec<Vec<Vec<Page>>>> = Arc::new((1..=3).map(|v| gen_many(&ts, cfg.events, 250, v)).collect());
    for writers in [false, true] {
        let stop = Arc::new(AtomicBool::new(false));
        let pages = Arc::new(AtomicUsize::new(0));
        let mut wh = vec![];
        if writers {
            for (i, t) in ts.iter().enumerate() {
                let (url, stop, upd, pages, t) = (cfg.pg_url.clone(), stop.clone(), upd.clone(), pages.clone(), *t as i64);
                wh.push(tokio::spawn(async move {
                    let mut w = pg::Writer::new(pg::connect(&url, true).await.unwrap()).await.unwrap();
                    w.use_tenant(t).await.unwrap();
                    let mut l = vec![];
                    'outer: for v in (0..).map(|k| k % 3) {
                        for p in &upd[v][i] {
                            if stop.load(Ordering::Relaxed) {
                                break 'outer;
                            }
                            let st = Instant::now();
                            w.apply_page(p).await.unwrap();
                            l.push(ms(st));
                            pages.fetch_add(p.items.len(), Ordering::Relaxed);
                        }
                    }
                    l
                }));
            }
        }
        let mut rh = vec![];
        for th in 0..8u64 {
            let (url, stop, ts) = (cfg.pg_url.clone(), stop.clone(), ts.clone());
            rh.push(tokio::spawn(async move {
                let rd = pg::Reader::new(pg::connect(&url, false).await.unwrap()).await.unwrap();
                let mut r = Rng::new(500 + th);
                let mut l = vec![];
                while !stop.load(Ordering::Relaxed) {
                    let q = queries(&mut r, &ts, 20, 30 * DAY, 1).pop().unwrap();
                    let t = q.tenant as i64;
                    let st = Instant::now();
                    rd.range(t, &q.colls, q.from, q.to, true).await.unwrap();
                    let head = rd.head_seq(t).await.unwrap();
                    rd.changes(t, head - 100, 500).await.unwrap();
                    l.push(ms(st));
                }
                l
            }));
        }
        tokio::time::sleep(Duration::from_secs(cfg.mixed_secs)).await;
        stop.store(true, Ordering::Relaxed);
        let (mut rl, mut wl) = (vec![], vec![]);
        for h in rh {
            rl.extend(h.await.unwrap());
        }
        for h in wh {
            wl.extend(h.await.unwrap());
        }
        let tag = if writers { "with 8 writers" } else { "baseline, no writers" };
        row("pg", &format!("mixed read (month range + changes) {tag}"), &mut rl, "8 readers, same 8 tenants");
        if writers {
            let notes = format!("{:.0} items/s while reading", pages.load(Ordering::Relaxed) as f64 / cfg.mixed_secs as f64);
            row("pg", "mixed write page=250 updates", &mut wl, &notes);
        }
    }
}
