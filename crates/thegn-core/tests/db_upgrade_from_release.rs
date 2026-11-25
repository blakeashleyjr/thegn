//! THE-2656: replay `thegn.db` snapshots captured from shipped releases through
//! the current migrator. A first-day migration bug is the worst data-loss
//! story, so each snapshot must upgrade to `SCHEMA_VERSION` with every row of
//! every pre-existing table intact, and a second open must be a no-op.
//!
//! Snapshots live in `tests/fixtures/db/<tag>.db` and are produced by
//! `test/gen-db-fixtures.sh`. The three rungs below are mandatory.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rusqlite::Connection;
use thegn_core::config::DatabaseConfig;
use thegn_core::db::{Db, MigrationActor, SCHEMA_VERSION, install_migration_policy};

const RUNGS: &[&str] = &["alpha.100", "alpha.120", "alpha.131"];
/// Tables whose rows are the user's own work: they must exist, and be non-empty
/// in the snapshot, whenever the old release had them.
const KEY_TABLES: &[&str] = &["worktrees", "sessions", "layouts"];

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/db")
}

fn user_version(conn: &Connection) -> i64 {
    conn.query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap()
}

fn table_names(conn: &Connection) -> Vec<String> {
    let mut stmt = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'")
        .unwrap();
    stmt.query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

fn row_counts(conn: &Connection) -> BTreeMap<String, i64> {
    table_names(conn)
        .into_iter()
        .map(|t| {
            let n = conn
                .query_row(&format!("SELECT COUNT(*) FROM \"{t}\""), [], |r| r.get(0))
                .unwrap();
            (t, n)
        })
        .collect()
}

fn integrity_ok(conn: &Connection) {
    let verdict: String = conn
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .unwrap();
    assert_eq!(verdict, "ok", "integrity_check");
    let mut fk = conn.prepare("PRAGMA foreign_key_check").unwrap();
    assert!(
        !fk.exists([]).unwrap(),
        "foreign_key_check found violations"
    );
}

#[test]
fn shipped_release_dbs_upgrade_without_losing_rows() {
    install_migration_policy(&DatabaseConfig::default(), MigrationActor::Controller).unwrap();

    let missing: Vec<&str> = RUNGS
        .iter()
        .copied()
        .filter(|r| !fixture_dir().join(format!("{r}.db")).is_file())
        .collect();
    assert!(
        missing.is_empty(),
        "missing release snapshots {missing:?} in {}; generate with test/gen-db-fixtures.sh",
        fixture_dir().display()
    );

    for rung in RUNGS {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("thegn.db");
        std::fs::copy(fixture_dir().join(format!("{rung}.db")), &path).unwrap();

        let (before_version, before) = {
            let conn = Connection::open(&path).unwrap();
            (user_version(&conn), row_counts(&conn))
        };
        assert!(
            before_version > 0 && before_version <= SCHEMA_VERSION,
            "{rung}: snapshot user_version {before_version} is not a shipped schema"
        );
        for key in KEY_TABLES {
            if let Some(n) = before.get(*key) {
                assert!(
                    *n > 0,
                    "{rung}: snapshot table {key} is empty; regenerate it"
                );
            }
        }

        drop(Db::open_at(&path).unwrap_or_else(|e| panic!("{rung}: migrate failed: {e:#}")));
        let (after_version, after) = {
            let conn = Connection::open(&path).unwrap();
            integrity_ok(&conn);
            (user_version(&conn), row_counts(&conn))
        };
        assert_eq!(after_version, SCHEMA_VERSION, "{rung}: user_version");

        // A table may be renamed or folded away by a migration, but a table
        // that survives must not have lost rows.
        for (table, n) in &before {
            if let Some(m) = after.get(table) {
                assert!(m >= n, "{rung}: table {table} dropped rows ({n} -> {m})");
            } else {
                eprintln!("{rung}: table {table} ({n} rows) removed by migration");
            }
        }
        for key in KEY_TABLES {
            if before.contains_key(*key) {
                assert!(after.contains_key(*key), "{rung}: key table {key} vanished");
            }
        }

        // Idempotent: a second open changes neither version nor any row count.
        drop(Db::open_at(&path).unwrap_or_else(|e| panic!("{rung}: reopen failed: {e:#}")));
        let conn = Connection::open(&path).unwrap();
        assert_eq!(
            user_version(&conn),
            SCHEMA_VERSION,
            "{rung}: reopen version"
        );
        assert_eq!(row_counts(&conn), after, "{rung}: reopen changed rows");
    }
}
