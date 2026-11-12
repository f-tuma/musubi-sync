//! `Storage` behaviour of `SqliteStorage`: build flags, migration, the LRU,
//! hot backup and the WAL reset policy (plan 3.2).

mod common;

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use common::*;
use jiff::tz::TimeZone;
use musubi_model::journal::ChangeTarget;
use musubi_model::remote::MediaType;
use musubi_model::store::{ItemWrite, RangeQuery};
use musubi_model::{Raw, Seq, Storage, StoreError, TenantId, TenantStore, TimeWindow};
use musubi_store_sqlite::SqliteStorage;
use rusqlite::Connection;
use serde_json::value::RawValue;

fn main_db(dir: &Path, t: TenantId) -> std::path::PathBuf {
    dir.join("tenants").join(t.to_string()).join("main.db")
}

fn all(limit: u32) -> RangeQuery {
    RangeQuery {
        window: TimeWindow::new(t(-24), t(24 * 400)).unwrap(),
        collections: None,
        zone: TimeZone::UTC,
        after: None,
        limit,
        ids_only: false,
    }
}

/// An event whose raw body is `size` bytes, unique per key.
fn big(key: &str, size: usize) -> ItemWrite {
    let mut w = event(key, "1", 1);
    let body = format!(r#"{{"id":"{key}","pad":"{}"}}"#, "x".repeat(size));
    w.raw = Raw {
        media_type: MediaType::Json,
        body: body.clone().into_bytes(),
    };
    w.raw_hash = w.raw.hash();
    w.canonical = Some(RawValue::from_string(r#"{"big":true}"#.into()).unwrap());
    w
}

#[test]
fn sqlite_is_built_without_memstatus() {
    let c = Connection::open_in_memory().unwrap();
    let mut s = c.prepare("PRAGMA compile_options").unwrap();
    let opts: Vec<String> = s
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert!(opts.iter().any(|o| o == "DEFAULT_MEMSTATUS=0"), "{opts:?}");
}

#[test]
fn older_file_migrates_and_newer_is_refused_untouched() {
    let d = TempDir::new();
    let s = SqliteStorage::new(d.path(), 4).unwrap();
    // Version 0: an empty file not made by `create_tenant`.
    let old = TenantId::from_u128(1);
    std::fs::create_dir_all(main_db(d.path(), old).parent().unwrap()).unwrap();
    Connection::open(main_db(d.path(), old)).unwrap();
    let mut tn = s.open(old).unwrap();
    let col = collection(&mut tn, "a");
    tn.apply_page(&page(col, vec![event("e", "1", 1)])).unwrap();
    assert_eq!(tn.stats().unwrap().items, 1);
    drop(tn);

    let new = TenantId::from_u128(2);
    s.create_tenant(new).unwrap();
    let path = main_db(d.path(), new);
    {
        let c = Connection::open(&path).unwrap();
        c.execute("UPDATE meta SET v = 99 WHERE k = 'schema'", [])
            .unwrap();
        c.pragma_update(None, "journal_mode", "DELETE").unwrap();
    }
    let before = std::fs::read(&path).unwrap();
    assert!(matches!(
        s.open(new),
        Err(StoreError::SchemaTooNew {
            found: 99,
            supported: 1
        })
    ));
    assert_eq!(std::fs::read(&path).unwrap(), before, "file untouched");
    assert!(!path.with_extension("db-wal").exists());
}

#[test]
fn interrupted_creation_leaves_nothing_behind() {
    let d = TempDir::new();
    let tenants = d.path().join("tenants");
    // A crash before the rename: a half-built directory beside the tenants.
    let leftover = tenants.join(format!(".{TENANT}.123.new"));
    std::fs::create_dir_all(&leftover).unwrap();
    std::fs::write(leftover.join("main.db"), b"").unwrap();
    // An empty tenant directory (made by hand or by an older build).
    std::fs::create_dir(tenants.join(TENANT.to_string())).unwrap();

    let s = SqliteStorage::new(d.path(), 2).unwrap();
    assert!(!leftover.exists(), "leftover removed on start");
    s.create_tenant(TENANT).unwrap();
    let mut tn = s.open(TENANT).unwrap();
    let col = collection(&mut tn, "a");
    tn.apply_page(&page(col, vec![event("e", "1", 1)])).unwrap();
    assert!(matches!(
        s.create_tenant(TENANT),
        Err(StoreError::AlreadyExists)
    ));
    let names: Vec<_> = std::fs::read_dir(&tenants)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(names, [TENANT.to_string()]);
}

/// Open descriptors of this process pointing at `path`.
fn fds(path: &Path) -> usize {
    let path = path.canonicalize().unwrap();
    std::fs::read_dir("/proc/self/fd")
        .unwrap()
        .filter_map(|e| std::fs::read_link(e.ok()?.path()).ok())
        .filter(|l| *l == path)
        .count()
}

#[test]
fn lru_closes_least_recently_used_tenant() {
    let d = TempDir::new();
    let s = SqliteStorage::new(d.path(), 2).unwrap();
    let ts: Vec<_> = (1..=3).map(TenantId::from_u128).collect();
    for t_ in &ts {
        s.create_tenant(*t_).unwrap();
    }
    let first = main_db(d.path(), ts[0]);
    drop(s.open(ts[0]).unwrap());
    assert!(fds(&first) > 0, "kept open by the LRU");
    drop(s.open(ts[1]).unwrap());
    drop(s.open(ts[2]).unwrap());
    // The checkpointer may hold a handle for one sweep.
    let deadline = Instant::now() + Duration::from_secs(2);
    while fds(&first) > 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(fds(&first), 0, "evicted tenant's files are closed");
    assert!(fds(&main_db(d.path(), ts[2])) > 0);

    let start = Instant::now();
    let tn = s.open(ts[0]).unwrap();
    tn.stats().unwrap();
    // Measured, not enforced (target < 1 ms).
    println!("reopen of an evicted tenant: {:?}", start.elapsed());
}

#[test]
fn hot_backup_restores_the_snapshot() {
    let d = TempDir::new();
    let s = Arc::new(storage(d.path()));
    let mut tn = s.open(TENANT).unwrap();
    let col = collection(&mut tn, "a");
    for p in 0..20 {
        let items = (0..50)
            .map(|i| event(&format!("pre{p}-{i}"), "1", i))
            .collect();
        tn.apply_page(&page(col, items)).unwrap();
    }
    let stop = Arc::new(AtomicBool::new(false));
    let writer = {
        let stop = stop.clone();
        std::thread::spawn(move || {
            let mut n = 0;
            while !stop.load(Ordering::Relaxed) {
                let items = (0..10)
                    .map(|i| event(&format!("w{n}-{i}"), "1", i))
                    .collect();
                tn.apply_page(&page(col, items)).unwrap();
                n += 1;
            }
            n
        })
    };
    let src = s.open(TENANT).unwrap();
    while src.stats().unwrap().head.0 < 1100 {
        std::thread::yield_now();
    }
    let out = TempDir::new();
    let started = src.stats().unwrap().head;
    let info = s.backup(TENANT, out.path()).unwrap();
    std::thread::sleep(Duration::from_millis(50));
    stop.store(true, Ordering::Relaxed);
    let pages = writer.join().unwrap();
    let ended = src.stats().unwrap().head;
    assert!(
        started <= info.head && info.head < ended,
        "{started:?} {info:?} {ended:?} after {pages} pages"
    );
    assert_eq!(info.files, [out.path().join("main.db")]);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&info.files[0])
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "created private, whatever the umask");
    }

    let restored_dir = TempDir::new();
    let dst = main_db(restored_dir.path(), TENANT);
    std::fs::create_dir_all(dst.parent().unwrap()).unwrap();
    std::fs::copy(&info.files[0], &dst).unwrap();
    let r = SqliteStorage::new(restored_dir.path(), 2).unwrap();
    let restored = r.open(TENANT).unwrap();
    assert_eq!(restored.stats().unwrap().head, info.head);

    // changes: the source's journal up to the backup head.
    let want: Vec<_> = journal(&src, Seq(0))
        .into_iter()
        .filter(|c| c.seq <= info.head)
        .collect();
    let got = journal(&restored, Seq(0));
    assert_eq!(got, want);

    // range: the source's occurrences of entities existing at the backup head.
    let entities: HashSet<_> = want
        .iter()
        .filter_map(|c| match c.target {
            ChangeTarget::Entity(e) => Some(e),
            _ => None,
        })
        .collect();
    let view = |p: musubi_model::store::RangePage| {
        assert!(p.next.is_none());
        p.occurrences
            .into_iter()
            .map(|o| {
                (
                    o.entity,
                    o.start,
                    o.end,
                    o.canonical.map(|c| c.get().to_owned()),
                )
            })
            .collect::<Vec<_>>()
    };
    let want: Vec<_> = view(src.range(&all(100_000)).unwrap())
        .into_iter()
        .filter(|o| entities.contains(&o.0))
        .collect();
    assert!(!want.is_empty());
    assert_eq!(view(restored.range(&all(100_000)).unwrap()), want);
}

