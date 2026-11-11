//! Schemas of `main.db` and `control.db` and the lazy migration of `main.db`.
//!
//! Ids are 16-byte big-endian blobs, timestamps microseconds since the epoch.
//! The schema version lives in `meta` (`k = 'schema'`); a file without `meta`
//! is version 0 (a tenant whose creation was interrupted).

use musubi_model::error::StoreError;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior};

use crate::Result;

/// `MAIN_MIGRATIONS[i]` takes a tenant file from version `i` to `i + 1`.
pub(crate) const MAIN_MIGRATIONS: &[&str] = &[MAIN_V1];

/// Schema version this binary writes.
pub(crate) const MAIN_VERSION: u32 = MAIN_MIGRATIONS.len() as u32;

/// Version 1. `meta` holds `schema`, `oldest` (oldest `since` that `changes`
/// accepts after a prune) and `max_span` (longest occurrence in the index, which
/// bounds the range scan); `projected` is reserved for phase 3.
/// `items.seen` is the full read that last saw the item (invariant 4).
/// `occurrences.rid` is `NO_RID` for a single event, so that row values compare.
const MAIN_V1: &str = r#"
CREATE TABLE meta(k TEXT PRIMARY KEY, v INTEGER NOT NULL);
INSERT INTO meta VALUES ('oldest', 0), ('max_span', 0);
CREATE TABLE collections(
  id BLOB PRIMARY KEY, account BLOB NOT NULL, remote TEXT NOT NULL, meta TEXT NOT NULL,
  cursor BLOB, active_read INTEGER, read_complete INTEGER NOT NULL DEFAULT 0, last_read INTEGER,
  coverage INTEGER NOT NULL DEFAULT 0, cov_start INTEGER, cov_end INTEGER,
  err_message TEXT, err_at INTEGER, err_permanent INTEGER,
  synced_at INTEGER, full_read_at INTEGER, gone INTEGER NOT NULL DEFAULT 0,
  UNIQUE(account, remote));
CREATE TABLE raw(hash BLOB PRIMARY KEY, media_type TEXT NOT NULL, body BLOB NOT NULL);
CREATE TABLE entities(
  id BLOB PRIMARY KEY, kind TEXT NOT NULL, rev INTEGER NOT NULL, seq INTEGER NOT NULL,
  tombstone INTEGER NOT NULL DEFAULT 0, deleted_at INTEGER,
  series_first INTEGER, series_last INTEGER, indexed_until INTEGER);
CREATE INDEX entities_horizon ON entities(indexed_until)
  WHERE indexed_until IS NOT NULL AND tombstone = 0;
CREATE TABLE items(
  id BLOB PRIMARY KEY, collection BLOB NOT NULL, key TEXT NOT NULL, entity BLOB NOT NULL,
  kind TEXT NOT NULL, validator TEXT NOT NULL, raw_hash BLOB NOT NULL,
  schema_version INTEGER NOT NULL, mapping_version INTEGER NOT NULL, observed TEXT,
  uid TEXT, recurrence_id INTEGER, tombstone INTEGER NOT NULL DEFAULT 0, deleted_at INTEGER,
  seen INTEGER,
  UNIQUE(collection, key));
CREATE INDEX items_entity ON items(entity);
CREATE INDEX items_raw ON items(raw_hash);
CREATE INDEX items_uid ON items(collection, uid, recurrence_id) WHERE uid IS NOT NULL;
CREATE TABLE placements(
  id BLOB PRIMARY KEY, entity BLOB NOT NULL, item BLOB NOT NULL UNIQUE, collection BLOB NOT NULL,
  role TEXT NOT NULL, direction TEXT NOT NULL, projection TEXT NOT NULL, origin TEXT NOT NULL,
  delivery TEXT NOT NULL, tombstone INTEGER NOT NULL DEFAULT 0, deleted_at INTEGER);
CREATE INDEX placements_entity ON placements(entity);
CREATE TABLE occurrences(
  start INTEGER NOT NULL, end INTEGER NOT NULL, entity BLOB NOT NULL, rid INTEGER NOT NULL,
  floating INTEGER NOT NULL, collection BLOB NOT NULL, item BLOB NOT NULL);
CREATE INDEX occurrences_time ON occurrences(start, entity, rid);
CREATE INDEX occurrences_entity ON occurrences(entity);
CREATE TABLE changes(
  seq INTEGER PRIMARY KEY AUTOINCREMENT, kind INTEGER NOT NULL, target BLOB NOT NULL,
  op INTEGER NOT NULL, at INTEGER NOT NULL);
"#;

/// Schema version stored in a tenant file; 0 if it has none yet.
pub(crate) fn main_version(c: &Connection) -> Result<u32> {
    let has_meta: bool = c.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'meta')",
        [],
        |r| r.get(0),
    )?;
    if !has_meta {
        return Ok(0);
    }
    let v: Option<u32> = c
        .query_row("SELECT v FROM meta WHERE k = 'schema'", [], |r| r.get(0))
        .optional()?;
    Ok(v.unwrap_or(0))
}

