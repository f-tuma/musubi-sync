//! SQLite, one WAL file per tenant, one writer connection per tenant.

use crate::datagen::{MAX_DUR, Page};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS meta(k TEXT PRIMARY KEY, v INTEGER NOT NULL) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS collections(id INTEGER PRIMARY KEY, remote_ref TEXT NOT NULL, cursor TEXT);
CREATE TABLE IF NOT EXISTS raw(hash BLOB PRIMARY KEY, body TEXT NOT NULL) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS items(
  id INTEGER PRIMARY KEY, collection_id INTEGER NOT NULL, remote_key TEXT NOT NULL,
  validator TEXT NOT NULL, raw_ref BLOB NOT NULL, observed TEXT NOT NULL, entity_id INTEGER NOT NULL,
  rev INTEGER NOT NULL, seq INTEGER NOT NULL, tombstone INTEGER NOT NULL DEFAULT 0,
  UNIQUE(collection_id, remote_key));
CREATE TABLE IF NOT EXISTS entities(
  id INTEGER PRIMARY KEY, kind INTEGER NOT NULL, canonical TEXT, rev INTEGER NOT NULL,
  seq INTEGER NOT NULL, tombstone INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS placements(
  id INTEGER PRIMARY KEY, entity_id INTEGER NOT NULL, item_id INTEGER NOT NULL UNIQUE,
  role INTEGER NOT NULL, direction INTEGER NOT NULL DEFAULT 0, projection INTEGER NOT NULL DEFAULT 0,
  origin INTEGER NOT NULL DEFAULT 0, delivered TEXT, local TEXT, delivery INTEGER NOT NULL DEFAULT 0);
CREATE INDEX IF NOT EXISTS placements_entity ON placements(entity_id);
CREATE TABLE IF NOT EXISTS occurrences(
  collection_id INTEGER NOT NULL, start_utc INTEGER NOT NULL, end_utc INTEGER NOT NULL,
  entity_id INTEGER NOT NULL, item_id INTEGER NOT NULL, recurrence_id INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS occ_range ON occurrences(collection_id, start_utc);
CREATE INDEX IF NOT EXISTS occ_item ON occurrences(item_id);
CREATE TABLE IF NOT EXISTS changes(entity_id INTEGER PRIMARY KEY, seq INTEGER NOT NULL UNIQUE, op INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS mutations(
  id INTEGER PRIMARY KEY, entity_id INTEGER NOT NULL, base_rev INTEGER NOT NULL, patch TEXT NOT NULL,
  idem_key TEXT NOT NULL UNIQUE);
CREATE TABLE IF NOT EXISTS outbox(
  id INTEGER PRIMARY KEY, placement_id INTEGER NOT NULL, mutation_id INTEGER NOT NULL,
  state INTEGER NOT NULL, payload TEXT NOT NULL);
INSERT OR IGNORE INTO meta VALUES ('seq', 0);
"#;

pub fn path(dir: &Path, tenant: u64) -> PathBuf {
    dir.join(format!("t{tenant:05}.db"))
}

/// Checkpoint strategy, A/B switch `SQLITE_CKPT`:
/// - `bg` (default): background checkpointer with a WAL size policy (see `Checkpointer`),
///   `wal_autocheckpoint` kept only as a high fuse;
/// - `inline`: SQLite default, `wal_autocheckpoint=1000` in the committing writer, no background thread;
/// - `passive`: the first (flawed) version, PASSIVE every 50 ms and no fuse; the WAL grows without
///   bound under constant readers. Kept only to reproduce that finding.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Ckpt {
    Bg,
    Inline,
    Passive,
}

pub fn ckpt_mode() -> Ckpt {
    match std::env::var("SQLITE_CKPT").as_deref() {
        Ok("inline") => Ckpt::Inline,
        Ok("passive") => Ckpt::Passive,
        Ok("bg") | Err(_) => Ckpt::Bg,
        Ok(x) => panic!("SQLITE_CKPT={x}: expected bg|inline|passive"),
    }
}

/// WAL at or above this size is checkpointed (PASSIVE) on the next sweep; smaller WALs at most once a second.
const WAL_SOFT: u64 = 8 << 20;
/// WAL at or above this size makes the tenant's writer run TRUNCATE after its commit (waits for readers,
/// bounded by `CKPT_WAIT`). The writer does it because it already owns the write lock between its
/// transactions; a separate thread polling for the lock is starved by a busy writer.
/// A/B knob `SQLITE_WAL_HARD_MB` (default 64).
fn wal_hard() -> u64 {
    std::env::var("SQLITE_WAL_HARD_MB").map_or(64, |v| v.parse().expect("SQLITE_WAL_HARD_MB: integer")) << 20
}
/// After a WAL reset the file is cut back to this size (journal_size_limit).
const WAL_LIMIT: u64 = 4 << 20;
/// Upper bound on how long TRUNCATE waits for readers and the writer (the writer is blocked meanwhile).
const CKPT_WAIT: Duration = Duration::from_millis(100);
/// Fuse: inline auto-checkpoint after 65536 pages (256 MB) in case the background checkpointer falls behind.
const FUSE_PAGES: u32 = 65536;

/// Tuning documented in the report: WAL, synchronous=NORMAL for synced data
/// (FULL is switched on per transaction for the outbox), 32 MB page cache, 256 MB mmap.
/// busy_timeout goes first so that switching to WAL already waits on a lock instead of failing.
pub fn open(p: &Path) -> rusqlite::Result<Connection> {
    let c = Connection::open(p)?;
    let autockpt = match ckpt_mode() {
        Ckpt::Bg => FUSE_PAGES,
        Ckpt::Inline => 1000,
        Ckpt::Passive => 0,
    };
    c.execute_batch(&format!(
        "PRAGMA busy_timeout=5000; PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA temp_store=MEMORY;
         PRAGMA cache_size=-32768; PRAGMA mmap_size=268435456; PRAGMA journal_size_limit={WAL_LIMIT};
         PRAGMA wal_autocheckpoint={autockpt};"
    ))?;
    c.set_prepared_statement_cache_capacity(64);
    // Experiment knob for tuning runs; the report states which pragmas the final run used.
    if let Ok(extra) = std::env::var("SQLITE_PRAGMAS") {
        c.execute_batch(&extra)?;
    }
    Ok(c)
}

/// Process-wide, reset by `Checkpointer::start`; one checkpointer per test phase.
static STATS: Stats = Stats {
    max_wal: AtomicU64::new(0),
    passive: AtomicU64::new(0),
    truncate: AtomicU64::new(0),
    truncate_busy: AtomicU64::new(0),
};

struct Stats {
    max_wal: AtomicU64,
    passive: AtomicU64,
    truncate: AtomicU64,
    truncate_busy: AtomicU64,
}

/// Background checkpoints for a set of tenant files, off the writers' commit path.
/// Files are registered with `add` only after the writer has created and initialised them.
/// Every 50 ms each file's WAL size is sampled; >= WAL_SOFT (or >= 1 s since the last one) runs PASSIVE,
/// which copies frames into the DB without blocking anyone. Resetting the WAL (TRUNCATE) is left to the
/// writer, see `Writer::reset_wal`; a WAL over that limit is the writer's and is skipped here (1/s fallback). A thread panic aborts the process (`panic = "abort"`), so a run cannot go on silently
/// without checkpoints.
pub struct Checkpointer {
    stop: Arc<AtomicBool>,
    paths: Arc<Mutex<Vec<PathBuf>>>,
    hs: Vec<JoinHandle<()>>,
}

impl Checkpointer {
    pub fn start(paths: Vec<PathBuf>, threads: usize) -> Self {
        let mode = ckpt_mode();
        let threads = if mode == Ckpt::Inline { 0 } else { threads };
        let stop = Arc::new(AtomicBool::new(false));
        let paths = Arc::new(Mutex::new(paths));
        for a in [&STATS.max_wal, &STATS.passive, &STATS.truncate, &STATS.truncate_busy] {
            a.store(0, Ordering::Relaxed);
        }
        let stats = &STATS;
        let hard = wal_hard();
        let hs = (0..threads)
            .map(|k| {
                let (stop, paths) = (stop.clone(), paths.clone());
                std::thread::spawn(move || {
                    let mut conns: HashMap<usize, (Connection, PathBuf, Instant)> = HashMap::new();
                    while !stop.load(Ordering::Relaxed) {
                        let n = paths.lock().unwrap().len();
                        for i in (k..n).step_by(threads) {
                            let (c, wal, last) = conns.entry(i).or_insert_with(|| {
                                let p = paths.lock().unwrap()[i].clone();
                                let c = open(&p).unwrap();
                                c.busy_timeout(CKPT_WAIT).unwrap();
                                (c, PathBuf::from(format!("{}-wal", p.display())), Instant::now())
                            });
                            let size = std::fs::metadata(&*wal).map(|m| m.len()).unwrap_or(0);
                            stats.max_wal.fetch_max(size, Ordering::Relaxed);
                            if mode == Ckpt::Passive {
                                stats.passive.fetch_add(1, Ordering::Relaxed);
                                c.execute_batch("PRAGMA wal_checkpoint(PASSIVE)").unwrap();
                                continue;
                            }
                            // Over the hard limit the writer resets the WAL itself; a PASSIVE here would hold the
                            // checkpoint lock and make the writer's TRUNCATE fail at once. Once a second as a fallback
                            // for an idle writer.
                            let throttle = size < WAL_SOFT || size >= hard;
                            if size == 0 || (throttle && last.elapsed() < Duration::from_secs(1)) {
                                continue;
                            }
                            *last = Instant::now();
                            stats.passive.fetch_add(1, Ordering::Relaxed);
                            c.execute_batch("PRAGMA wal_checkpoint(PASSIVE)").unwrap();
                        }
                        std::thread::sleep(Duration::from_millis(50));
                    }
                })
            })
            .collect();
        Checkpointer { stop, paths, hs }
    }

    /// Register a tenant file; call only after the file exists with schema and WAL mode set.
    pub fn add(&self, p: PathBuf) {
        self.paths.lock().unwrap().push(p);
    }

    /// One-line summary for result notes.
    pub fn summary(&self) -> String {
        let s = &STATS;
        match ckpt_mode() {
            Ckpt::Inline => "ckpt=inline (wal_autocheckpoint=1000)".to_string(),
            m => format!(
                "ckpt={}: max WAL {:.1} MB, {} passive, {} truncate ({} busy: readers past {} ms or another checkpoint running)",
                if m == Ckpt::Bg { "bg" } else { "passive-50ms" },
                s.max_wal.load(Ordering::Relaxed) as f64 / 1e6,
                s.passive.load(Ordering::Relaxed),
                s.truncate.load(Ordering::Relaxed),
                s.truncate_busy.load(Ordering::Relaxed),
                CKPT_WAIT.as_millis()
            ),
        }
    }
}

impl Drop for Checkpointer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for h in self.hs.drain(..) {
            h.join().expect("checkpointer thread panicked");
        }
    }
}

