//! [`SqliteStorage`]: tenant files, the LRU of open tenants, the checkpointer
//! and backups.
//!
//! Checkpoints (phase-0 condition 4, measured in `docs/research/phase0/storage.md`):
//! a background thread samples every open tenant's WAL every 50 ms and runs
//! `PASSIVE` from 8 MB (smaller WALs at most once a second). Resetting the WAL
//! (`TRUNCATE`) is the writer's job, after a commit that left the WAL at 64 MB or
//! more: it already owns the write lock between its transactions, a separate
//! thread would be starved by a busy writer. The reset waits for readers at most
//! 10 ms; if they are still there it backs off for 1 s or 32 MB of WAL growth.
//!
//! Waiting for readers alone does not work under steady read load: overlapping
//! readers keep the WAL read marks share-locked at old frames, so SQLite can
//! neither backfill past them nor restart the log. Reads of a tenant therefore
//! pass a [`Gate`]: the reset stops new reads, waits at most 10 ms for running
//! ones (bounded read transactions) and lets them go again after the reset. A
//! `PASSIVE` right before it, outside the gate, keeps that pause short.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use musubi_model::error::StoreError;
use musubi_model::id::{Seq, TenantId};
use musubi_model::store::{BackupInfo, CheckpointInfo, CheckpointMode, Storage};
use rusqlite::backup::{Backup, StepResult};
use rusqlite::{Connection, ErrorCode, OpenFlags, Transaction, TransactionBehavior};

use crate::schema::{MAIN_MIGRATIONS, MAIN_VERSION, main_version, migrate};
use crate::{BUSY_TIMEOUT, Error, Result, connect};

const WAL_SOFT: u64 = 8 << 20;
const WAL_HARD: u64 = 64 << 20;
const CKPT_WAIT: Duration = Duration::from_millis(10);
const RESET_BACKOFF: Duration = Duration::from_secs(1);
const RESET_BACKOFF_GROWTH: u64 = 32 << 20;
const SWEEP: Duration = Duration::from_millis(50);
/// `wal_autocheckpoint` (pages) of tenant files: a 256 MB fuse behind the checkpointer.
const TENANT_AUTOCHECKPOINT: u32 = 65536;
/// Suffix of a tenant directory under construction.
const TMP_SUFFIX: &str = ".new";
/// How long a backup retries a busy or locked source.
const BACKUP_BUSY: Duration = Duration::from_secs(30);
/// Idle read connections kept per tenant.
const IDLE_READERS: usize = 4;

/// Tenant files under a data directory. See the crate documentation for the layout.
pub struct SqliteStorage {
    inner: Arc<Inner>,
    checkpointer: Option<JoinHandle<()>>,
}

struct Inner {
    tenants: PathBuf,
    capacity: usize,
    open: Mutex<Open>,
    stop: AtomicBool,
}

/// Open tenants. `lru` holds the cached ones; `live` every tenant that still
/// has a handle somewhere, so that a reopen after eviction finds the same
/// writer instead of creating a second one for the file.
#[derive(Default)]
struct Open {
    tick: u64,
    lru: HashMap<TenantId, (Arc<Shared>, u64)>,
    live: HashMap<TenantId, Weak<Shared>>,
}

impl SqliteStorage {
    /// Opens the data directory `dir` (created if missing), keeping at most
    /// `capacity` tenants open (at least 1), and starts the background checkpointer.
    pub fn new(dir: &Path, capacity: usize) -> Result<Self, StoreError> {
        let tenants = dir.join("tenants");
        fs::create_dir_all(&tenants).map_err(StoreError::Io)?;
        // Tenants whose creation was interrupted (see `create_tenant`).
        for e in fs::read_dir(&tenants).map_err(StoreError::Io)? {
            let e = e.map_err(StoreError::Io)?;
            if e.file_name().to_string_lossy().ends_with(TMP_SUFFIX) {
                fs::remove_dir_all(e.path()).map_err(StoreError::Io)?;
            }
        }
        let inner = Arc::new(Inner {
            tenants,
            capacity: capacity.max(1),
            open: Mutex::default(),
            stop: AtomicBool::new(false),
        });
        let bg = inner.clone();
        let checkpointer = std::thread::Builder::new()
            .name("musubi-checkpointer".into())
            .spawn(move || bg.checkpoint_loop())
            .map_err(StoreError::Io)?;
        Ok(Self {
            inner,
            checkpointer: Some(checkpointer),
        })
    }
}