/// Over the hard limit a page triggers a bounded TRUNCATE. With a reader
/// holding an old snapshot the reset fails and backs off; once the reader is
/// gone the next eligible page resets the WAL.
#[test]
fn wal_reset_fails_backs_off_and_recovers() {
    let d = TempDir::new();
    let s = storage(d.path());
    let mut tn = s.open(TENANT).unwrap();
    let col = collection(&mut tn, "a");
    tn.apply_page(&page(col, vec![event("seed", "1", 1)]))
        .unwrap();

    let mut reader = Connection::open(main_db(d.path(), TENANT)).unwrap();
    let snapshot = reader.transaction().unwrap();
    let _: i64 = snapshot
        .query_row("SELECT count(*) FROM items", [], |r| r.get(0))
        .unwrap();

    let mut n = 0;
    let mut write = |tn: &mut musubi_store_sqlite::SqliteTenant| {
        let items = (0..16)
            .map(|i| big(&format!("b{n}-{i}"), 64 << 10))
            .collect();
        tn.apply_page(&page(col, items)).unwrap();
        n += 1;
    };
    while tn.stats().unwrap().reset_failures == 0 {
        write(&mut tn);
        assert!(
            tn.stats().unwrap().wal_bytes < 512 << 20,
            "no reset attempt"
        );
    }
    let st = tn.stats().unwrap();
    assert!(st.wal_bytes >= 64 << 20);
    assert_eq!(st.truncates, 0);
    // Backoff: the next page (well under +32 MB, within 1 s) does not retry.
    write(&mut tn);
    assert_eq!(tn.stats().unwrap().reset_failures, 1);

    drop(snapshot);
    drop(reader);
    std::thread::sleep(Duration::from_millis(1100));
    // Without the reader the background PASSIVE has caught up and the WAL
    // restarts small, so one page must take it past the limit again. A
    // background PASSIVE running longer than the reset's 10 ms drain still
    // fails the reset like a reader would, hence the retries.
    for _ in 0..3 {
        let items = (0..70)
            .map(|i| big(&format!("r{n}-{i}"), 1 << 20))
            .collect();
        tn.apply_page(&page(col, items)).unwrap();
        n += 1;
        if tn.stats().unwrap().truncates > 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(1100));
    }
    let st = tn.stats().unwrap();
    assert_eq!(st.truncates, 1, "{st:?}");
    assert!(st.truncate_max > Duration::ZERO);
    assert!(st.wal_bytes < 8 << 20, "{st:?}");
    assert!(st.wal_max_bytes >= 64 << 20);
    println!("{st:?}");
}
