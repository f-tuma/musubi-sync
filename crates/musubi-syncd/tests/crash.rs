//! Plan 6.3: `kill -9` of the whole `musubi-syncd` binary over the fake
//! Google server, at random moments and at named crash points (feature
//! `crash-points`, see `musubi_store_sqlite::crash`), then a restart and the
//! invariants of 6.3 once the node is quiet.
//!
//! `MUSUBI_SYNCD_CRASH_KILLS` (20) kills per run, `MUSUBI_SYNCD_CRASH_RUNS`
//! (1) runs, `MUSUBI_SYNCD_CRASH_JOBS` (4) runs at a time, `MUSUBI_CRASH_SEED`
//! the seed of run 0 (run `i` uses seed + i). Every failing run prints its
//! seed and round; `MUSUBI_CRASH_SEED=<seed> MUSUBI_SYNCD_CRASH_RUNS=1`
//! replays its plan (the kill timing itself is not reproducible).
//!
//! Run: `cargo test -p musubi-syncd --features crash-points --release --test crash`.

mod common;

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::whole::{Follower, World, check_store, head};
use musubi_testkit::google::{self, Change, Seed, Step};
use musubi_testkit::harness::{ADMIN_KEY, CRASH_MARK, Rng};
use serde_json::json;

/// Named crash points; `renewal` is a kill right after the node asked the
/// fake for a new channel.
const POINTS: [&str; 4] = ["apply-page", "truncate", "commit-sse", "backup"];