impl Drop for SqliteStorage {
    fn drop(&mut self) {
        self.inner.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.checkpointer.take() {
            let _ = h.join();
        }
    }
}

impl Inner {
    fn dir(&self, t: TenantId) -> PathBuf {
        self.tenants.join(t.to_string())
    }

    fn open_mut(&self) -> MutexGuard<'_, Open> {
        self.open.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn open(&self, t: TenantId) -> Result<Arc<Shared>> {
        let mut o = self.open_mut();
        // Every cached tenant is also live.
        let shared = match o.live.get(&t).and_then(Weak::upgrade) {
            Some(s) => s,
            None => {
                // Opening may migrate: not under the lock every tenant's open takes.
                drop(o);
                let opened = Arc::new(Shared::open(&self.dir(t))?);
                o = self.open_mut();
                // A concurrent open of the same tenant may have won; keep one writer.
                match o.live.get(&t).and_then(Weak::upgrade) {
                    Some(s) => s,
                    None => {
                        o.live.insert(t, Arc::downgrade(&opened));
                        opened
                    }
                }
            }
        };
        o.tick += 1;
        let tick = o.tick;
        if let Some((_, used)) = o.lru.get_mut(&t) {
            *used = tick;
            return Ok(shared);
        }
        if o.lru.len() >= self.capacity
            && let Some(oldest) = o
                .lru
                .iter()
                .min_by_key(|(_, (_, used))| *used)
                .map(|(k, _)| *k)
        {
            o.lru.remove(&oldest);
        }
        o.lru.insert(t, (shared.clone(), tick));
        o.live.retain(|_, w| w.strong_count() > 0);
        Ok(shared)
    }

    fn checkpoint_loop(&self) {
        let mut last: HashMap<TenantId, Instant> = HashMap::new();
        while !self.stop.load(Ordering::Relaxed) {
            let live: Vec<(TenantId, Arc<Shared>)> = self
                .open_mut()
                .live
                .iter()
                .filter_map(|(t, w)| Some((*t, w.upgrade()?)))
                .collect();
            last.retain(|t, _| live.iter().any(|(l, _)| l == t));
            for (t, s) in live {
                let size = s.wal_size();
                if size == 0 {
                    continue;
                }
                // Over the hard limit the writer resets the WAL; a PASSIVE here
                // would hold the checkpoint lock and make that reset fail.
                let since = last.entry(t).or_insert_with(Instant::now);
                let throttle = !(WAL_SOFT..WAL_HARD).contains(&size);
                if throttle && since.elapsed() < Duration::from_secs(1) {
                    continue;
                }
                *since = Instant::now();
                // Errors are retried on the next sweep; the fuse bounds the WAL meanwhile.
                let _ = s.passive();
            }
            std::thread::sleep(SWEEP);
        }
    }
}

impl Storage for SqliteStorage {
    type Tenant = SqliteTenant;

    fn open(&self, t: TenantId) -> Result<SqliteTenant, StoreError> {
        Ok(SqliteTenant {
            shared: self.inner.open(t)?,
        })
    }

    fn create_tenant(&self, t: TenantId) -> Result<(), StoreError> {
        let dir = self.inner.dir(t);
        if dir.join("main.db").exists() {
            return Err(StoreError::AlreadyExists);
        }
        // Built aside and renamed into place, so a crash never leaves a
        // half-created tenant; `new` removes the leftovers.
        let nanos = jiff::Timestamp::now().as_nanosecond();
        let tmp = self.inner.tenants.join(format!(".{t}.{nanos}{TMP_SUFFIX}"));
        let built = (|| {
            fs::create_dir(&tmp)?;
            let mut c = connect(&tmp.join("main.db"), "NORMAL", TENANT_AUTOCHECKPOINT)?;
            migrate(&mut c, MAIN_MIGRATIONS)?;
            drop(c);
            // Replaces a missing or empty directory; a non-empty one is a tenant.
            fs::rename(&tmp, &dir).map_err(|e| match e.kind() {
                std::io::ErrorKind::DirectoryNotEmpty | std::io::ErrorKind::AlreadyExists => {
                    Error(StoreError::AlreadyExists)
                }
                _ => Error(StoreError::Io(e)),
            })?;
            fs::File::open(&self.inner.tenants)?.sync_all()?;
            Ok::<_, Error>(())
        })();
        if built.is_err() {
            let _ = fs::remove_dir_all(&tmp);
        }
        Ok(built?)
    }

