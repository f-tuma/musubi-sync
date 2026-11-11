//! The whole `musubi-syncd` binary over the fake Google server: one tenant
//! with one connected account, the node restartable and killable, and the
//! store files read directly to compare with the fake (crash tests, backup
//! during sync).

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use musubi_model::journal::{ChangeOp, ChangeTarget, ChangesPage};
use musubi_model::{Change, CollectionId, EntityId, PlacementId, Seq};
use musubi_testkit::google::{self, FakeGoogle};
use musubi_testkit::harness::{Api, Daemon, config, free_addr};
use rusqlite::{Connection, OpenFlags};

pub const BIN: &str = env!("CARGO_BIN_EXE_musubi-syncd");

/// `(calendar, event id)` → validator (`etag`).
pub type Items = BTreeMap<(String, String), String>;

pub struct World {
    pub fake: FakeGoogle,
    pub dir: tempfile::TempDir,
    pub addr: SocketAddr,
    pub config: PathBuf,
    pub api: Api,
    pub node: Option<Daemon>,
    pub tenant: String,
    pub admin: String,
    pub read: String,
    /// Fake requests before the last (re)start.
    pub mark: usize,
}

impl World {
    /// Starts the fake and the node, creates a tenant and connects the account.
    pub async fn start(fake: google::Config, env: &[(&str, String)]) -> Self {
        let fake = FakeGoogle::start(fake).await.expect("fake google");
        let dir = tempfile::tempdir().expect("tempdir");
        let addr = free_addr();
        let config = dir.path().join("config.json");
        let c = self::config(addr, &dir.path().join("data"), &fake);
        std::fs::write(&config, c.to_string()).expect("config");
        let mut w = Self {
            fake,
            dir,
            addr,
            config,
            api: Api::new(addr),
            node: None,
            tenant: String::new(),
            admin: String::new(),
            read: String::new(),
            mark: 0,
        };
        w.serve(env);
        w.tenant = w.api.tenant().await;
        w.admin = w.api.key(&w.tenant, "admin").await;
        w.read = w.api.key(&w.tenant, "read").await;
        w.api.connect(&w.admin).await;
        w
    }

    pub fn data(&self) -> PathBuf {
        self.dir.path().join("data")
    }

    /// (Re)starts the node; a running one is killed first.
    pub fn serve(&mut self, env: &[(&str, String)]) {
        self.kill();
        self.mark = self.fake.requests().len();
        let config = self.config.clone();
        self.node = Some(tokio::task::block_in_place(|| {
            Daemon::serve(Path::new(BIN), &config, env)
        }));
    }

    /// SIGKILL.
    pub fn kill(&mut self) {
        if let Some(mut n) = self.node.take() {
            n.kill();
        }
    }

    pub fn node(&self) -> &Daemon {
        self.node.as_ref().expect("node running")
    }

    pub fn main_db(&self) -> Connection {
        open(
            &self
                .data()
                .join("tenants")
                .join(&self.tenant)
                .join("main.db"),
        )
    }

    pub fn control_db(&self) -> Connection {
        open(&self.data().join("control.db"))
    }

    /// What the store must hold: every event of the fake except deleted ones
    /// (a cancelled exception of a series is a live item).
    pub fn expected(&self) -> Items {
        let mut out = Items::new();
        for cal in self.fake.calendars() {
            for (id, e) in self.fake.snapshot(&cal) {
                if e["status"] == "cancelled" && e.get("recurringEventId").is_none() {
                    continue;
                }
                let etag = e["etag"].as_str().expect("etag").to_owned();
                out.insert((cal.clone(), id), etag);
            }
        }
        out
    }

    /// Live items of the store.
    pub fn items(&self) -> Items {
        let db = self.main_db();
        let mut q = db
            .prepare(
                "SELECT c.remote, i.key, i.validator FROM items i
                 JOIN collections c ON c.id = i.collection
                 WHERE i.tombstone = 0 AND c.gone = 0",
            )
            .expect("prepare");
        q.query_map([], |r| Ok(((r.get(0)?, r.get(1)?), r.get(2)?)))
            .expect("items")
            .collect::<Result<_, _>>()
            .expect("items")
    }

    /// Fake requests since the last (re)start, tokens decoded.
    pub fn since_start(&self) -> Vec<String> {
        self.fake.requests()[self.mark..]
            .iter()
            .map(|r| unhex(r))
            .collect()
    }

