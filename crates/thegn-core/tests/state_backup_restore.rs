//! Proves the documented state-database backup and restore procedure.

use std::path::Path;

use rusqlite::Connection;
use thegn_core::db::{Db, MigrationAccessError, SCHEMA_VERSION};
use thegn_core::store::CacheStore;
use thegn_core::testenv::IsolatedState;

fn copy_state_files(src_db: &Path, dest_dir: &Path) -> Vec<String> {
    let mut copied = Vec::new();
    for name in ["thegn.db", "thegn.db-wal", "thegn.db-shm"] {
        let source = if name == "thegn.db" {
            src_db.to_path_buf()
        } else {
            src_db.with_file_name(name)
        };
        if name != "thegn.db" && !source.exists() {
            continue;
        }
        let destination = dest_dir.join(name);
        std::fs::copy(&source, &destination)
            .unwrap_or_else(|error| panic!("copy {source:?} to {destination:?}: {error}"));
        copied.push(name.to_owned());
    }
    copied
}

fn remove_if_exists(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => panic!("remove {path:?}: {error}"),
    }
}

/// Positive control: proves a cold, closed-database copy restores prior rows.
#[test]
fn cold_copy_after_closing_the_database_restores_the_rows() {
    let state = IsolatedState::new();
    let live = state.state_home().join("thegn").join("thegn.db");
    let db = Db::open_at(&live).unwrap();
    db.put_my_work_cache("/repo/a", r#"[{"n":1}]"#).unwrap();
    drop(db);

    let backup_dir = state.root().join("backup");
    std::fs::create_dir_all(&backup_dir).unwrap();
    let copied = copy_state_files(&live, &backup_dir);

    let db = Db::open_at(&live).unwrap();
    db.put_my_work_cache("/repo/a", r#"[{"n":2}]"#).unwrap();
    db.put_my_work_cache("/repo/b", r#"[{"n":2}]"#).unwrap();
    drop(db);

    for name in ["thegn.db-wal", "thegn.db-shm"] {
        remove_if_exists(&live.with_file_name(name));
    }
    for name in copied {
        let source = backup_dir.join(&name);
        let destination = live.with_file_name(&name);
        std::fs::copy(&source, &destination)
            .unwrap_or_else(|error| panic!("restore {source:?} to {destination:?}: {error}"));
    }

    let restored = Db::open_at(&live).unwrap();
    assert_eq!(
        restored.get_my_work_cache("/repo/a").unwrap().unwrap().0,
        r#"[{"n":1}]"#
    );
    assert!(restored.get_my_work_cache("/repo/b").unwrap().is_none());
}

/// Positive control: a quiescent WAL-inclusive copy restores a committed row.
#[test]
fn copy_with_wal_while_a_handle_is_open_restores_the_committed_rows() {
    let state = IsolatedState::new();
    let live = state.state_home().join("thegn").join("thegn.db");
    let db = Db::open_at(&live).unwrap();
    db.put_my_work_cache("/repo/a", r#"[{"n":1}]"#).unwrap();

    let backup_dir = state.root().join("backup");
    std::fs::create_dir_all(&backup_dir).unwrap();
    let copied = copy_state_files(&live, &backup_dir);
    assert!(
        copied.iter().any(|name| name == "thegn.db-wal"),
        "an open WAL database copy must include its committed WAL"
    );
    drop(db);

    for name in ["thegn.db", "thegn.db-wal", "thegn.db-shm"] {
        remove_if_exists(&live.with_file_name(name));
    }
    for name in copied {
        let source = backup_dir.join(&name);
        let destination = live.with_file_name(&name);
        std::fs::copy(&source, &destination)
            .unwrap_or_else(|error| panic!("restore {source:?} to {destination:?}: {error}"));
    }

    let restored = Db::open_at(&live).unwrap();
    assert_eq!(
        restored.get_my_work_cache("/repo/a").unwrap().unwrap().0,
        r#"[{"n":1}]"#
    );
}

/// Positive control: a restored database stamped by a newer build is refused.
#[test]
fn a_restored_copy_stamped_newer_is_refused_by_an_older_build() {
    let state = IsolatedState::new();
    let live = state.state_home().join("thegn").join("thegn.db");
    let db = Db::open_at(&live).unwrap();
    drop(db);

    let restored_dir = state.root().join("restored");
    std::fs::create_dir_all(&restored_dir).unwrap();
    let copy = restored_dir.join("thegn.db");
    std::fs::copy(&live, &copy).unwrap();
    {
        let connection = Connection::open(&copy).unwrap();
        connection
            .execute_batch(&format!("PRAGMA user_version = {};", SCHEMA_VERSION + 1))
            .unwrap();
    }

    let error = match Db::open_at(&copy) {
        Ok(_) => panic!("a database stamped newer than this build must be refused"),
        Err(error) => error,
    };
    assert!(matches!(
        error.downcast_ref::<MigrationAccessError>(),
        Some(MigrationAccessError::NewerSchema { .. })
    ));
    assert!(error.to_string().contains("Refusing to run"));
}

/// Positive control: a stamped older backup migrates forward while preserving rows.
#[test]
fn an_older_backup_migrates_forward_on_open() {
    let state = IsolatedState::new();
    let live = state.state_home().join("thegn").join("thegn.db");
    let db = Db::open_at(&live).unwrap();
    db.put_my_work_cache("/repo/a", r#"[{"n":1}]"#).unwrap();
    drop(db);

    let restored_dir = state.root().join("restored");
    std::fs::create_dir_all(&restored_dir).unwrap();
    let copy = restored_dir.join("thegn.db");
    std::fs::copy(&live, &copy).unwrap();
    {
        let connection = Connection::open(&copy).unwrap();
        connection
            .execute_batch("PRAGMA user_version = 100;")
            .unwrap();
    }

    let upgraded = Db::open_at(&copy).unwrap();
    assert_eq!(
        upgraded.get_my_work_cache("/repo/a").unwrap().unwrap().0,
        r#"[{"n":1}]"#
    );
    drop(upgraded);

    {
        let connection = Connection::open(&copy).unwrap();
        assert_eq!(
            connection
                .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            SCHEMA_VERSION
        );
    }
}