    fn delete_tenant(&self, t: TenantId) -> Result<(), StoreError> {
        {
            let mut o = self.inner.open_mut();
            o.lru.remove(&t);
            o.live.remove(&t);
        }
        // Handles still held elsewhere keep working on the unlinked files until dropped.
        match fs::remove_dir_all(self.inner.dir(t)) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(StoreError::NotFound),
            r => r.map_err(StoreError::Io),
        }
    }

    fn backup(&self, t: TenantId, dest: &Path) -> Result<BackupInfo, StoreError> {
        if fs::read_dir(dest).map_err(StoreError::Io)?.next().is_some() {
            return Err(StoreError::Io(std::io::Error::new(
                std::io::ErrorKind::DirectoryNotEmpty,
                "backup destination is not empty",
            )));
        }
        // Straight from the files: a backup does not evict an open tenant from the LRU.
        let dir = self.inner.dir(t);
        if !dir.join("main.db").exists() {
            return Err(StoreError::NotFound);
        }
        let mut info = BackupInfo {
            files: Vec::new(),
            bytes: 0,
            head: Seq(0),
        };
        // Order is fixed (plan 3.2): main.db first, then outbox.db (phase 3).
        for name in ["main.db", "outbox.db"] {
            let src = dir.join(name);
            if !src.exists() {
                continue;
            }
            let out = dest.join(name);
            let dst = snapshot(&src, &out)?;
            if name == "main.db" {
                info.head = crate::tenant::head(&dst)?;
            }
            drop(dst);
            info.bytes += fs::metadata(&out).map_err(StoreError::Io)?.len();
            info.files.push(out);
        }
        Ok(info)
    }
}

/// Copies `src` to `out` through the backup API in one step: the source read
/// transaction is a consistent snapshot while writers go on (WAL), and a step
/// covering all pages cannot be restarted by their commits.
fn snapshot(src: &Path, out: &Path) -> Result<Connection> {
    let from = open_existing(src)?;
    let mut to = Connection::open(out)?;
    {
        let b = Backup::new(&from, &mut to)?;
        let deadline = Instant::now() + BACKUP_BUSY;
        loop {
            match b.step(-1)? {
                StepResult::Done => break,
                _ if Instant::now() >= deadline => return Err(StoreError::Busy.into()),
                // Busy or locked: wait and retry.
                _ => std::thread::sleep(Duration::from_millis(5)),
            }
        }
    }
    Ok(to)
}

/// Opens an existing file without changing it (no create, no pragmas besides the timeout).
fn open_existing(path: &Path) -> Result<Connection> {
    let c = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    c.busy_timeout(BUSY_TIMEOUT)?;
    Ok(c)
}

/// Handle to one tenant (`main.db`). Cheap to clone through [`Storage::open`];
/// all handles of a tenant share one writer connection and a pool of readers.
pub struct SqliteTenant {
    pub(crate) shared: Arc<Shared>,
}

impl SqliteTenant {
    /// Longest time a read of this process waited for a WAL reset since the
    /// tenant was opened. Not part of `StoreStats` (a model type).
    pub fn gate_wait_max(&self) -> Duration {
        Duration::from_micros(self.shared.gate.wait_max_us.load(Ordering::Relaxed))
    }
}

pub(crate) struct Shared {
    dir: PathBuf,
    main: PathBuf,
    wal: PathBuf,
    writer: Mutex<Writer>,
    readers: Mutex<Vec<Connection>>,
    gate: Gate,
    pub(crate) stats: Stats,
}

/// Lets the WAL reset drain this process's readers of a tenant.
#[derive(Default)]
struct Gate {
    /// Running reads, whether a reset is draining them, and whether a reset
    /// is running at all (which keeps out only the background PASSIVE).
    state: Mutex<(usize, bool, bool)>,
    changed: Condvar,
    /// Longest time a read waited for a reset, in microseconds.
    wait_max_us: AtomicU64,
}