    /// Collections with their cursors, for failure messages.
    pub fn collections(&self) -> Vec<String> {
        let db = self.main_db();
        let mut q = db
            .prepare("SELECT remote, cursor, active_read, read_complete, coverage FROM collections")
            .expect("prepare");
        q.query_map([], |r| {
            let (remote, cursor, read, complete, cov): (
                String,
                Option<Vec<u8>>,
                Option<i64>,
                i64,
                i64,
            ) = (r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?);
            let cursor = cursor.map(|c| String::from_utf8_lossy(&c).into_owned());
            Ok(format!(
                "{remote} cursor {cursor:?} read {read:?} complete {complete} coverage {cov}"
            ))
        })
        .expect("collections")
        .collect::<Result<_, _>>()
        .expect("collections")
    }

    /// Rows of the job queue, for failure messages.
    pub fn jobs(&self) -> Vec<String> {
        let db = self.control_db();
        let mut q = db
            .prepare(
                "SELECT id, lane, kind, due, dedup, lease_until, attempts FROM jobs ORDER BY id",
            )
            .expect("prepare");
        q.query_map([], |r| {
            let (id, lane, kind, due, dedup, until, attempts): (
                i64,
                String,
                String,
                i64,
                Option<String>,
                Option<i64>,
                i64,
            ) = (
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
            );
            let now = jiff::Timestamp::now().as_microsecond();
            Ok(format!(
                "{id} lane {lane} {kind} due {:+}s {dedup:?} lease {:?} attempts {attempts}",
                (due - now) / 1_000_000,
                until.map(|u| (u - now) / 1_000_000)
            ))
        })
        .expect("jobs")
        .collect::<Result<_, _>>()
        .expect("jobs")
    }

    /// The whole journal, in `seq` order.
    pub fn journal(&self) -> Vec<Change> {
        journal(&self.main_db())
    }

