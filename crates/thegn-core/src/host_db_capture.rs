//! Logically read-only WAL capture, not a path-trust or launch-authority API.
//!
//! The host must first establish the selected path's supported namespace model.
//! This primitive independently opens that filename using SQLite's Unix VFS;
//! it does not consume a caller-pinned file descriptor. Normal SQLite locking
//! and WAL/SHM support-file activity are allowed. No migrations, global policy
//! installation, connection PRAGMA changes, or immutable URI shortcuts occur.
//! Call only off-loop. The busy timeout bounds lock waits, not total I/O time.

use std::{fmt, path::Path, time::Duration};

use rusqlite::{Connection, OpenFlags};

use crate::host_definition_snapshot::{HostDefinitionReadError, HostDefinitionsSnapshot};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostCaptureReadError {
    Open,
    Busy,
    NotReadOnly,
    Close,
    Definitions(HostDefinitionReadError),
}

impl fmt::Display for HostCaptureReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Open => "state database could not be opened for capture",
            Self::Busy => "state database capture is busy",
            Self::NotReadOnly => "state database capture is not read-only",
            Self::Close => "state database capture could not be closed",
            Self::Definitions(_) => "state database definitions could not be captured",
        })
    }
}

impl std::error::Error for HostCaptureReadError {}

fn open_error(error: rusqlite::Error) -> HostCaptureReadError {
    match error.sqlite_error_code() {
        Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked) => {
            HostCaptureReadError::Busy
        }
        _ => HostCaptureReadError::Open,
    }
}

