//! SQLite, one WAL file per tenant, one writer connection per tenant.

use crate::datagen::{MAX_DUR, Page};
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

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

/// Tuning documented in the report: WAL, synchronous=NORMAL for synced data
/// (FULL is switched on per transaction for the outbox), 32 MB page cache, 256 MB mmap.
/// Auto-checkpoint is off: an inline checkpoint costs the committing writer 25-40 ms here,
/// so a background `Checkpointer` does PASSIVE checkpoints instead.
pub fn open(p: &Path) -> rusqlite::Result<Connection> {
    let c = Connection::open(p)?;
    c.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA temp_store=MEMORY;
         PRAGMA cache_size=-32768; PRAGMA mmap_size=268435456; PRAGMA busy_timeout=5000;
         PRAGMA wal_autocheckpoint=0;",
    )?;
    c.set_prepared_statement_cache_capacity(64);
    // Experiment knob for tuning runs; the report states which pragmas the final run used.
    if let Ok(extra) = std::env::var("SQLITE_PRAGMAS") {
        c.execute_batch(&extra)?;
    }
    Ok(c)
}

/// Background PASSIVE checkpoints for a set of tenant files, off the writers' commit path.
pub struct Checkpointer {
    stop: Arc<AtomicBool>,
    hs: Vec<JoinHandle<()>>,
}

impl Checkpointer {
    pub fn start(paths: Vec<PathBuf>, threads: usize) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let paths = Arc::new(paths);
        let hs = (0..threads)
            .map(|k| {
                let (stop, paths) = (stop.clone(), paths.clone());
                std::thread::spawn(move || {
                    let mut conns: HashMap<usize, Connection> = HashMap::new();
                    while !stop.load(Ordering::Relaxed) {
                        for i in (k..paths.len()).step_by(threads) {
                            // Never create a file here; the writer owns creation.
                            if !conns.contains_key(&i) && !paths[i].exists() {
                                continue;
                            }
                            let c = conns.entry(i).or_insert_with(|| open(&paths[i]).unwrap());
                            let _ = c.execute_batch("PRAGMA wal_checkpoint(PASSIVE)");
                        }
                        std::thread::sleep(Duration::from_millis(50));
                    }
                })
            })
            .collect();
        Checkpointer { stop, hs }
    }
}

impl Drop for Checkpointer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for h in self.hs.drain(..) {
            let _ = h.join();
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
}

impl Writer {
    pub fn new(c: Connection) -> rusqlite::Result<Self> {
        let seq = c.query_row("SELECT v FROM meta WHERE k='seq'", [], |r| r.get(0))?;
        Ok(Writer { c, seq })
    }

    /// One transaction per page: items + raw + entities + placements + occurrences + journal + cursor.
    /// Returns the number of changed items.
    pub fn apply_page(&mut self, page: &Page) -> rusqlite::Result<usize> {
        let tx = self.c.transaction()?;
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
        Ok(changed)
    }

    /// Accepting an application mutation: mutation + entity rev + journal + outbox rows, fsync'd.
    pub fn accept_mutation(&mut self, entity_id: i64, idem: &str, patch: &str) -> rusqlite::Result<()> {
        self.c.execute_batch("PRAGMA synchronous=FULL")?;
        let res = (|| {
            let tx = self.c.transaction()?;
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
        res
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
