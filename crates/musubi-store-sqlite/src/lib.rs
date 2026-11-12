//! SQLite implementation of `musubi_model::store::{Storage, TenantStore}` and
//! `musubi_model::control::ControlStore` (docs/plan/phase1.md, chapter 3).
//!
//! Only `musubi-syncd` and tests may depend on this crate.
//!
//! Layout under the data directory (plan 3.1):
//!
//! ```text
//! control.db                        SqliteControl
//! tenants/<tenant_id>/main.db       SqliteTenant (+ -wal, -shm)
//! tenants/<tenant_id>/outbox.db     reserved for phase 3, backed up after main.db
//! ```
//!
//! Phase-0 conditions kept here (plan 3.2): `busy_timeout` is the first pragma
//! of every connection; writes run in `BEGIN IMMEDIATE` on a writer connection
//! separate from the readers; `main.db` commits with `synchronous=NORMAL`,
//! `control.db` with `FULL`; WAL checkpoints are run by the store itself
//! (see [`storage`]); `changes.seq` is `AUTOINCREMENT`.

mod control;
#[cfg(feature = "crash-points")]
pub mod crash;
mod schema;
mod storage;
mod tenant;

use std::path::Path;
use std::time::Duration;

use jiff::Timestamp;
use musubi_model::error::StoreError;
use rusqlite::{Connection, ErrorCode};
use serde::Serialize;
use serde::de::DeserializeOwned;

pub use control::SqliteControl;
pub use storage::{SqliteStorage, SqliteTenant};

/// How long a statement waits for a lock before `Busy`.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Internal error: lets `?` convert SQLite, JSON and I/O errors. Converted to
/// [`StoreError`] at the trait boundary.
#[derive(Debug)]
struct Error(StoreError);

type Result<T, E = Error> = std::result::Result<T, E>;

impl From<StoreError> for Error {
    fn from(e: StoreError) -> Self {
        Error(e)
    }
}

impl From<Error> for StoreError {
    fn from(e: Error) -> Self {
        e.0
    }
}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Error(match e.sqlite_error_code() {
            Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked) => StoreError::Busy,
            _ => StoreError::Backend(Box::new(e)),
        })
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error(StoreError::Io(e))
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        corrupt(e)
    }
}

fn corrupt(e: impl std::fmt::Display) -> Error {
    Error(StoreError::Corrupt(e.to_string()))
}

/// Opens a connection: `busy_timeout` first, so that switching to WAL already
/// waits on a lock instead of failing, then WAL and the tuning from the
/// phase-0 report (32 MB page cache, 256 MB mmap). `journal_size_limit` cuts the
/// WAL back to 4 MB after a reset. `autocheckpoint` is in pages: tenant files
/// pass 65536 (256 MB), only a fuse behind the store's own checkpointer;
/// control.db, which has no checkpointer, keeps SQLite's default 1000.
fn connect(path: &Path, synchronous: &str, autocheckpoint: u32) -> Result<Connection> {
    let c = Connection::open(path)?;
    c.busy_timeout(BUSY_TIMEOUT)?;
    c.execute_batch(&format!(
        "PRAGMA journal_mode=WAL; PRAGMA synchronous={synchronous}; PRAGMA temp_store=MEMORY;
         PRAGMA cache_size=-32768; PRAGMA mmap_size=268435456;
         PRAGMA journal_size_limit=4194304; PRAGMA wal_autocheckpoint={autocheckpoint};"
    ))?;
    c.set_prepared_statement_cache_capacity(64);
    Ok(c)
}

/// Timestamps are stored as microseconds since the Unix epoch.
fn us(t: Timestamp) -> i64 {
    t.as_microsecond()
}

fn ts(v: i64) -> Result<Timestamp> {
    Timestamp::from_microsecond(v).map_err(corrupt)
}

fn opt_ts(v: Option<i64>) -> Result<Option<Timestamp>> {
    v.map(ts).transpose()
}

/// A unit-variant serde enum (`snake_case`) as its text name.
fn enum_text<T: Serialize>(v: &T) -> Result<String> {
    match serde_json::to_value(v)? {
        serde_json::Value::String(s) => Ok(s),
        other => Err(corrupt(format!("not a unit variant: {other}"))),
    }
}

fn parse_enum<T: DeserializeOwned>(s: String) -> Result<T> {
    Ok(serde_json::from_value(serde_json::Value::String(s))?)
}