pub fn create(p: &Path) -> rusqlite::Result<Connection> {
    let c = open(p)?;
    c.execute_batch(SCHEMA)?;
    let mut st = c.prepare("INSERT OR IGNORE INTO collections(id, remote_ref) VALUES (?, ?)")?;
    for coll in 0..crate::datagen::NCOLL {
        st.execute(params![coll, format!("https://calendar.example.com/c/{coll}")])?;
    }
    drop(st);
    Ok(c)
}

/// The single writer of one tenant.
pub struct Writer {
    pub c: Connection,
    seq: i64,
    wal: Option<(PathBuf, u64)>,
}

impl Writer {
    pub fn new(c: Connection) -> rusqlite::Result<Self> {
        let seq = c.query_row("SELECT v FROM meta WHERE k='seq'", [], |r| r.get(0))?;
        let wal = (ckpt_mode() == Ckpt::Bg).then(|| (PathBuf::from(format!("{}-wal", c.path().unwrap())), wal_hard()));
        Ok(Writer { c, seq, wal })
    }

    /// After a commit: if the WAL reached the hard limit (`wal_hard`), TRUNCATE it. Waits at most CKPT_WAIT for readers that
    /// still use old frames (this tenant's writes wait meanwhile); on timeout the next commit tries again.
    fn reset_wal(&mut self) -> rusqlite::Result<()> {
        let Some((wal, hard)) = &self.wal else { return Ok(()) };
        if std::fs::metadata(wal).map(|m| m.len()).unwrap_or(0) < *hard {
            return Ok(());
        }
        self.c.busy_timeout(CKPT_WAIT)?;
        let busy: i64 = self.c.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| r.get(0))?;
        self.c.busy_timeout(Duration::from_secs(5))?;
        STATS.truncate.fetch_add(1, Ordering::Relaxed);
        STATS.truncate_busy.fetch_add(busy as u64, Ordering::Relaxed);
        Ok(())
    }

    /// One transaction per page: items + raw + entities + placements + occurrences + journal + cursor.
    /// Returns the number of changed items.
    pub fn apply_page(&mut self, page: &Page) -> rusqlite::Result<usize> {
        // IMMEDIATE: a deferred txn that upgrades read->write gets SQLITE_BUSY without the busy handler
        // while the checkpointer's TRUNCATE holds the write lock; IMMEDIATE waits for it instead.
        let tx = self.c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut changed = 0;
        {
            let mut find = tx.prepare_cached("SELECT id, entity_id, validator FROM items WHERE collection_id=? AND remote_key=?")?;
            let mut find_link = tx.prepare_cached("SELECT entity_id FROM items WHERE collection_id=0 AND remote_key=?")?;
            let mut raw = tx.prepare_cached("INSERT OR IGNORE INTO raw(hash, body) VALUES (?, ?)")?;
            let mut ins_ent = tx.prepare_cached("INSERT INTO entities(kind, canonical, rev, seq) VALUES (0, NULL, 1, ?)")?;
            let mut ins_item = tx.prepare_cached(
                "INSERT INTO items(collection_id, remote_key, validator, raw_ref, observed, entity_id, rev, seq) VALUES (?,?,?,?,?,?,1,?)",
            )?;
            let mut upd_item = tx.prepare_cached("UPDATE items SET validator=?, raw_ref=?, observed=?, rev=rev+1, seq=? WHERE id=?")?;
            let mut upd_ent = tx.prepare_cached("UPDATE entities SET canonical=coalesce(?, canonical), rev=rev+1, seq=? WHERE id=?")?;
            let mut ins_pl = tx.prepare_cached("INSERT INTO placements(entity_id, item_id, role) VALUES (?,?,?)")?;
            let mut del_occ = tx.prepare_cached("DELETE FROM occurrences WHERE item_id=?")?;
            let mut ins_occ = tx.prepare_cached(
                "INSERT INTO occurrences(collection_id, start_utc, end_utc, entity_id, item_id, recurrence_id) VALUES (?,?,?,?,?,?)",
            )?;
            let mut journal = tx.prepare_cached(
                "INSERT INTO changes(entity_id, seq, op) VALUES (?,?,?) ON CONFLICT(entity_id) DO UPDATE SET seq=excluded.seq, op=excluded.op",
            )?;
            for it in &page.items {
                let existing: Option<(i64, i64, String)> =
                    find.query_row(params![page.coll, it.key], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()?;
                if matches!(&existing, Some((_, _, v)) if *v == it.validator) {
                    continue; // unchanged validator is a no-op (proposal 3.3)
                }
                self.seq += 1;
                let seq = self.seq;
                raw.execute(params![it.raw_hash, it.raw])?;
                let (item_id, entity_id, op) = match existing {
                    Some((id, ent, _)) => {
                        upd_item.execute(params![it.validator, it.raw_hash, it.canonical, seq, id])?;
                        upd_ent.execute(params![it.link.as_ref().map(|_| &it.canonical), seq, ent])?;
                        del_occ.execute([id])?;
                        (id, ent, 2)
                    }
                    None => {
                        let link: Option<i64> = match &it.link {
                            Some(k) => find_link.query_row([k], |r| r.get(0)).optional()?,
                            None => None,
                        };
                        let ent = match link {
                            Some(ent) => {
                                // Second placement materializes the entity (chapter 12.2).
                                upd_ent.execute(params![it.canonical, seq, ent])?;
                                ent
                            }
                            None => {
                                ins_ent.execute([seq])?;
                                tx.last_insert_rowid()
                            }
                        };
                        ins_item.execute(params![page.coll, it.key, it.validator, it.raw_hash, it.canonical, ent, seq])?;
                        let id = tx.last_insert_rowid();
                        ins_pl.execute(params![ent, id, link.is_some() as i64])?;
                        (id, ent, 1)
                    }
                };
                for o in &it.occ {
                    ins_occ.execute(params![page.coll, o.start, o.end, entity_id, item_id, o.rid])?;
                }
                journal.execute(params![entity_id, seq, op])?;
                changed += 1;
            }
            tx.prepare_cached("UPDATE collections SET cursor=? WHERE id=?")?.execute(params![page.cursor, page.coll])?;
            tx.prepare_cached("UPDATE meta SET v=? WHERE k='seq'")?.execute([self.seq])?;
        }
        tx.commit()?;
        self.reset_wal()?;
        Ok(changed)
    }

    /// Accepting an application mutation: mutation + entity rev + journal + outbox rows, fsync'd.
    pub fn accept_mutation(&mut self, entity_id: i64, idem: &str, patch: &str) -> rusqlite::Result<()> {
        self.c.execute_batch("PRAGMA synchronous=FULL")?;
        let res = (|| {
            let tx = self.c.transaction_with_behavior(TransactionBehavior::Immediate)?;
            self.seq += 1;
            let seq = self.seq;
            let base: i64 = tx.prepare_cached("SELECT rev FROM entities WHERE id=?")?.query_row([entity_id], |r| r.get(0))?;
            tx.prepare_cached("INSERT INTO mutations(entity_id, base_rev, patch, idem_key) VALUES (?,?,?,?)")?
                .execute(params![entity_id, base, patch, idem])?;
            let mid = tx.last_insert_rowid();
            tx.prepare_cached("UPDATE entities SET rev=rev+1, seq=? WHERE id=?")?.execute(params![seq, entity_id])?;
            tx.prepare_cached(
                "INSERT INTO changes(entity_id, seq, op) VALUES (?,?,2) ON CONFLICT(entity_id) DO UPDATE SET seq=excluded.seq, op=2",
            )?
            .execute(params![entity_id, seq])?;
            let mut pls = tx.prepare_cached("SELECT id FROM placements WHERE entity_id=?")?;
            let ids: Vec<i64> = pls.query_map([entity_id], |r| r.get(0))?.collect::<Result<_, _>>()?;
            drop(pls);
            for pid in ids {
                tx.prepare_cached("INSERT INTO outbox(placement_id, mutation_id, state, payload) VALUES (?,?,0,?)")?
                    .execute(params![pid, mid, patch])?;
            }
            tx.prepare_cached("UPDATE meta SET v=? WHERE k='seq'")?.execute([seq])?;
            tx.commit()
        })();
        self.c.execute_batch("PRAGMA synchronous=NORMAL")?;
        res?;
        self.reset_wal()
    }
}