impl Gate {
    fn lock(&self) -> MutexGuard<'_, (usize, bool, bool)> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// `passive`: a PASSIVE holds the checkpoint lock, which would fail a
    /// running reset at once, so it also waits for the reset to end.
    fn enter(&self, passive: bool) -> GateRead<'_> {
        let mut s = self.lock();
        let blocked = |s: &mut (usize, bool, bool)| s.1 || (passive && s.2);
        if blocked(&mut s) {
            let start = Instant::now();
            s = self
                .changed
                .wait_while(s, blocked)
                .unwrap_or_else(|p| p.into_inner());
            if !passive {
                let us = u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX);
                self.wait_max_us.fetch_max(us, Ordering::Relaxed);
            }
        }
        s.0 += 1;
        GateRead(self)
    }

    /// Stops new reads and waits up to `wait` for running ones. `None` if
    /// they did not finish (reads are let go again).
    fn drain(&self, wait: Duration) -> Option<GateDrain<'_>> {
        let mut s = self.lock();
        if s.2 {
            return None;
        }
        (s.1, s.2) = (true, true);
        let (mut s, _) = self
            .changed
            .wait_timeout_while(s, wait, |s| s.0 > 0)
            .unwrap_or_else(|p| p.into_inner());
        if s.0 == 0 {
            return Some(GateDrain(self));
        }
        (s.1, s.2) = (false, false);
        self.changed.notify_all();
        None
    }
}

struct GateRead<'a>(&'a Gate);

impl Drop for GateRead<'_> {
    fn drop(&mut self) {
        let mut s = self.0.lock();
        s.0 -= 1;
        if s.0 == 0 {
            self.0.changed.notify_all();
        }
    }
}

struct GateDrain<'a>(&'a Gate);

impl GateDrain<'_> {
    /// Lets reads in again; the background PASSIVE stays out until drop.
    fn let_reads_in(&self) {
        self.0.lock().1 = false;
        self.0.changed.notify_all();
    }
}

impl Drop for GateDrain<'_> {
    fn drop(&mut self) {
        let mut s = self.0.lock();
        (s.1, s.2) = (false, false);
        self.0.changed.notify_all();
    }
}

struct Writer {
    conn: Connection,
    /// No reset before this instant unless the WAL reaches this size.
    next_reset: (Instant, u64),
}

/// Checkpoint metrics since the tenant was opened (phase-0 condition 8).
#[derive(Default)]
pub(crate) struct Stats {
    pub(crate) wal_max: AtomicU64,
    pub(crate) truncates: AtomicU64,
    pub(crate) truncate_max_us: AtomicU64,
    pub(crate) reset_failures: AtomicU64,
}

impl Shared {
    /// Opens and, if older, migrates a tenant file. A newer file is
    /// `SchemaTooNew` before any pragma or write touches it.
    fn open(dir: &Path) -> Result<Self> {
        let main = dir.join("main.db");
        let probe = open_existing(&main).map_err(|e| match e.0 {
            StoreError::Backend(_) if !main.exists() => Error(StoreError::NotFound),
            _ => e,
        })?;
        let found = main_version(&probe)?;
        if found > MAIN_VERSION {
            return Err(StoreError::SchemaTooNew {
                found,
                supported: MAIN_VERSION,
            }
            .into());
        }
        drop(probe);
        let mut conn = connect(&main, "NORMAL", TENANT_AUTOCHECKPOINT)?;
        migrate(&mut conn, MAIN_MIGRATIONS)?;
        let wal = dir.join("main.db-wal");
        Ok(Self {
            dir: dir.to_owned(),
            main,
            wal,
            writer: Mutex::new(Writer {
                conn,
                next_reset: (Instant::now(), 0),
            }),
            readers: Mutex::default(),
            gate: Gate::default(),
            stats: Stats::default(),
        })
    }

    pub(crate) fn wal_size(&self) -> u64 {
        let size = fs::metadata(&self.wal).map_or(0, |m| m.len());
        self.stats.wal_max.fetch_max(size, Ordering::Relaxed);
        size
    }

    pub(crate) fn main_size(&self) -> u64 {
        fs::metadata(&self.main).map_or(0, |m| m.len())
    }

    fn reader(&self) -> Result<Connection> {
        let idle = self.readers.lock().unwrap_or_else(|p| p.into_inner()).pop();
        match idle {
            Some(c) => Ok(c),
            None => connect(&self.main, "NORMAL", TENANT_AUTOCHECKPOINT),
        }
    }

