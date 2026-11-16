//! Plan K9: `kill -9` of the whole `musubi-syncd` binary while it reads a
//! DAV account from the slowed fake DAV server, then a restart. Each round
//! changes the fake while the node is down (new, edited and deleted events
//! and contacts, now and then expired sync tokens), starts the node, kills it
//! once the `n`-th DAV request is in flight and checks:
//!
//! - at the kill: a collection whose stored cursor is the fake's current sync
//!   token holds exactly the fake's objects (the cursor never runs ahead of
//!   the pages committed before it);
//! - after the restart: the account resumes and the store converges to the
//!   fake with nothing lost or duplicated (one item per object, the store
//!   invariants of plan 6.3, a client following `/v1/changes` saw every
//!   journal entry once).
//!
//! A run is 20 rounds over its own fake and node (the fake grows with every
//! round). `MUSUBI_DAV_CRASH_RUNS` (1) runs, `MUSUBI_DAV_CRASH_JOBS` (4) at a
//! time, `MUSUBI_CRASH_SEED` the seed of run 0 (run `i` uses seed + i); a
//! failure names its seed and round, and the seed replays its changes (the
//! kill timing itself is not reproducible).
//!
//! Run: `cargo test -p musubi-syncd --release --test crash_dav -- --nocapture`.

mod common;

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant};

use common::whole::{BIN, Follower, check_dbs, head, journal, open};
use musubi_testkit::dav::{Config as DavConfig, FakeDav, Kind};
use musubi_testkit::google::FakeGoogle;
use musubi_testkit::harness::{Api, Daemon, Rng, config, free_addr};
use serde_json::{Value, json};

/// Per request: a 20-object page waits for its `multiget`, so a read of a
/// few hundred objects takes a second and commits page by page.
const LATENCY: Duration = Duration::from_millis(15);

/// Kills per run.
const ROUNDS: u64 = 20;

/// `collection path` → `object name` → served ETag.
type Objects = BTreeMap<String, BTreeMap<String, String>>;

struct World {
    dav: FakeDav,
    _google: FakeGoogle,
    dir: tempfile::TempDir,
    config: std::path::PathBuf,
    api: Api,
    node: Option<Daemon>,
    started: i64,
    tenant: String,
    read: String,
    cal: String,
    book: String,
    /// Names of the live objects of each collection.
    live: BTreeMap<String, Vec<String>>,
    next: u64,
}

impl World {
    fn serve(&mut self) {
        self.kill();
        self.started = jiff::Timestamp::now().as_microsecond();
        let config = self.config.clone();
        self.node = Some(tokio::task::block_in_place(|| {
            Daemon::serve(Path::new(BIN), &config, &[])
        }));
    }

    fn kill(&mut self) {
        if let Some(mut n) = self.node.take() {
            n.kill();
        }
    }

    fn tail(&self) -> String {
        self.node.as_ref().map_or(String::new(), |n| n.tail(40))
    }

    fn main_db(&self) -> rusqlite::Connection {
        open(
            &self
                .dir
                .path()
                .join("data/tenants")
                .join(&self.tenant)
                .join("main.db"),
        )
    }

    /// What the fake serves.
    fn expected(&self) -> Objects {
        self.live
            .iter()
            .map(|(col, names)| {
                let objs = names
                    .iter()
                    .map(|n| {
                        let etag = self.dav.etag(&format!("{col}{n}")).expect("etag");
                        (n.clone(), etag.trim_matches('"').to_owned())
                    })
                    .collect();
                (col.clone(), objs)
            })
            .collect()
    }

