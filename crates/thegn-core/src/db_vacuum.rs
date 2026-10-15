//! SQLite space inspection, bounded incremental reclamation, and conversion.

use crate::db::Db;
use anyhow::{Result, anyhow};
use rusqlite::{Error as SqlError, ErrorCode, params};
use serde::Serialize;
use std::time::Duration;

pub const NOTIFICATION_READ_RETENTION_SECS: i64 = 30 * 24 * 60 * 60;
pub const INCREMENTAL_PAGES_PER_ROUND: u32 = 2048;
pub const AUTO_CONVERT_MAX_LIVE_BYTES: u64 = 64 * 1024 * 1024;
const MIN_RECLAIM_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoVacuum {
    None,
    Full,
    Incremental,
}

impl AutoVacuum {
    fn from_pragma(value: i64) -> Result<Self> {
        match value {
            0 => Ok(Self::None),
            1 => Ok(Self::Full),
            2 => Ok(Self::Incremental),
            other => Err(anyhow!("SQLite returned unknown auto_vacuum mode {other}")),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct DbSpace {
    pub page_size: u64,
    pub page_count: u64,
    pub freelist_count: u64,
    pub auto_vacuum: AutoVacuum,
}

impl DbSpace {
    pub fn live_bytes(&self) -> u64 {
        self.page_count
            .saturating_sub(self.freelist_count)
            .saturating_mul(self.page_size)
    }

    pub fn free_bytes(&self) -> u64 {
        self.freelist_count.saturating_mul(self.page_size)
    }

    pub fn file_bytes(&self) -> u64 {
        self.page_count.saturating_mul(self.page_size)
    }
}

#[derive(Debug)]
pub enum VacuumError {
    Busy,
    NotIncremental,
}

impl std::fmt::Display for VacuumError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy => formatter.write_str("database is busy"),
            Self::NotIncremental => {
                formatter.write_str("incremental vacuum requires auto_vacuum=INCREMENTAL")
            }
        }
    }
}

