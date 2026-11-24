//! Tenant files as earlier binaries left them, for upgrade tests.

use std::path::Path;

/// Turns a tenant's `main.db` into the `MAIN_V2` file a binary without task
/// occurrences (K11) had: no task rows or series spans, no `done` column,
/// schema 2. The next open migrates it to `MAIN_V3` (docs/plan/phase2.md 12.4).
///
/// # Panics
/// If the file cannot be opened or changed.
pub fn downgrade_to_main_v2(main_db: &Path) {
    let c = rusqlite::Connection::open(main_db).expect("open main.db");
    c.execute_batch(
        "DELETE FROM occurrences WHERE entity IN (SELECT id FROM entities WHERE kind = 'task');
         UPDATE entities SET series_first = NULL, series_last = NULL, indexed_until = NULL
           WHERE kind = 'task';
         ALTER TABLE occurrences DROP COLUMN done;
         UPDATE meta SET v = 2 WHERE k = 'schema';",
    )
    .expect("downgrade to MAIN_V2");
}
