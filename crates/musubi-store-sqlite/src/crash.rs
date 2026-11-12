//! Test-only crash points (feature `crash-points`, off by default and never
//! enabled by a release build). The whole-binary crash test kills the node at
//! them (`musubi-syncd/tests/crash.rs`).
//!
//! `MUSUBI_CRASH_AT=<name>[:<n>]` makes the n-th pass (default the first)
//! through point `<name>` print `musubi-crashpoint: <name>` to stderr; a
//! blocking point then parks its thread until the harness sends SIGKILL.
//! `MUSUBI_WAL_HARD=<bytes>` lowers the WAL reset limit so small test data
//! reaches `wal_checkpoint(TRUNCATE)`.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

/// Marks crash point `name`; see the module documentation.
pub fn point(name: &str, block: bool) {
    static AT: OnceLock<Option<(String, u64)>> = OnceLock::new();
    static HITS: AtomicU64 = AtomicU64::new(0);
    let at = AT.get_or_init(|| {
        let v = std::env::var("MUSUBI_CRASH_AT").ok()?;
        Some(match v.split_once(':') {
            Some((name, n)) => (name.to_owned(), n.parse().ok()?),
            None => (v, 1),
        })
    });
    let Some((at, n)) = at else { return };
    if at != name || HITS.fetch_add(1, Ordering::Relaxed) + 1 != *n {
        return;
    }
    eprintln!("musubi-crashpoint: {name}");
    if block {
        loop {
            std::thread::park();
        }
    }
}

/// The `MUSUBI_WAL_HARD` override.
pub(crate) fn wal_hard() -> Option<u64> {
    static V: OnceLock<Option<u64>> = OnceLock::new();
    *V.get_or_init(|| std::env::var("MUSUBI_WAL_HARD").ok()?.parse().ok())
}
