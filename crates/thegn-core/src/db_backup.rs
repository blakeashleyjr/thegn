//! Automatic backup of the state database before a forward schema migration.
//!
//! The migrator is forward-only: once `user_version` is stamped higher, an older
//! build refuses the file. A timestamped `VACUUM INTO` copy next to the database
//! makes a bad migration (or a rollback to the previous release) recoverable.
//! The newest [`KEEP`] copies are retained. Restore steps live in
//! `docs/help/upgrading-and-uninstalling.md`.

use anyhow::{Context, Result};
use rusqlite::Connection;
use std::path::{Path, PathBuf};

/// How many pre-migration backups are retained next to the database.
pub const KEEP: usize = 3;

const PREFIX: &str = "thegn.db.pre-migrate-";
const SUFFIX: &str = ".bak";

/// `thegn.db.pre-migrate-<UTC stamp>-v<from>.bak`. The stamp is fixed-width
/// (`%Y%m%dT%H%M%SZ`), so names sort chronologically as plain strings.
pub fn backup_file_name(stamp: &str, from_version: i64) -> String {
    format!("{PREFIX}{stamp}-v{from_version}{SUFFIX}")
}

/// Whether `name` is one of this module's backup files.
pub fn is_backup_name(name: &str) -> bool {
    name.starts_with(PREFIX) && name.ends_with(SUFFIX)
}

/// The backup names to delete so only the newest `keep` remain. Names that are
/// not backups are never returned.
pub fn rotation_victims(names: &[String], keep: usize) -> Vec<String> {
    let mut backups: Vec<&String> = names.iter().filter(|n| is_backup_name(n)).collect();
    backups.sort();
    let excess = backups.len().saturating_sub(keep);
    backups.into_iter().take(excess).cloned().collect()
}

/// Delete all but the newest `keep` backups in `dir`. Best-effort per file.
pub fn rotate(dir: &Path, keep: usize) -> Result<Vec<PathBuf>> {
    let names: Vec<String> = std::fs::read_dir(dir)
        .with_context(|| format!("list {}", dir.display()))?
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect();
    let mut removed = Vec::new();
    for name in rotation_victims(&names, keep) {
        let path = dir.join(&name);
        match std::fs::remove_file(&path) {
            Ok(()) => removed.push(path),
            Err(error) => {
                tracing::debug!(%error, path = %path.display(), "could not remove old state backup")
            }
        }
    }
    Ok(removed)
}

/// Write a verified copy of the database behind `conn` (schema `from_version`)
/// next to `db_file`, then rotate. Returns the backup path. An existing backup
/// with the same stamp and version is reused rather than overwritten.
pub fn backup_before_migration(
    conn: &Connection,
    db_file: &Path,
    from_version: i64,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<PathBuf> {
    let dir = db_file
        .parent()
        .ok_or_else(|| anyhow::anyhow!("database {} has no parent directory", db_file.display()))?;
    let stamp = now.format("%Y%m%dT%H%M%SZ").to_string();
    let dest = dir.join(backup_file_name(&stamp, from_version));
    if dest.exists() {
        return Ok(dest);
    }
    let dest_text = dest
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("backup path {} is not UTF-8", dest.display()))?;
    conn.execute("VACUUM INTO ?1", [dest_text])
        .with_context(|| format!("write backup {}", dest.display()))?;
    if let Err(error) = verify(&dest, from_version) {
        if let Err(error) = std::fs::remove_file(&dest) {
            // A failed copy must not count toward retention; removal is best-effort.
            tracing::debug!(%error, path = %dest.display(), "could not remove a failed database backup");
        }
        return Err(error);
    }
    if let Err(error) = crate::fsperm::restrict_to_owner(&dest) {
        // Hardening only: a failed chmod must never block the migration.
        tracing::debug!(%error, "could not restrict the database backup to its owner");
    }
    rotate(dir, KEEP)?;
    Ok(dest)
}

fn verify(dest: &Path, expected_version: i64) -> Result<()> {
    let copy = Connection::open_with_flags(dest, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("open backup {}", dest.display()))?;
    let check: String = copy.query_row("PRAGMA quick_check", [], |row| row.get(0))?;
    if check != "ok" {
        anyhow::bail!("backup {} failed quick_check: {check}", dest.display());
    }
    let actual: i64 = copy.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if actual != expected_version {
        anyhow::bail!(
            "backup {} has user_version {actual}, expected {expected_version}",
            dest.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn names(stamps: &[&str]) -> Vec<String> {
        stamps.iter().map(|s| backup_file_name(s, 80)).collect()
    }

    #[test]
    fn rotation_keeps_the_newest_and_ignores_other_files() {
        let mut all = names(&[
            "20261101T000000Z",
            "20261103T000000Z",
            "20261102T000000Z",
            "20261105T000000Z",
            "20261104T000000Z",
        ]);
        all.push("thegn.db".into());
        all.push("thegn.db-wal".into());
        let victims = rotation_victims(&all, 3);
        assert_eq!(victims, names(&["20261101T000000Z", "20261102T000000Z"]));
        assert!(rotation_victims(&names(&["20261101T000000Z"]), 3).is_empty());
    }

    #[test]
    fn backup_is_verified_named_and_rotated_to_three() {
        let dir = tempfile::tempdir().unwrap();
        let db_file = dir.path().join("thegn.db");
        let conn = Connection::open(&db_file).unwrap();
        conn.execute_batch(
            "CREATE TABLE t(x); INSERT INTO t VALUES (7); PRAGMA user_version = 41;",
        )
        .unwrap();
        let mut last = PathBuf::new();
        for day in 1..=5u32 {
            let now = chrono::Utc
                .with_ymd_and_hms(2026, 11, day, 12, 0, 0)
                .unwrap();
            last = backup_before_migration(&conn, &db_file, 41, now).unwrap();
        }
        let mut present: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|n| is_backup_name(n))
            .collect();
        present.sort();
        assert_eq!(present.len(), KEEP);
        assert_eq!(
            present.last().unwrap(),
            &backup_file_name("20261105T120000Z", 41)
        );
        let copy = Connection::open(&last).unwrap();
        let (x, version): (i64, i64) = (
            copy.query_row("SELECT x FROM t", [], |r| r.get(0)).unwrap(),
            copy.query_row("PRAGMA user_version", [], |r| r.get(0))
                .unwrap(),
        );
        assert_eq!((x, version), (7, 41));
    }

    #[test]
    fn same_second_backup_is_reused_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let db_file = dir.path().join("thegn.db");
        let conn = Connection::open(&db_file).unwrap();
        conn.execute_batch("CREATE TABLE t(x); PRAGMA user_version = 3;")
            .unwrap();
        let now = chrono::Utc.with_ymd_and_hms(2026, 11, 1, 0, 0, 0).unwrap();
        let first = backup_before_migration(&conn, &db_file, 3, now).unwrap();
        let second = backup_before_migration(&conn, &db_file, 3, now).unwrap();
        assert_eq!(first, second);
    }
}