    fn put_reader(&self, c: Connection) {
        let mut idle = self.readers.lock().unwrap_or_else(|p| p.into_inner());
        if idle.len() < IDLE_READERS {
            idle.push(c);
        }
    }

    /// Runs `f` in one short read transaction (a consistent snapshot).
    pub(crate) fn read<T>(&self, f: impl FnOnce(&Transaction<'_>) -> Result<T>) -> Result<T> {
        let _read = self.gate.enter(false);
        let mut c = self.reader()?;
        let r = {
            let tx = c.transaction()?;
            f(&tx)
        };
        self.put_reader(c);
        r
    }

    /// Runs `f` in one `BEGIN IMMEDIATE` transaction on the writer connection,
    /// then resets the WAL if it grew past the hard limit.
    pub(crate) fn write<T>(&self, f: impl FnOnce(&Transaction<'_>) -> Result<T>) -> Result<T> {
        let mut w = self.writer.lock().unwrap_or_else(|p| p.into_inner());
        let tx = w
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let r = f(&tx)?;
        tx.commit()?;
        let size = self.wal_size();
        if size >= WAL_HARD && (Instant::now() >= w.next_reset.0 || size >= w.next_reset.1) {
            // The page is committed; a failed reset only delays the next one.
            if let Err(e) = self.truncate(&mut w, size) {
                tracing::warn!(error = %e.0, dir = %self.dir.display(), "WAL reset failed");
            }
        }
        Ok(r)
    }

    fn truncate(&self, w: &mut Writer, size: u64) -> Result<(bool, Duration)> {
        // Copy what readers allow while they still run; busy (a background
        // PASSIVE) only leaves more for the TRUNCATE.
        let _ = w
            .conn
            .query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |_| Ok(()));
        let start = Instant::now();
        let r: rusqlite::Result<i64> = match self.gate.drain(CKPT_WAIT) {
            Some(drain) => {
                // The old snapshots are gone and the writer adds no frames
                // meanwhile, so new reads need not wait for the fsyncs.
                drain.let_reads_in();
                // Readers outside the gate (a backup) get the same bounded wait.
                w.conn.busy_timeout(CKPT_WAIT)?;
                let r = w
                    .conn
                    .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| r.get(0));
                w.conn.busy_timeout(BUSY_TIMEOUT)?;
                r
            }
            None => Ok(1),
        };
        let took = start.elapsed();
        let micros = u64::try_from(took.as_micros()).unwrap_or(u64::MAX);
        self.stats
            .truncate_max_us
            .fetch_max(micros, Ordering::Relaxed);
        let reset = match r {
            Ok(busy) => Ok(busy == 0),
            Err(e)
                if matches!(
                    e.sqlite_error_code(),
                    Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked)
                ) =>
            {
                Ok(false)
            }
            Err(e) => Err(e),
        };
        if let Ok(true) = reset {
            self.stats.truncates.fetch_add(1, Ordering::Relaxed);
            w.next_reset = (Instant::now(), 0);
        } else {
            // Busy or an error: either way back off instead of retrying every page.
            self.stats.reset_failures.fetch_add(1, Ordering::Relaxed);
            w.next_reset = (Instant::now() + RESET_BACKOFF, size + RESET_BACKOFF_GROWTH);
        }
        Ok((reset?, took))
    }

    fn passive(&self) -> Result<()> {
        let _read = self.gate.enter(true);
        let c = self.reader()?;
        let r = c.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |_| Ok(()));
        self.put_reader(c);
        Ok(r?)
    }

    pub(crate) fn checkpoint(&self, mode: CheckpointMode) -> Result<CheckpointInfo> {
        let wal_bytes_before = self.wal_size();
        let start = Instant::now();
        let reset = match mode {
            CheckpointMode::Passive => {
                self.passive()?;
                false
            }
            CheckpointMode::Truncate => {
                let mut w = self.writer.lock().unwrap_or_else(|p| p.into_inner());
                self.truncate(&mut w, wal_bytes_before)?.0
            }
        };
        Ok(CheckpointInfo {
            mode,
            reset,
            wal_bytes_before,
            took: start.elapsed(),
        })
    }
}