/// Capture SQL data from an already host-selected path. The caller must check
/// path trust and platform support separately; this function grants neither.
/// Missing/unreadable files are errors, never an empty host map. The explicit
/// Unix VFS keeps this primitive unavailable on unsupported builds. No general
/// connection escapes to the caller, and no URI parameters are accepted.
pub fn capture_host_definitions_wal_at(
    path: &Path,
) -> Result<HostDefinitionsSnapshot, HostCaptureReadError> {
    if !path.is_absolute()
        || path.as_os_str().len() > 4096
        || path
            .as_os_str()
            .as_encoded_bytes()
            .iter()
            .any(|byte| byte.is_ascii_control() || matches!(*byte, 0 | b'?' | b'#'))
    {
        return Err(HostCaptureReadError::Open);
    }
    let conn = Connection::open_with_flags_and_vfs(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_PRIVATE_CACHE
            | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        "unix",
    )
    .map_err(open_error)?;
    conn.busy_timeout(Duration::from_millis(250))
        .map_err(open_error)?;
    if !conn.is_readonly("main").map_err(open_error)? {
        return Err(HostCaptureReadError::NotReadOnly);
    }
    let result = crate::host_db_snapshot::read(&conn).map_err(|error| match error {
        HostDefinitionReadError::Busy => HostCaptureReadError::Busy,
        other => HostCaptureReadError::Definitions(other),
    });
    // Close before returning even an unsuccessful read; no borrowed SQLite data
    // escapes. A close failure is itself a visible refusal, never a fallback.
    conn.close().map_err(|_| HostCaptureReadError::Close)?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unsupported_vfs() -> bool {
        // This is a VFS capability check, not a target-name inference. On a
        // build without Unix VFS these tests assert the explicit refusal path;
        // they do not claim to have exercised positive WAL capture there.
        let absent = unsafe { rusqlite::ffi::sqlite3_vfs_find(c"unix".as_ptr()).is_null() };
        if absent {
            let dir = tempfile::tempdir().unwrap();
            assert!(matches!(
                capture_host_definitions_wal_at(&dir.path().join("state.db")),
                Err(HostCaptureReadError::Open)
            ));
            dir.close().expect("owned unsupported-VFS fixture cleanup");
        }
        absent
    }

    fn database(path: &Path) -> Connection {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(&format!(
            "PRAGMA user_version={}; CREATE TABLE hosts (host_id TEXT PRIMARY KEY, name TEXT, config_json TEXT);",
            crate::db::SCHEMA_VERSION
        )).unwrap();
        conn
    }

    #[test]
    fn wal_capture_sees_uncheckpointed_rows_without_changing_logical_state() {
        if unsupported_vfs() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let writer = database(&path);
        writer.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; INSERT INTO hosts VALUES ('a','private-host','{}');").unwrap();
        writer.execute_batch("CREATE TABLE capture_canary (payload TEXT); INSERT INTO capture_canary VALUES ('expired-looking sentinel retained');").unwrap();
        assert!(dir.path().join("state.db-wal").is_file());
        let base_before = std::fs::read(&path).unwrap();
        let wal_before = std::fs::read(dir.path().join("state.db-wal")).unwrap();
        let captured = capture_host_definitions_wal_at(&path).unwrap();
        assert_eq!(captured.definitions()[0].0, "private-host");
        assert_eq!(
            writer
                .query_row("SELECT count(*) FROM hosts", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            writer
                .query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            crate::db::SCHEMA_VERSION
        );
        assert_eq!(
            writer
                .query_row("PRAGMA journal_mode", [], |r| r.get::<_, String>(0))
                .unwrap(),
            "wal"
        );
        assert_eq!(
            writer
                .query_row("SELECT payload FROM capture_canary", [], |r| r
                    .get::<_, String>(0))
                .unwrap(),
            "expired-looking sentinel retained"
        );
        assert_eq!(std::fs::read(&path).unwrap(), base_before);
        assert_eq!(
            std::fs::read(dir.path().join("state.db-wal")).unwrap(),
            wal_before
        );
        let mut files = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        files.sort();
        assert_eq!(
            files,
            ["state.db", "state.db-shm", "state.db-wal"].map(std::ffi::OsString::from)
        );
        // Normal SHM locks/bookkeeping are permitted; SQL capture must not
        // checkpoint or rewrite the base/WAL or prune unrelated logical data.
        drop(writer);
        dir.close().expect("owned WAL capture fixture cleanup");
    }

    #[test]
    fn missing_and_invalid_sources_are_not_absent_or_fabricated_snapshots() {
        if unsupported_vfs() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.db");
        assert!(matches!(
            capture_host_definitions_wal_at(&path),
            Err(HostCaptureReadError::Open)
        ));
        assert!(!path.exists());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
        assert!(matches!(
            capture_host_definitions_wal_at(Path::new(":memory:")),
            Err(HostCaptureReadError::Open)
        ));
        std::fs::write(&path, []).unwrap();
        assert!(matches!(
            capture_host_definitions_wal_at(&path),
            Err(HostCaptureReadError::Definitions(
                HostDefinitionReadError::IncompatibleSchema { observed: 0, .. }
            ))
        ));
        std::fs::write(&path, b"not a sqlite database").unwrap();
        assert!(matches!(
            capture_host_definitions_wal_at(&path),
            Err(HostCaptureReadError::Definitions(
                HostDefinitionReadError::Unavailable
            ))
        ));
        assert_eq!(std::fs::read(&path).unwrap(), b"not a sqlite database");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
        dir.close().expect("owned invalid-source fixture cleanup");
    }

    #[test]
    fn malformed_and_version_errors_remain_typed() {
        if unsupported_vfs() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let writer = database(&path);
        writer
            .execute("INSERT INTO hosts VALUES ('a','a','{')", [])
            .unwrap();
        assert!(matches!(
            capture_host_definitions_wal_at(&path),
            Err(HostCaptureReadError::Definitions(
                HostDefinitionReadError::InvalidJson
            ))
        ));
        writer.execute("DELETE FROM hosts", []).unwrap();
        assert!(
            capture_host_definitions_wal_at(&path)
                .unwrap()
                .definitions()
                .is_empty()
        );
        for version in [crate::db::SCHEMA_VERSION - 1, 999] {
            writer
                .execute_batch(&format!("PRAGMA user_version={version}"))
                .unwrap();
            assert!(
                matches!(capture_host_definitions_wal_at(&path), Err(HostCaptureReadError::Definitions(HostDefinitionReadError::IncompatibleSchema { observed, .. })) if observed == version)
            );
        }
        writer
            .execute_batch(&format!(
                "PRAGMA user_version={}; DROP TABLE hosts",
                crate::db::SCHEMA_VERSION
            ))
            .unwrap();
        assert!(matches!(
            capture_host_definitions_wal_at(&path),
            Err(HostCaptureReadError::Definitions(
                HostDefinitionReadError::InvalidSchema
            ))
        ));
        drop(writer);
        dir.close().expect("owned schema fixture cleanup");
    }

    #[test]
    fn query_like_literal_filenames_are_explicitly_unsupported() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["state.db?immutable=1", "state.db#fragment"] {
            let path = dir.path().join(name);
            assert!(matches!(
                capture_host_definitions_wal_at(&path),
                Err(HostCaptureReadError::Open)
            ));
            assert!(!path.exists());
        }
        dir.close().expect("owned URI refusal fixture cleanup");
    }

    #[test]
    fn busy_and_diagnostics_do_not_echo_sql_or_paths() {
        if unsupported_vfs() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret-name.db");
        let writer = database(&path);
        writer.execute_batch("BEGIN EXCLUSIVE").unwrap();
        let started = std::time::Instant::now();
        let result = capture_host_definitions_wal_at(&path);
        let elapsed = started.elapsed();
        writer.execute_batch("ROLLBACK").unwrap();
        let error = result.unwrap_err();
        assert_eq!(error, HostCaptureReadError::Busy);
        assert!(!format!("{error:?}: {error}").contains("secret-name"));
        // A generous owned lock-fixture bound, not a total filesystem I/O SLA.
        assert!(
            elapsed < Duration::from_secs(3),
            "SQLite busy handler did not return within fixture bound: {elapsed:?}"
        );
        assert!(
            capture_host_definitions_wal_at(&path)
                .unwrap()
                .definitions()
                .is_empty()
        );
        drop(writer);
        dir.close().expect("owned busy fixture cleanup");
    }
}