    /// Live items of the store by collection path, and each collection's
    /// stored cursor.
    fn store(&self) -> (Objects, BTreeMap<String, Option<Value>>) {
        let db = self.main_db();
        let mut cursors = BTreeMap::new();
        let mut q = db
            .prepare("SELECT remote, cursor FROM collections WHERE gone = 0")
            .expect("prepare");
        let rows = q
            .query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, Option<Vec<u8>>>(1)?))
            })
            .expect("collections");
        for row in rows {
            let (remote, cursor) = row.expect("collection");
            let cursor = cursor.map(|c| serde_json::from_slice(&c).expect("cursor json"));
            cursors.insert(path_of(&remote), cursor);
        }
        let mut objs = Objects::new();
        let mut q = db
            .prepare(
                "SELECT c.remote, i.key, i.validator FROM items i
                 JOIN collections c ON c.id = i.collection
                 WHERE i.tombstone = 0 AND c.gone = 0",
            )
            .expect("prepare");
        let rows = q
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })
            .expect("items");
        for row in rows {
            let (remote, key, validator) = row.expect("item");
            let col = path_of(&remote);
            let name = key.strip_prefix(&col).unwrap_or(&key).to_owned();
            let etag = validator.trim_matches('"').to_owned();
            objs.entry(col).or_default().insert(name, etag);
        }
        (objs, cursors)
    }

    /// New, edited and deleted objects while the node is down.
    fn change(&mut self, rng: &mut Rng, round: u64) {
        let n = if round == 0 { 300 } else { 10 + rng.below(80) };
        for _ in 0..n {
            let (col, ics) = if rng.below(4) == 0 {
                (self.book.clone(), false)
            } else {
                (self.cal.clone(), true)
            };
            let live = self.live.entry(col.clone()).or_default();
            match rng.below(10) {
                0..=5 => {
                    self.next += 1;
                    let uid = format!("crash-{}", self.next);
                    let name = format!("{uid}.{}", if ics { "ics" } else { "vcf" });
                    self.dav.put(&col, &name, &body(ics, &uid, rng.next_u64()));
                    live.push(name);
                }
                6..=8 if !live.is_empty() => {
                    let name = live[rng.below(live.len() as u64) as usize].clone();
                    let uid = name.split('.').next().expect("uid").to_owned();
                    self.dav.put(&col, &name, &body(ics, &uid, rng.next_u64()));
                }
                _ if !live.is_empty() => {
                    let name = live.swap_remove(rng.below(live.len() as u64) as usize);
                    self.dav.delete(&format!("{col}{name}"));
                }
                _ => {}
            }
        }
        if round % 5 == 4 {
            self.dav.expire_sync_tokens(&self.cal.clone());
        }
    }
}

/// The collection path of a stored collection's remote id (a URL or a path).
fn path_of(remote: &str) -> String {
    match remote.find("://") {
        Some(i) => remote[i + 3..]
            .find('/')
            .map_or("/".into(), |j| remote[i + 3 + j..].to_owned()),
        None => remote.to_owned(),
    }
}

fn body(ics: bool, uid: &str, v: u64) -> String {
    if ics {
        format!(
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//musubi//crash//EN\r\nBEGIN:VEVENT\r\n\
             UID:{uid}\r\nDTSTAMP:20260101T000000Z\r\nDTSTART:20261116T{:02}0000Z\r\n\
             DTEND:20261116T{:02}3000Z\r\nSUMMARY:event {v}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
            v % 24,
            v % 24
        )
    } else {
        format!("BEGIN:VCARD\r\nVERSION:4.0\r\nUID:{uid}\r\nFN:Person {v}\r\nEND:VCARD\r\n")
    }
}