impl std::error::Error for VacuumError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConvertOutcome {
    pub before: DbSpace,
    pub after: DbSpace,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MaintenanceStep {
    Nothing,
    Incremental { pages: u32 },
    Convert,
    TooLargeForAuto { live_bytes: u64, free_bytes: u64 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MaintenanceReport {
    pub pruned_notifications: usize,
    pub reclaimed_pages: u64,
    pub conversion: Option<ConvertOutcome>,
}

impl Db {
    /// Read the current SQLite page and auto-vacuum counters without writing.
    pub fn space(&self) -> Result<DbSpace> {
        let conn = self.conn();
        let page_size = conn.query_row("PRAGMA page_size", [], |row| row.get::<_, i64>(0))?;
        let page_count = conn.query_row("PRAGMA page_count", [], |row| row.get::<_, i64>(0))?;
        let freelist_count =
            conn.query_row("PRAGMA freelist_count", [], |row| row.get::<_, i64>(0))?;
        let auto_vacuum = conn.query_row("PRAGMA auto_vacuum", [], |row| row.get::<_, i64>(0))?;
        Ok(DbSpace {
            page_size: page_size as u64,
            page_count: page_count as u64,
            freelist_count: freelist_count as u64,
            auto_vacuum: AutoVacuum::from_pragma(auto_vacuum)?,
        })
    }

    /// Free at most `max_pages` from the freelist in an incremental-mode DB.
    pub fn incremental_vacuum(&self, max_pages: u32) -> Result<u64> {
        if self.space()?.auto_vacuum != AutoVacuum::Incremental {
            return Err(VacuumError::NotIncremental.into());
        }
        let before = self.space()?.freelist_count;
        if max_pages > 0 {
            self.conn()
                .execute_batch(&format!("PRAGMA incremental_vacuum({max_pages});"))
                .map_err(map_busy)?;
        }
        let after = self.space()?.freelist_count;
        Ok(before.saturating_sub(after))
    }

    /// Convert a legacy DB. The normal five-second timeout is restored on all paths.
    pub fn convert_to_incremental(&self, busy: Duration) -> Result<ConvertOutcome> {
        let before = self.space()?;
        let conversion = match self.conn().busy_timeout(busy) {
            Err(error) => Err(map_busy(error)),
            Ok(()) => (|| -> Result<DbSpace> {
                self.conn()
                    .execute_batch("PRAGMA auto_vacuum=INCREMENTAL; VACUUM;")
                    .map_err(map_busy)?;
                if let Err(error) = checkpoint_wal(self.conn()) {
                    tracing::debug!(%error, "could not truncate the state database WAL after VACUUM");
                }
                let after = self.space()?;
                ensure_incremental(after)?;
                Ok(after)
            })(),
        };
        restore_busy_timeout(self.conn());
        conversion.map(|after| ConvertOutcome { before, after })
    }

    /// Count expired read notifications before a writable open's startup prune.
    pub fn expired_read_notification_count(&self, now_secs: i64) -> Result<usize> {
        let cutoff = now_secs.saturating_sub(NOTIFICATION_READ_RETENTION_SECS);
        let count = self.conn().query_row(
            "SELECT count(*) FROM notifications WHERE read=1 AND created_at_ms < ?1",
            params![cutoff],
            |row| row.get::<_, i64>(0),
        )?;
        Ok(usize::try_from(count)?)
    }

    /// Prune expired read notifications first, then perform the selected step.
    pub fn maintain(&self, now_secs: i64, step: MaintenanceStep) -> Result<MaintenanceReport> {
        let cutoff = now_secs.saturating_sub(NOTIFICATION_READ_RETENTION_SECS);
        let pruned_notifications = self
            .conn()
            .execute(
                "DELETE FROM notifications WHERE read=1 AND created_at_ms < ?1",
                params![cutoff],
            )
            .map_err(map_busy)?;
        let mut report = MaintenanceReport {
            pruned_notifications,
            reclaimed_pages: 0,
            conversion: None,
        };
        match step {
            MaintenanceStep::Nothing | MaintenanceStep::TooLargeForAuto { .. } => {}
            MaintenanceStep::Incremental { pages } => {
                report.reclaimed_pages = self.incremental_vacuum(pages)?;
            }
            MaintenanceStep::Convert => {
                report.conversion = Some(self.convert_to_incremental(Duration::from_secs(5))?);
            }
        }
        Ok(report)
    }
}

fn restore_busy_timeout(conn: &rusqlite::Connection) {
    if let Err(error) = conn.busy_timeout(Duration::from_millis(5000)) {
        tracing::debug!(%error, "could not restore the normal database busy timeout");
    }
}

fn checkpoint_wal(conn: &rusqlite::Connection) -> Result<()> {
    let busy = conn
        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
            row.get::<_, i64>(0)
        })
        .map_err(anyhow::Error::from)?;
    if busy != 0 {
        return Err(anyhow!(
            "WAL checkpoint could not acquire the database lock"
        ));
    }
    Ok(())
}

fn ensure_incremental(after: DbSpace) -> Result<()> {
    if after.auto_vacuum != AutoVacuum::Incremental {
        return Err(anyhow!(
            "database conversion did not enable incremental auto-vacuum"
        ));
    }
    Ok(())
}

fn map_busy(error: SqlError) -> anyhow::Error {
    match error {
        SqlError::SqliteFailure(code, _)
            if matches!(
                code.code,
                ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked
            ) =>
        {
            VacuumError::Busy.into()
        }
        error => error.into(),
    }
}

/// Decide one bounded maintenance step without touching SQLite.
pub fn plan_maintenance(space: &DbSpace, auto_attempts_left: u32) -> MaintenanceStep {
    let threshold = MIN_RECLAIM_BYTES.max(space.file_bytes().div_ceil(10));
    if space.auto_vacuum == AutoVacuum::Incremental && space.free_bytes() >= threshold {
        return MaintenanceStep::Incremental {
            pages: u32::try_from(space.freelist_count)
                .unwrap_or(u32::MAX)
                .min(INCREMENTAL_PAGES_PER_ROUND),
        };
    }
    if space.auto_vacuum == AutoVacuum::Incremental {
        return MaintenanceStep::Nothing;
    }
    if auto_attempts_left == 0 {
        return MaintenanceStep::Nothing;
    }
    if space.live_bytes() <= AUTO_CONVERT_MAX_LIVE_BYTES {
        MaintenanceStep::Convert
    } else {
        MaintenanceStep::TooLargeForAuto {
            live_bytes: space.live_bytes(),
            free_bytes: space.free_bytes(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn temp_db() -> (tempfile::TempDir, Db) {
        let dir = tempfile::Builder::new().tempdir().unwrap();
        let db = Db::open_at(&dir.path().join("state.db")).unwrap();
        (dir, db)
    }

    fn full_db() -> (tempfile::TempDir, Db) {
        let dir = tempfile::Builder::new().tempdir().unwrap();
        let path = dir.path().join("state.db");
        drop(Db::open_at(&path).unwrap());
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("PRAGMA auto_vacuum=NONE; VACUUM; CREATE TABLE fixture(data BLOB);")
            .unwrap();
        drop(conn);
        let db = Db::open_at(&path).unwrap();
        (dir, db)
    }

    #[test]
    fn fresh_file_db_is_incremental() {
        let (_dir, db) = temp_db();
        assert_eq!(db.space().unwrap().auto_vacuum, AutoVacuum::Incremental);
    }

    #[test]
    fn convert_existing_full_db_to_incremental_shrinks_file() {
        let (_dir, db) = full_db();
        db.conn()
            .execute("INSERT INTO fixture(data) VALUES(zeroblob(2097152))", [])
            .unwrap();
        db.conn().execute("DELETE FROM fixture", []).unwrap();
        let before_pages = db.space().unwrap().page_count;
        let converted = db.convert_to_incremental(Duration::from_secs(1)).unwrap();
        assert_eq!(converted.after.auto_vacuum, AutoVacuum::Incremental);
        assert!(converted.after.page_count < before_pages);
    }

    #[test]
    fn incremental_vacuum_frees_pages_and_refuses_in_none_mode() {
        let (_dir, db) = temp_db();
        db.conn()
            .execute("CREATE TABLE fixture(data BLOB)", [])
            .unwrap();
        db.conn()
            .execute("INSERT INTO fixture(data) VALUES(zeroblob(2097152))", [])
            .unwrap();
        db.conn().execute("DELETE FROM fixture", []).unwrap();
        assert_eq!(db.incremental_vacuum(0).unwrap(), 0);
        assert!(db.incremental_vacuum(2048).unwrap() > 0);
        let (_dir, legacy) = full_db();
        assert!(matches!(
            legacy
                .incremental_vacuum(1)
                .unwrap_err()
                .downcast_ref::<VacuumError>(),
            Some(VacuumError::NotIncremental)
        ));
    }

    #[test]
    fn convert_reports_busy_when_another_connection_holds_the_write_lock() {
        let (dir, db) = full_db();
        let locker = Connection::open(dir.path().join("state.db")).unwrap();
        locker.execute_batch("BEGIN IMMEDIATE").unwrap();
        let error = db
            .convert_to_incremental(Duration::from_millis(50))
            .unwrap_err();
        assert!(matches!(
            error.downcast_ref::<VacuumError>(),
            Some(VacuumError::Busy)
        ));
        assert_eq!(db.space().unwrap().auto_vacuum, AutoVacuum::None);
        locker.execute_batch("ROLLBACK").unwrap();
    }

    #[test]
    fn pragma_modes_and_non_busy_errors_are_typed() {
        assert_eq!(AutoVacuum::from_pragma(0).unwrap(), AutoVacuum::None);
        assert_eq!(AutoVacuum::from_pragma(1).unwrap(), AutoVacuum::Full);
        assert_eq!(AutoVacuum::from_pragma(2).unwrap(), AutoVacuum::Incremental);
        assert!(AutoVacuum::from_pragma(3).is_err());
        let error = map_busy(SqlError::InvalidQuery);
        assert!(error.downcast_ref::<VacuumError>().is_none());
    }

    #[test]
    fn vacuum_error_display_and_conversion_mode_check_are_stable() {
        assert_eq!(VacuumError::Busy.to_string(), "database is busy");
        assert_eq!(
            VacuumError::NotIncremental.to_string(),
            "incremental vacuum requires auto_vacuum=INCREMENTAL"
        );
        let legacy = DbSpace {
            page_size: 4096,
            page_count: 1,
            freelist_count: 0,
            auto_vacuum: AutoVacuum::None,
        };
        assert!(
            ensure_incremental(legacy)
                .unwrap_err()
                .to_string()
                .contains("database conversion did not enable incremental auto-vacuum")
        );
    }

    #[test]
    fn maintain_executes_incremental_convert_and_large_steps() {
        let (_dir, db) = temp_db();
        let incremental = db
            .maintain(2_000_000_000, MaintenanceStep::Incremental { pages: 1 })
            .unwrap();
        assert_eq!(incremental.reclaimed_pages, 0);
        let converted = db
            .maintain(2_000_000_000, MaintenanceStep::Convert)
            .unwrap();
        assert_eq!(
            converted.conversion.unwrap().after.auto_vacuum,
            AutoVacuum::Incremental
        );
        let large = db
            .maintain(
                2_000_000_000,
                MaintenanceStep::TooLargeForAuto {
                    live_bytes: 1,
                    free_bytes: 1,
                },
            )
            .unwrap();
        assert_eq!(large.reclaimed_pages, 0);
        assert!(large.conversion.is_none());
    }

    #[test]
    fn plan_maintenance_thresholds_and_caps() {
        let base = DbSpace {
            page_size: 4096,
            page_count: 16384,
            freelist_count: 0,
            auto_vacuum: AutoVacuum::None,
        };
        assert_eq!(plan_maintenance(&base, 1), MaintenanceStep::Convert);
        let too_large = DbSpace {
            page_count: base.page_count + 1,
            ..base
        };
        assert!(matches!(
            plan_maintenance(&too_large, 1),
            MaintenanceStep::TooLargeForAuto { .. }
        ));
        assert_eq!(plan_maintenance(&base, 0), MaintenanceStep::Nothing);
        let incremental = DbSpace {
            page_count: 20_000,
            freelist_count: 2048,
            auto_vacuum: AutoVacuum::Incremental,
            ..base
        };
        assert_eq!(
            plan_maintenance(&incremental, 0),
            MaintenanceStep::Incremental { pages: 2048 }
        );
        let ten_percent = DbSpace {
            page_size: 100_000,
            page_count: 1000,
            freelist_count: 100,
            ..incremental
        };
        assert_eq!(
            plan_maintenance(&ten_percent, 0),
            MaintenanceStep::Incremental { pages: 100 }
        );
        let below_ten_percent = DbSpace {
            freelist_count: 99,
            ..ten_percent
        };
        assert_eq!(
            plan_maintenance(&below_ten_percent, 0),
            MaintenanceStep::Nothing
        );
        let eight_mib = DbSpace {
            page_count: 5000,
            freelist_count: 2048,
            ..incremental
        };
        assert_eq!(
            plan_maintenance(&eight_mib, 0),
            MaintenanceStep::Incremental { pages: 2048 }
        );
        let below_eight_mib = DbSpace {
            freelist_count: 2047,
            ..eight_mib
        };
        assert_eq!(
            plan_maintenance(&below_eight_mib, 0),
            MaintenanceStep::Nothing
        );
    }

    #[test]
    fn maintain_prunes_old_read_notifications_and_keeps_unread() {
        let (_dir, db) = temp_db();
        let now = 2_000_000_000;
        db.conn().execute("INSERT INTO notifications(kind,issue_id,message,created_at_ms,read,worktree_path) VALUES('old','1','old',?1,1,'')", [now - NOTIFICATION_READ_RETENTION_SECS - 1]).unwrap();
        db.conn().execute("INSERT INTO notifications(kind,issue_id,message,created_at_ms,read,worktree_path) VALUES('unread','2','unread',?1,0,'')", [now - NOTIFICATION_READ_RETENTION_SECS - 1]).unwrap();
        assert_eq!(db.expired_read_notification_count(now).unwrap(), 1);
        let report = db.maintain(now, MaintenanceStep::Nothing).unwrap();
        assert_eq!(report.pruned_notifications, 1);
        assert_eq!(db.expired_read_notification_count(now).unwrap(), 0);
        let remaining: i64 = db
            .conn()
            .query_row("SELECT count(*) FROM notifications", [], |row| row.get(0))
            .unwrap();
        assert_eq!(remaining, 1);
    }

    #[test]
    fn notification_retention_constant_is_thirty_days() {
        assert_eq!(NOTIFICATION_READ_RETENTION_SECS, 30 * 24 * 60 * 60);
    }
}