#[derive(Debug, Clone, Copy)]
enum Kill {
    /// After this many milliseconds of load.
    Random(u64),
    /// At the `n`-th hit of a crash point.
    Point(&'static str, u64),
    /// While a watch channel is renewed.
    Renewal,
}

fn plan(round: u64, rng: &mut Rng) -> Kill {
    match round {
        r if r % 20 == 19 => Kill::Renewal,
        r if r % 2 == 0 => Kill::Random(rng.below(1500)),
        r => {
            let name = POINTS[(r / 2 % 4) as usize];
            // A backup runs once per round.
            let n = if name == "backup" {
                1
            } else {
                1 + rng.below(3)
            };
            Kill::Point(name, n)
        }
    }
}

fn var(name: &str, default: u64) -> u64 {
    std::env::var(name).map_or(default, |v| v.parse().expect(name))
}

/// A WAL reset limit low enough that small syncs reach `TRUNCATE`.
fn env() -> Vec<(&'static str, String)> {
    vec![("MUSUBI_WAL_HARD", "65536".into())]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn syncd_survives_sigkill_at_random_and_named_points() {
    let runs = var("MUSUBI_SYNCD_CRASH_RUNS", 1);
    let kills = var("MUSUBI_SYNCD_CRASH_KILLS", 20);
    let jobs = var("MUSUBI_SYNCD_CRASH_JOBS", 4).min(runs);
    let base = var(
        "MUSUBI_CRASH_SEED",
        jiff::Timestamp::now().as_nanosecond() as u64,
    );
    println!("crash: seed {base}, {runs} runs of {kills} kills, {jobs} at a time");

    let next = Arc::new(AtomicU64::new(0));
    let reached = Arc::new(Mutex::new(BTreeSet::new()));
    let failures = Arc::new(Mutex::new(Vec::new()));
    let mut workers = Vec::new();
    for _ in 0..jobs {
        let (next, reached, failures) = (next.clone(), reached.clone(), failures.clone());
        workers.push(tokio::spawn(async move {
            loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= runs {
                    return;
                }
                let seed = base.wrapping_add(i);
                let round = Arc::new(AtomicU64::new(0));
                let r = tokio::spawn(run(seed, kills, round.clone(), reached.clone())).await;
                if let Err(e) = r {
                    let msg = match e.try_into_panic() {
                        Ok(p) => p
                            .downcast_ref::<String>()
                            .cloned()
                            .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
                            .unwrap_or_default(),
                        Err(e) => e.to_string(),
                    };
                    let round = round.load(Ordering::Relaxed);
                    eprintln!("crash: FAILED seed {seed} round {round}: {msg}");
                    failures.lock().expect("failures").push((seed, round));
                }
            }
        }));
    }
    for w in workers {
        w.await.expect("worker");
    }
    let failures = failures.lock().expect("failures");
    assert!(
        failures.is_empty(),
        "failing (seed, round): {failures:?}; replay one with MUSUBI_CRASH_SEED=<seed> MUSUBI_SYNCD_CRASH_RUNS=1"
    );
    if kills >= 20 {
        let reached = reached.lock().expect("reached");
        for p in POINTS.iter().chain(&["renewal"]) {
            assert!(reached.contains(*p), "no kill at {p}; reached {reached:?}");
        }
    }
}

async fn run(
    seed: u64,
    kills: u64,
    round: Arc<AtomicU64>,
    reached: Arc<Mutex<BTreeSet<&'static str>>>,
) {
    let mut rng = Rng::new(seed);
    let fake = google::Config {
        seed: Seed {
            seed,
            calendars: 1,
            events: 60,
        },
        max_page_size: 25,
        empty_page_every: 3,
        // Renewal comes 60 s after a channel is made (the engine's minimum).
        default_watch_ttl: Duration::from_secs(90),
        max_watch_ttl: Duration::from_secs(90),
        ..google::Config::default()
    };
    let mut w = World::start(fake, &env()).await;
    let follower = Follower::spawn(w.api.clone(), w.read.clone());
    let backups = w.dir.path().join("backups");

    for r in 0..kills {
        round.store(r, Ordering::Relaxed);
        let kill = plan(r, &mut rng);
        if let Kill::Point(name, n) = kill {
            let mut e = env();
            e.push(("MUSUBI_CRASH_AT", format!("{name}:{n}")));
            w.serve(&e);
        }
        let script = w.fake.run_script(load(&w, &mut rng));
        let hit = match kill {
            Kill::Random(ms) => {
                tokio::time::sleep(Duration::from_millis(ms)).await;
                None
            }
            Kill::Point(name, _) => {
                if name == "backup" {
                    let (api, to) = (w.api.clone(), backups.join(r.to_string()));
                    // Ends with the node; its answer does not matter.
                    tokio::spawn(async move {
                        let body = json!({ "to": to });
                        let _ = api
                            .call("POST", "/v1/admin/backup", Some(ADMIN_KEY), Some(body))
                            .await;
                    });
                }
                let node = w.node();
                let mark = format!("{CRASH_MARK}{name}");
                let seen =
                    tokio::task::block_in_place(|| node.wait_line(&mark, Duration::from_secs(20)));
                seen.map(|_| name)
            }
            Kill::Renewal => {
                let watches = || {
                    w.fake
                        .requests()
                        .iter()
                        .filter(|q| q.contains("/events/watch"))
                        .count()
                };
                let before = watches();
                let deadline = Instant::now() + Duration::from_secs(150);
                while watches() == before && Instant::now() < deadline {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                (watches() > before).then_some("renewal")
            }
        };
        w.kill();
        if let Some(p) = hit {
            reached.lock().expect("reached").insert(p);
        }
        script.await.expect("script");

        // Nothing wakes the node but its own startup catch-up.
        w.serve(&env());
        w.converge(Duration::from_secs(90), |_| true).await;
        let journal = settle(&w, &follower).await;
        check_store(&w);
        follower.check(&journal);
    }
}

/// Waits until the follower has read the whole journal and the journal stood
/// still for a moment; returns the journal.
async fn settle(w: &World, f: &Follower) -> Vec<musubi_model::Change> {
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut last = 0;
    loop {
        let j = w.journal();
        let h = head(&j);
        if h == last && f.cursor() == h {
            return j;
        }
        assert!(
            Instant::now() < deadline,
            "follower at {} of {h}\n{}",
            f.cursor(),
            w.node().tail(40)
        );
        last = h;
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

/// 5–34 changes over a second: new events, edits of any live event
/// (exceptions too), deletions of standalone events, rarely expired sync tokens.
fn load(w: &World, rng: &mut Rng) -> Vec<Step> {
    let cals = w.fake.calendars();
    let snaps: Vec<_> = cals.iter().map(|c| w.fake.snapshot(c)).collect();
    let mut steps = Vec::new();
    for _ in 0..5 + rng.below(30) {
        let at_ms = rng.below(1000);
        let i = rng.below(cals.len() as u64) as usize;
        let calendar = cals[i].clone();
        let live: Vec<_> = snaps[i]
            .values()
            .filter(|e| e["status"] != "cancelled")
            .collect();
        let pick =
            |rng: &mut Rng, v: &[&serde_json::Value]| v[rng.below(v.len() as u64) as usize].clone();
        let change = match rng.below(40) {
            0 => Change::ExpireSyncTokens { calendar },
            1..=15 => {
                let start = google::Config::default().epoch
                    + jiff::SignedDuration::from_hours(rng.below(1440) as i64 - 720);
                let end = start + jiff::SignedDuration::from_mins(30);
                let event = json!({
                    "summary": format!("new {}", rng.next_u64()),
                    "start": { "dateTime": start.to_string(), "timeZone": "UTC" },
                    "end": { "dateTime": end.to_string(), "timeZone": "UTC" },
                });
                Change::Put { calendar, event }
            }
            16..=29 if !live.is_empty() => {
                let mut event = pick(rng, &live);
                event["summary"] = json!(format!("edit {}", rng.next_u64()));
                Change::Put { calendar, event }
            }
            _ => {
                let standalone: Vec<_> = live
                    .iter()
                    .copied()
                    .filter(|e| {
                        e.get("recurrence").is_none() && e.get("recurringEventId").is_none()
                    })
                    .collect();
                if standalone.is_empty() {
                    continue;
                }
                let id = pick(rng, &standalone)["id"]
                    .as_str()
                    .expect("id")
                    .to_owned();
                Change::Delete { calendar, id }
            }
        };
        steps.push(Step { at_ms, change });
    }
    steps.sort_by_key(|s| s.at_ms);
    steps
}