fn var(name: &str, default: u64) -> u64 {
    std::env::var(name).map_or(default, |v| v.parse().expect(name))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn syncd_survives_sigkill_during_a_dav_read() {
    let runs = var("MUSUBI_DAV_CRASH_RUNS", 1);
    let jobs = var("MUSUBI_DAV_CRASH_JOBS", 4).min(runs);
    let base = var(
        "MUSUBI_CRASH_SEED",
        jiff::Timestamp::now().as_nanosecond() as u64,
    );
    println!("crash_dav: seed {base}, {runs} runs of {ROUNDS} rounds, {jobs} at a time");
    let mut set = tokio::task::JoinSet::new();
    let (mut mid_read, mut between) = (0, 0);
    for i in 0..runs {
        if set.len() as u64 >= jobs {
            let (m, b) = set.join_next().await.expect("run").expect("run");
            (mid_read, between) = (mid_read + m, between + b);
        }
        set.spawn(run(base.wrapping_add(i)));
    }
    while let Some(r) = set.join_next().await {
        let (m, b) = r.expect("run");
        (mid_read, between) = (mid_read + m, between + b);
    }
    let rounds = runs * ROUNDS;
    println!(
        "crash_dav: {rounds} rounds, {mid_read} kills with a request in flight, \
         {between} collections caught between pages"
    );
    assert!(
        mid_read * 2 >= rounds,
        "only {mid_read} of {rounds} kills hit a read"
    );
}

/// One run of [`ROUNDS`] kills over its own fake and node: kills with a
/// request in flight, collections caught between pages.
async fn run(seed: u64) -> (u64, u64) {
    let mut rng = Rng::new(seed);
    let dav = FakeDav::start(DavConfig {
        latency: LATENCY,
        ..DavConfig::default()
    })
    .await
    .expect("fake dav");
    let cal = dav.add_collection(Kind::Calendar(&["VEVENT"]), "home", "Home");
    let book = dav.add_collection(Kind::AddressBook, "contacts", "Contacts");
    let google = FakeGoogle::start(Default::default())
        .await
        .expect("fake google");
    let dir = tempfile::tempdir().expect("tempdir");
    let addr = free_addr();
    let mut c = config(addr, &dir.path().join("data"), &google);
    c["dav"] = json!({ "allow_private": true });
    let fast = json!({ "min": 0.3, "max": 1 });
    c["sync"] = json!({ "active": fast, "normal": fast, "sleeping": fast, "page_size": 20 });
    let config = dir.path().join("config.json");
    std::fs::write(&config, c.to_string()).expect("config");
    let mut w = World {
        dav,
        _google: google,
        dir,
        config,
        api: Api::new(addr),
        node: None,
        started: 0,
        tenant: String::new(),
        read: String::new(),
        cal,
        book,
        live: BTreeMap::new(),
        next: 0,
    };
    w.serve();
    w.tenant = w.api.tenant().await;
    let admin = w.api.key(&w.tenant, "admin").await;
    w.read = w.api.key(&w.tenant, "read").await;
    let follower = Follower::spawn(w.api.clone(), w.read.clone());

    let (mut mid_read, mut between) = (0, 0);
    let mut account = String::new();
    for round in 0..ROUNDS {
        w.change(&mut rng, round);
        let expected = w.expected();
        let mark = w.dav.requests().len();
        if round == 0 {
            let body = json!({
                "profile": "generic", "server_url": w.dav.server_url(),
                "username": "alice", "password": "secret",
            });
            let (s, v) = w
                .api
                .call("POST", "/v1/accounts/dav", Some(&admin), Some(body))
                .await
                .expect("connect");
            assert_eq!(s, 200, "{v}");
            account = v["id"].as_str().expect("id").to_owned();
        } else {
            w.serve();
        }
        // Kill once the n-th request of this run is in flight.
        let n = 1 + rng.below(25) as usize;
        let deadline = Instant::now() + Duration::from_secs(10);
        while w.dav.requests().len() < mark + n && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        tokio::time::sleep(Duration::from_millis(rng.below(LATENCY.as_millis() as u64))).await;
        w.kill();
        if w.dav.requests().len() >= mark + n {
            mid_read += 1;
        }

        let (held, cursors) = w.store();
        for (col, cursor) in &cursors {
            let token = cursor
                .as_ref()
                .and_then(|c| c["sync_token"]["token"].as_str());
            if token == Some(w.dav.sync_token(col).as_str()) {
                assert_eq!(
                    held.get(col).cloned().unwrap_or_default(),
                    expected.get(col).cloned().unwrap_or_default(),
                    "round {round} (seed {seed}): {col} has the current sync token but not its objects"
                );
            } else if held.get(col) != expected.get(col) {
                between += 1;
            }
        }

        w.serve();
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let (held, cursors) = w.store();
            let at_token = cursors.iter().all(|(col, c)| {
                c.as_ref().and_then(|c| c["sync_token"]["token"].as_str())
                    == Some(w.dav.sync_token(col).as_str())
            });
            if held == expected && at_token && cursors.len() == 2 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "round {round} (seed {seed}): no convergence; cursors {cursors:?}\n{}",
                w.tail()
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let (s, v) = w
            .api
            .call(
                "GET",
                &format!("/v1/accounts/{account}"),
                Some(&w.read),
                None,
            )
            .await
            .expect("accounts");
        assert_eq!(s, 200, "{v}");
        assert_eq!(v["state"], "active", "round {round}: {v}");
        check_dbs(
            &w.main_db(),
            &open(&w.dir.path().join("data/control.db")),
            w.started,
        );
        let j = journal(&w.main_db());
        let deadline = Instant::now() + Duration::from_secs(30);
        while !follower.at(head(&j)) {
            assert!(
                Instant::now() < deadline,
                "follower at {} of {}",
                follower.cursor(),
                head(&j)
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        follower.check(&j);
    }
    (mid_read, between)
}
