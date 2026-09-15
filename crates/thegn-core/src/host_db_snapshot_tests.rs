use super::*;
use crate::host_config::{HostConfig, HostReach};
use crate::host_definition_snapshot::MAX_HOST_SNAPSHOT_BYTES;
use crate::store::HostStore;

fn schema_fixture(conn: &Connection) {
    conn.execute_batch(
        "CREATE TABLE hosts(host_id TEXT PRIMARY KEY, name TEXT, config_json TEXT);",
    )
    .unwrap();
    conn.pragma_update(None, "user_version", crate::db::SCHEMA_VERSION)
        .unwrap();
}

fn fixture() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    schema_fixture(&conn);
    conn
}

fn put(conn: &Connection, id: &str, name: &str, json: Option<&str>) {
    conn.execute(
        "INSERT INTO hosts(host_id,name,config_json) VALUES(?1,?2,?3)",
        rusqlite::params![id, name, json],
    )
    .unwrap();
}

#[test]
fn current_empty_and_sorted_definitions_use_object_safe_store_seam() {
    let db = crate::db::Db::open_memory().unwrap();
    let store: &dyn HostStore = &db;
    assert!(store.host_defs_checked().unwrap().definitions().is_empty());
    for name in ["z", "a"] {
        store
            .put_host_def(
                name,
                &HostConfig {
                    reach: HostReach::Local,
                    ..Default::default()
                },
                1,
            )
            .unwrap();
    }
    let changes = db.conn().total_changes();
    let snapshot = store.host_defs_checked().unwrap();
    assert_eq!(snapshot.observed_schema(), crate::db::SCHEMA_VERSION);
    assert_eq!(
        snapshot
            .definitions()
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>(),
        ["a", "z"]
    );
    assert!(
        snapshot
            .definitions()
            .iter()
            .all(|(_, def)| def.reach == HostReach::Local)
    );
    assert!(db.conn().is_autocommit());
    assert_eq!(db.conn().total_changes(), changes);
}

#[test]
fn null_inventory_is_not_a_definition_but_empty_named_definition_is_invalid() {
    let conn = fixture();
    put(&conn, "inventory", "", None);
    assert!(read(&conn).unwrap().definitions().is_empty());
    put(&conn, "invalid", "", Some("{}"));
    assert_eq!(read(&conn).unwrap_err(), Error::InvalidName);
}

#[test]
fn duplicate_names_refuse_even_identical_definitions() {
    let conn = fixture();
    put(&conn, "one", "same", Some("{}"));
    put(&conn, "two", "same", Some("{}"));
    assert_eq!(read(&conn).unwrap_err(), Error::DuplicateName);
}