    /// Waits until the store holds what the fake does and `extra` holds.
    pub async fn converge(&self, timeout: Duration, mut extra: impl FnMut(&Self) -> bool) {
        let deadline = Instant::now() + timeout;
        loop {
            let (want, have) = (self.expected(), self.items());
            if want == have && extra(self) {
                return;
            }
            if Instant::now() > deadline {
                let missing: Vec<_> = want
                    .iter()
                    .filter(|(k, v)| have.get(*k) != Some(v))
                    .take(5)
                    .collect();
                let extra: Vec<_> = have
                    .iter()
                    .filter(|(k, v)| want.get(*k) != Some(v))
                    .take(5)
                    .collect();
                panic!(
                    "no convergence in {timeout:?}: {} wanted, {} held; missing or stale {missing:?}; \
                     extra or stale {extra:?}\njobs {:#?}\nfake channels {:?}\nrequests since start {:#?}\ncollections {:#?}\n--- node log ---\n{}",
                    want.len(),
                    have.len(),
                    self.jobs(),
                    self.fake.channels(),
                    self.since_start(),
                    self.collections(),
                    self.node.as_ref().map_or(String::new(), |n| n.tail(60)),
                );
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

/// Decodes the fake's hex page and sync tokens in a request line.
fn unhex(r: &str) -> String {
    let mut out = String::new();
    for part in r.split('&') {
        if !out.is_empty() {
            out.push('&');
        }
        match part.split_once("Token=") {
            Some((k, v)) => {
                let bytes: Option<Vec<u8>> = (0..v.len() / 2)
                    .map(|i| u8::from_str_radix(v.get(2 * i..2 * i + 2)?, 16).ok())
                    .collect();
                let v = bytes.map_or(v.to_owned(), |b| String::from_utf8_lossy(&b).into_owned());
                out.push_str(&format!("{k}Token={v}"));
            }
            None => out.push_str(part),
        }
    }
    out
}

/// A read-write connection used only for reading (a read-only one cannot
/// recover a WAL left by a killed writer).
pub fn open(path: &Path) -> Connection {
    let c = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    c.busy_timeout(Duration::from_secs(10))
        .expect("busy timeout");
    c
}

pub fn journal(db: &Connection) -> Vec<Change> {
    let mut q = db
        .prepare("SELECT seq, kind, target, op FROM changes ORDER BY seq")
        .expect("prepare");
    q.query_map([], |r| {
        let (seq, kind, target, op): (i64, i64, [u8; 16], i64) =
            (r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?);
        Ok(Change {
            seq: Seq(seq as u64),
            target: match kind {
                0 => ChangeTarget::Entity(EntityId::from_bytes(target)),
                1 => ChangeTarget::Placement(PlacementId::from_bytes(target)),
                _ => ChangeTarget::Collection(CollectionId::from_bytes(target)),
            },
            op: match op {
                0 => ChangeOp::Created,
                1 => ChangeOp::Updated,
                _ => ChangeOp::Deleted,
            },
        })
    })
    .expect("journal")
    .collect::<Result<_, _>>()
    .expect("journal")
}

/// Store invariants of plan 6.3 that hold at rest: one item per remote key,
/// a live entity behind every live item and a live item behind every live
/// entity, `entities.seq` the entity's last journal entry, and no job lease
/// expired or longer than the engine's 60 s term.
pub fn check_store(w: &World) {
    let db = w.main_db();
    let count = |sql: &str| -> i64 { db.query_row(sql, [], |r| r.get(0)).expect(sql) };
    let checks = [
        (
            "keys with more than one item",
            "SELECT count(*) FROM (SELECT 1 FROM items GROUP BY collection, key HAVING count(*) > 1)",
        ),
        (
            "live items of a missing or deleted entity",
            "SELECT count(*) FROM items i LEFT JOIN entities e ON e.id = i.entity
             WHERE i.tombstone = 0 AND (e.id IS NULL OR e.tombstone = 1)",
        ),
        (
            "live entities without a live item",
            "SELECT count(*) FROM entities e WHERE e.tombstone = 0
             AND NOT EXISTS (SELECT 1 FROM items i WHERE i.entity = e.id AND i.tombstone = 0)",
        ),
        (
            "entities with more than one live item",
            "SELECT count(*) FROM (SELECT 1 FROM items WHERE tombstone = 0
             GROUP BY entity HAVING count(*) > 1)",
        ),
        (
            "entities whose seq is not their last journal entry",
            "SELECT count(*) FROM entities e WHERE e.seq IS NOT
             (SELECT max(seq) FROM changes c WHERE c.kind = 0 AND c.target = e.id)",
        ),
        (
            "journal entries above the AUTOINCREMENT high-water mark",
            "SELECT count(*) FROM changes WHERE seq >
             (SELECT seq FROM sqlite_sequence WHERE name = 'changes')",
        ),
    ];
    for (what, sql) in checks {
        assert_eq!(count(sql), 0, "{what}");
    }
    let now = jiff::Timestamp::now().as_microsecond();
    let ctl = w.control_db();
    let leases: i64 = ctl
        .query_row(
            "SELECT count(*) FROM jobs WHERE lease IS NOT NULL
             AND (lease_until < ?1 - 1000000 OR lease_until > ?1 + 61000000)",
            [now],
            |r| r.get(0),
        )
        .expect("jobs");
    assert_eq!(leases, 0, "jobs with an expired or overlong lease");
}

/// A client polling `/v1/changes` from 0 through every crash.
pub struct Follower {
    pub seen: Arc<Mutex<Seen>>,
    task: tokio::task::JoinHandle<()>,
}

#[derive(Default)]
pub struct Seen {
    pub cursor: u64,
    pub log: BTreeMap<u64, Change>,
    pub errors: Vec<String>,
}

impl Follower {
    pub fn spawn(api: Api, key: String) -> Self {
        let seen = Arc::new(Mutex::new(Seen::default()));
        let s = seen.clone();
        let task = tokio::spawn(async move {
            loop {
                let since = s.lock().expect("seen").cursor;
                let more = match api.changes(&key, since, 1000).await {
                    Ok((200, v)) => {
                        let page: ChangesPage = serde_json::from_value(v).expect("changes page");
                        let mut s = s.lock().expect("seen");
                        for c in page.changes {
                            if c.seq.0 <= s.cursor {
                                let e = format!("seq {} after cursor {}", c.seq.0, s.cursor);
                                s.errors.push(e);
                            }
                            s.cursor = c.seq.0;
                            s.log.insert(c.seq.0, c);
                        }
                        page.more
                    }
                    Ok((status, v)) if status != 503 => {
                        s.lock()
                            .expect("seen")
                            .errors
                            .push(format!("since {since}: {status} {v}"));
                        false
                    }
                    // The node is down or restarting.
                    _ => false,
                };
                if !more {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }
        });
        Self { seen, task }
    }

    /// Everything the client read is still in the journal, unchanged, and
    /// the client has read up to the journal's end.
    pub fn check(&self, journal: &[Change]) {
        let s = self.seen.lock().expect("seen");
        assert!(s.errors.is_empty(), "follower: {:?}", s.errors);
        let by_seq: BTreeMap<u64, &Change> = journal.iter().map(|c| (c.seq.0, c)).collect();
        for (seq, c) in &s.log {
            assert_eq!(
                by_seq.get(seq),
                Some(&c),
                "the client read seq {seq} as {c:?}"
            );
        }
        assert_eq!(
            s.log.len(),
            journal.len(),
            "the client skipped journal entries"
        );
    }

    pub fn cursor(&self) -> u64 {
        self.seen.lock().expect("seen").cursor
    }
}

impl Drop for Follower {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Head of a journal (0 when empty).
pub fn head(journal: &[Change]) -> u64 {
    journal.last().map_or(0, |c| c.seq.0)
}