/// Range query across collections; dedups an entity placed in several of them.
pub fn range(c: &Connection, colls: &[i64], from: i64, to: i64, payload: bool) -> rusqlite::Result<usize> {
    let list = colls.iter().map(|c| c.to_string()).collect::<Vec<_>>().join(",");
    let sql = if payload {
        format!(
            "SELECT o.entity_id, o.recurrence_id, o.start_utc, o.end_utc, coalesce(e.canonical, i.observed)
             FROM occurrences o JOIN entities e ON e.id=o.entity_id JOIN items i ON i.id=o.item_id
             WHERE o.collection_id IN ({list}) AND o.start_utc >= ?1 - {MAX_DUR} AND o.start_utc < ?2 AND o.end_utc > ?1
             GROUP BY o.entity_id, o.recurrence_id"
        )
    } else {
        format!(
            "SELECT o.entity_id, o.recurrence_id, o.start_utc, o.end_utc FROM occurrences o
             WHERE o.collection_id IN ({list}) AND o.start_utc >= ?1 - {MAX_DUR} AND o.start_utc < ?2 AND o.end_utc > ?1
             GROUP BY o.entity_id, o.recurrence_id"
        )
    };
    let mut st = c.prepare_cached(&sql)?;
    let mut rows = st.query(params![from, to])?;
    let mut n = 0;
    while let Some(r) = rows.next()? {
        if payload {
            let s: &str = r.get_ref(4)?.as_str()?;
            n += (s.len() > 0) as usize;
        } else {
            let _: i64 = r.get(0)?;
            n += 1;
        }
    }
    Ok(n)
}

pub fn changes(c: &Connection, since: i64, limit: i64) -> rusqlite::Result<usize> {
    let mut st = c.prepare_cached(
        "SELECT c.seq, c.entity_id, c.op, coalesce(e.canonical, i.observed)
         FROM changes c JOIN entities e ON e.id=c.entity_id
         JOIN placements p ON p.entity_id=c.entity_id AND p.role=0 JOIN items i ON i.id=p.item_id
         WHERE c.seq > ? ORDER BY c.seq LIMIT ?",
    )?;
    let mut rows = st.query(params![since, limit])?;
    let mut n = 0;
    while let Some(r) = rows.next()? {
        let s: &str = r.get_ref(3)?.as_str()?;
        n += (s.len() > 0) as usize;
    }
    Ok(n)
}

pub fn head_seq(c: &Connection) -> rusqlite::Result<i64> {
    c.query_row("SELECT v FROM meta WHERE k='seq'", [], |r| r.get(0))
}