#[test]
fn malformed_rows_are_not_laundered_through_legacy_host_defs() {
    let db = crate::db::Db::open_memory().unwrap();
    db.conn()
        .execute(
            "INSERT INTO hosts(host_id,name,config_json) VALUES('one','broken','{')",
            [],
        )
        .unwrap();
    // Legacy display behavior remains unchanged; strict capture never calls it.
    assert!(db.host_defs().unwrap().is_empty());
    assert_eq!(db.host_defs_checked().unwrap_err(), Error::InvalidJson);
    db.conn()
        .execute("UPDATE hosts SET config_json=?1", [r#"{"reach":"typo"}"#])
        .unwrap();
    assert_eq!(db.host_defs().unwrap()[0].1.reach, HostReach::Ssh);
    assert_eq!(
        db.host_defs_checked().unwrap_err(),
        Error::InvalidDefinition
    );
    db.conn()
        .execute(
            "UPDATE hosts SET config_json=?1",
            [r#"{"unknown":"private-secret"}"#],
        )
        .unwrap();
    assert_eq!(
        db.host_defs_checked().unwrap_err(),
        Error::InvalidDefinition
    );
}

#[test]
fn malformed_persisted_definitions_refuse_even_when_declarative_host_shadows_name() {
    for (raw, expected) in [
        ("{", Error::InvalidJson),
        (r#"{"reach":"typo"}"#, Error::InvalidDefinition),
        (
            r#"{"reach":"ssh","unknown":"private-shadowed-value"}"#,
            Error::InvalidDefinition,
        ),
    ] {
        let declarative: crate::config::Config =
            toml::from_str("[host.shadowed]\nreach = \"local\"\n").unwrap();
        let before = serde_json::to_value(&declarative).unwrap();
        let db = crate::db::Db::open_memory().unwrap();
        let store: &dyn HostStore = &db;
        store
            .put_host_def(
                "shadowed",
                &HostConfig {
                    reach: HostReach::Ssh,
                    ssh: crate::config::EnvSshConfig {
                        host: "unused.invalid".into(),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                1,
            )
            .unwrap();
        // Positive source control: the actual object-safe seam captures the
        // persisted SSH definition even though declarative config wins its name.
        let valid = store.host_defs_checked().unwrap();
        assert_eq!(valid.definitions().len(), 1);
        assert_eq!(valid.definitions()[0].0, "shadowed");
        assert_eq!(valid.definitions()[0].1.reach, HostReach::Ssh);
        assert_eq!(declarative.host["shadowed"].reach, HostReach::Local);
        drop(valid);
        assert_eq!(
            db.conn()
                .execute(
                    "UPDATE hosts SET config_json=?1 WHERE name='shadowed'",
                    [raw],
                )
                .unwrap(),
            1
        );
        let changes = db.conn().total_changes();
        // Capture must inspect every persisted definition, not a post-shadowing
        // subset. This is source refusal, not a simulated launch-admission gate.
        assert_eq!(store.host_defs_checked().unwrap_err(), expected);
        assert_eq!(serde_json::to_value(&declarative).unwrap(), before);
        assert!(db.conn().is_autocommit());
        assert_eq!(db.conn().total_changes(), changes);
        assert_eq!(
            db.conn()
                .query_row(
                    "SELECT config_json FROM hosts WHERE name='shadowed'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            raw
        );
    }
}

#[test]
fn sql_types_utf8_and_control_names_fail_without_contents_in_error() {
    for (name, json, expected) in [
        ("x'61'", "'{}'", Error::InvalidColumnType),
        ("'a'", "x'7b7d'", Error::InvalidColumnType),
        ("CAST(x'ff' AS TEXT)", "'{}'", Error::InvalidUtf8),
        ("'a'", "CAST(x'ff' AS TEXT)", Error::InvalidUtf8),
        ("'bad' || char(10)", "'{}'", Error::InvalidName),
        ("NULL", "'{}'", Error::InvalidColumnType),
    ] {
        let conn = fixture();
        conn.execute_batch(&format!("INSERT INTO hosts VALUES('one',{name},{json});"))
            .unwrap();
        let error = read(&conn).unwrap_err();
        assert_eq!(error, expected);
        assert!(error.to_string().len() < 100);
        assert!(!error.to_string().contains('\n'));
        assert!(conn.is_autocommit());
    }
}

#[test]
fn versions_are_read_fallibly_inside_owned_transaction() {
    let conn = fixture();
    for version in [
        0,
        30,
        crate::db::SCHEMA_VERSION - 1,
        crate::db::SCHEMA_VERSION + 1,
    ] {
        conn.pragma_update(None, "user_version", version).unwrap();
        assert_eq!(
            read(&conn).unwrap_err(),
            Error::IncompatibleSchema {
                observed: version,
                supported: crate::db::SCHEMA_VERSION,
            }
        );
        assert!(conn.is_autocommit());
        assert_eq!(
            conn.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            version
        );
    }
}

#[test]
fn corrupt_database_is_not_schema_zero_or_empty_snapshot() {
    use std::io::Write;
    let mut file = tempfile::NamedTempFile::new().unwrap();
    file.write_all(&[0x55; 4096]).unwrap();
    file.flush().unwrap();
    let conn = Connection::open(file.path()).unwrap();
    assert_eq!(read(&conn).unwrap_err(), Error::Unavailable);
    assert!(conn.is_autocommit());
}

#[test]
fn ordinary_required_schema_is_checked_without_reading_schema_sql() {
    for ddl in [
        "CREATE TABLE other(x)",
        "CREATE VIEW hosts AS SELECT 'id' AS host_id, 'a' AS name, '{}' AS config_json",
        "CREATE VIRTUAL TABLE hosts USING fts5(host_id,name,config_json)",
        "CREATE TABLE hosts(host_id TEXT PRIMARY KEY, name TEXT)",
        "CREATE TABLE hosts(host_id TEXT PRIMARY KEY, name BLOB, config_json TEXT)",
        "CREATE TABLE hosts(host_id TEXT, name TEXT, config_json TEXT)",
        "CREATE TABLE hosts(host_id TEXT PRIMARY KEY, name TEXT, config_json TEXT, computed TEXT GENERATED ALWAYS AS (name) VIRTUAL)",
    ] {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(ddl).unwrap();
        conn.pragma_update(None, "user_version", crate::db::SCHEMA_VERSION)
            .unwrap();
        assert_eq!(read(&conn).unwrap_err(), Error::InvalidSchema, "{ddl}");
    }
}

#[test]
fn temporary_table_cannot_shadow_verified_main_table() {
    let conn = fixture();
    put(&conn, "id", "original", Some(r#"{"reach":"local"}"#));
    conn.execute_batch("CREATE TEMP TABLE hosts(host_id,name,config_json); INSERT INTO temp.hosts VALUES('bad','shadow','{');").unwrap();
    let snapshot = read(&conn).unwrap();
    assert_eq!(snapshot.definitions()[0].0, "original");
}

#[test]
fn schema_strings_and_column_count_are_bounded_before_owned_copies() {
    let conn = fixture();
    conn.execute_batch(&format!(
        "ALTER TABLE hosts ADD COLUMN \"{}\" TEXT",
        "x".repeat(MAX_HOST_NAME_BYTES + 1)
    ))
    .unwrap();
    assert_eq!(read(&conn).unwrap_err(), Error::Bounds);
    let conn = fixture();
    conn.execute_batch(&format!(
        "ALTER TABLE hosts ADD COLUMN extra {}",
        "X".repeat(65)
    ))
    .unwrap();
    assert_eq!(read(&conn).unwrap_err(), Error::Bounds);
    let conn = fixture();
    for n in 3..MAX_HOST_SCHEMA_COLUMNS {
        conn.execute_batch(&format!("ALTER TABLE hosts ADD COLUMN c{n} TEXT"))
            .unwrap();
    }
    assert!(read(&conn).unwrap().definitions().is_empty());
    conn.execute_batch("ALTER TABLE hosts ADD COLUMN too_many TEXT")
        .unwrap();
    assert_eq!(read(&conn).unwrap_err(), Error::InvalidSchema);
}

#[test]
fn active_transaction_is_not_committed_or_rolled_back_by_snapshot() {
    let conn = fixture();
    conn.execute_batch("BEGIN; INSERT INTO hosts VALUES('one','a','{}');")
        .unwrap();
    assert_eq!(read(&conn).unwrap_err(), Error::TransactionActive);
    assert!(!conn.is_autocommit());
    assert_eq!(
        conn.query_row("SELECT count(*) FROM hosts", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        1
    );
    conn.execute_batch("ROLLBACK").unwrap();
    assert!(read(&conn).unwrap().definitions().is_empty());
}

#[test]
fn capture_failure_rolls_back_only_its_owned_transaction_without_policy_changes() {
    let conn = fixture();
    conn.busy_timeout(std::time::Duration::from_millis(37))
        .unwrap();
    let policy = || {
        (
            conn.query_row("PRAGMA busy_timeout", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            conn.query_row("PRAGMA query_only", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            conn.query_row("PRAGMA journal_mode", [], |row| row.get::<_, String>(0))
                .unwrap(),
        )
    };
    let before = policy();
    let changes = conn.total_changes();
    assert_eq!(
        read_with_snapshot_hook(&conn, || Err(Error::Unavailable)).unwrap_err(),
        Error::Unavailable
    );
    assert!(conn.is_autocommit());
    assert_eq!(policy(), before);
    assert_eq!(conn.total_changes(), changes);
    assert!(read(&conn).is_ok());
    assert_eq!(policy(), before);
}

#[test]
fn definition_count_and_sparse_inventory_have_independent_exact_limits() {
    let conn = fixture();
    {
        let tx = conn.unchecked_transaction().unwrap();
        for n in 0..MAX_HOST_DEFINITIONS {
            put(&tx, &n.to_string(), &n.to_string(), Some("{}"));
        }
        tx.commit().unwrap();
    }
    assert_eq!(
        read(&conn).unwrap().definitions().len(),
        MAX_HOST_DEFINITIONS
    );
    put(&conn, "overflow", "overflow", Some("{}"));
    assert_eq!(read(&conn).unwrap_err(), Error::TooManyDefinitions);
    let conn = fixture();
    {
        let tx = conn.unchecked_transaction().unwrap();
        for n in 0..MAX_HOST_ROWS {
            put(&tx, &n.to_string(), "", None);
        }
        tx.commit().unwrap();
    }
    assert!(read(&conn).unwrap().definitions().is_empty());
    put(&conn, "overflow", "", None);
    assert_eq!(read(&conn).unwrap_err(), Error::TooManyRows);
}

#[test]
fn byte_limits_check_metadata_and_aggregate_before_copy() {
    let conn = fixture();
    let exact = format!("{{}}{}", " ".repeat(MAX_HOST_SNAPSHOT_BYTES - 3));
    put(&conn, "one", "a", Some(&exact));
    assert_eq!(read(&conn).unwrap().definitions().len(), 1);
    put(&conn, "two", "b", Some("{}"));
    assert_eq!(read(&conn).unwrap_err(), Error::Bounds);
    conn.execute("DELETE FROM hosts", []).unwrap();
    put(
        &conn,
        "large",
        "a",
        Some(&" ".repeat(MAX_HOST_DEFINITION_BYTES + 1)),
    );
    assert_eq!(read(&conn).unwrap_err(), Error::Bounds);
    conn.execute("DELETE FROM hosts", []).unwrap();
    put(&conn, "name", &"n".repeat(MAX_HOST_NAME_BYTES), Some("{}"));
    assert!(read(&conn).is_ok());
    conn.execute(
        "UPDATE hosts SET name=?1",
        [&"n".repeat(MAX_HOST_NAME_BYTES + 1)],
    )
    .unwrap();
    assert_eq!(read(&conn).unwrap_err(), Error::Bounds);
}

#[test]
fn wal_writer_cannot_mix_new_version_schema_or_data_into_existing_snapshot() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("private.db");
    let reader = Connection::open(&path).unwrap();
    schema_fixture(&reader);
    reader.pragma_update(None, "journal_mode", "WAL").unwrap();
    put(&reader, "one", "before", Some(r#"{"reach":"local"}"#));
    let writer = Connection::open(&path).unwrap();
    let before_changes = reader.total_changes();
    let snapshot = read_with_snapshot_hook(&reader, || {
        // Deterministic interleaving: the first read already captured version
        // and columns. This second connection commits while it remains open.
        writer.execute_batch("BEGIN; UPDATE hosts SET name='after', config_json='{\"reach\":\"ssh\"}'; ALTER TABLE hosts ADD COLUMN later TEXT;").unwrap();
        writer.pragma_update(None, "user_version", crate::db::SCHEMA_VERSION + 1).unwrap();
        writer.execute_batch("COMMIT").unwrap();
        Ok(())
    }).unwrap();
    assert_eq!(snapshot.observed_schema(), crate::db::SCHEMA_VERSION);
    assert_eq!(snapshot.definitions()[0].0, "before");
    assert_eq!(snapshot.definitions()[0].1.reach, HostReach::Local);
    assert_eq!(reader.total_changes(), before_changes);
    assert_eq!(
        read(&reader).unwrap_err(),
        Error::IncompatibleSchema {
            observed: crate::db::SCHEMA_VERSION + 1,
            supported: crate::db::SCHEMA_VERSION,
        }
    );
    writer
        .pragma_update(None, "user_version", crate::db::SCHEMA_VERSION)
        .unwrap();
    assert_eq!(read(&reader).unwrap().definitions()[0].0, "after");
}

#[test]
fn existing_busy_policy_is_respected_without_claiming_query_deadline() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("private.db");
    let reader = Connection::open(&path).unwrap();
    schema_fixture(&reader);
    reader.busy_timeout(std::time::Duration::ZERO).unwrap();
    let writer = Connection::open(&path).unwrap();
    writer.execute_batch("BEGIN EXCLUSIVE").unwrap();
    assert_eq!(read(&reader).unwrap_err(), Error::Busy);
    assert!(reader.is_autocommit());
    writer.execute_batch("ROLLBACK").unwrap();
    assert!(read(&reader).is_ok());
}