/// Brings a tenant file up to the last version of `steps`, in one transaction.
/// A file from a newer binary is `SchemaTooNew` and left untouched.
pub(crate) fn migrate(c: &mut Connection, steps: &[&str]) -> Result<()> {
    let supported = steps.len() as u32;
    let found = main_version(c)?;
    if found > supported {
        return Err(StoreError::SchemaTooNew { found, supported }.into());
    }
    if found == supported {
        return Ok(());
    }
    let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
    // Re-read under the write lock: another process may have migrated meanwhile.
    let found = main_version(&tx)?;
    for step in &steps[found as usize..] {
        tx.execute_batch(step)?;
    }
    tx.execute(
        "INSERT INTO meta VALUES ('schema', ?1) ON CONFLICT(k) DO UPDATE SET v = ?1",
        [supported],
    )?;
    tx.commit()?;
    Ok(())
}

/// `control.db`. `jobs.account` is derived from the job kind (`Discover`,
/// `Sync`: its account; `RenewChannel`: the channel's account at enqueue time;
/// `ExtendIndex`, `Maintain`: none, they belong to the tenant) so that
/// `delete_account` finds the account's jobs through an index.
pub(crate) const CONTROL: &str = r#"
CREATE TABLE IF NOT EXISTS meta(k TEXT PRIMARY KEY, v INTEGER NOT NULL);
INSERT OR IGNORE INTO meta VALUES ('lease', 0);
CREATE TABLE IF NOT EXISTS tenants(id BLOB PRIMARY KEY);
CREATE TABLE IF NOT EXISTS api_keys(
  hash BLOB PRIMARY KEY, tenant BLOB NOT NULL, scope TEXT NOT NULL,
  created INTEGER NOT NULL, revoked INTEGER);
CREATE INDEX IF NOT EXISTS api_keys_tenant ON api_keys(tenant);
CREATE TABLE IF NOT EXISTS accounts(
  id BLOB PRIMARY KEY, tenant BLOB NOT NULL, provider TEXT NOT NULL, subject TEXT NOT NULL,
  state TEXT NOT NULL, tokens BLOB,
  UNIQUE(tenant, provider, subject));
CREATE TABLE IF NOT EXISTS oauth_states(
  hash BLOB PRIMARY KEY, tenant BLOB NOT NULL, provider TEXT NOT NULL, account BLOB,
  verifier TEXT NOT NULL, expires INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS oauth_states_expires ON oauth_states(expires);
CREATE TABLE IF NOT EXISTS jobs(
  id INTEGER PRIMARY KEY AUTOINCREMENT, tenant BLOB NOT NULL, account BLOB,
  lane TEXT NOT NULL, kind TEXT NOT NULL, priority INTEGER NOT NULL, due INTEGER NOT NULL,
  dedup TEXT, lease INTEGER, lease_until INTEGER, attempts INTEGER NOT NULL DEFAULT 0);
CREATE INDEX IF NOT EXISTS jobs_due ON jobs(lane, priority DESC, due);
CREATE INDEX IF NOT EXISTS jobs_tenant ON jobs(tenant);
CREATE INDEX IF NOT EXISTS jobs_account ON jobs(account) WHERE account IS NOT NULL;
CREATE INDEX IF NOT EXISTS jobs_dedup ON jobs(tenant, dedup) WHERE dedup IS NOT NULL AND lease IS NULL;
CREATE TABLE IF NOT EXISTS channels(
  id TEXT PRIMARY KEY, tenant BLOB NOT NULL, account BLOB NOT NULL, collection BLOB NOT NULL,
  remote TEXT NOT NULL, resource TEXT NOT NULL, token TEXT NOT NULL, expiration INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS channels_tenant ON channels(tenant, account);
CREATE INDEX IF NOT EXISTS channels_expiration ON channels(expiration);
"#;

#[cfg(test)]
mod tests {
    use super::*;

    /// A file written by an older binary is migrated in place on open and keeps
    /// its data; a step added later runs exactly once.
    #[test]
    fn migrates_older_file_and_keeps_data() {
        let mut c = Connection::open_in_memory().unwrap();
        migrate(&mut c, MAIN_MIGRATIONS).unwrap();
        c.execute("INSERT INTO raw VALUES (x'00', 'json', x'7b7d')", [])
            .unwrap();
        let v2 = [
            MAIN_V1,
            "ALTER TABLE raw ADD COLUMN extra INTEGER NOT NULL DEFAULT 7",
        ];
        migrate(&mut c, &v2).unwrap();
        assert_eq!(main_version(&c).unwrap(), 2);
        let extra: i64 = c
            .query_row("SELECT extra FROM raw", [], |r| r.get(0))
            .unwrap();
        assert_eq!(extra, 7);
        migrate(&mut c, &v2).unwrap(); // idempotent at the current version
        let err = migrate(&mut c, MAIN_MIGRATIONS).unwrap_err();
        assert!(matches!(
            err.0,
            StoreError::SchemaTooNew {
                found: 2,
                supported: 1
            }
        ));
    }
}
