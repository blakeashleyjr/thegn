//! SQLite space inspection, bounded incremental reclamation, and conversion.

use crate::db::Db;
use crate::db_workspace::{ORPHAN_CACHE_SWEEP_MAX_KEYS, is_removed_root, registered_roots};
use crate::issue::AgentDispatchStatus;
use anyhow::{Result, anyhow};
use rusqlite::{Error as SqlError, ErrorCode, Transaction, TransactionBehavior, params};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::time::Duration;

pub const NOTIFICATION_READ_RETENTION_SECS: i64 = 30 * 24 * 60 * 60;
pub const INCREMENTAL_PAGES_PER_ROUND: u32 = 2048;
pub const AUTO_CONVERT_MAX_LIVE_BYTES: u64 = 64 * 1024 * 1024;
pub(crate) const DISPATCH_TERMINAL_RETENTION_SECS: i64 = 90 * 24 * 60 * 60;
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
    pub pruned_removed_notifications: usize,
    pub pruned_issue_links: usize,
    pub pruned_dispatches: usize,
    pub reclaimed_pages: u64,
    pub conversion: Option<ConvertOutcome>,
}

impl Db {
    /// Prune read notifications and issue links for a bounded set of removed roots.
    pub(crate) fn prune_removed_worktree_rows(&self, max_keys: usize) -> Result<(usize, usize)> {
        if max_keys == 0 {
            return Ok((0, 0));
        }
        let conn = self.conn();
        let keys = {
            let mut stmt = conn.prepare(
                "SELECT DISTINCT worktree_path FROM (
                   SELECT worktree_path FROM notifications WHERE read=1
                   UNION SELECT worktree_path FROM issue_links
                 ) WHERE worktree_path != '' ORDER BY worktree_path",
            )?;
            let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let registered = registered_roots(conn)?;
        let removed: Vec<String> = keys
            .into_iter()
            .filter(|key| is_removed_root(&registered, key))
            .take(max_keys.min(ORPHAN_CACHE_SWEEP_MAX_KEYS))
            .collect();
        let mut notifications = 0;
        let mut issue_links = 0;
        for key in removed {
            let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
            notifications += tx.execute(
                "DELETE FROM notifications WHERE worktree_path=?1 AND read=1",
                params![key],
            )?;
            issue_links += tx.execute(
                "DELETE FROM issue_links WHERE worktree_path=?1",
                params![key],
            )?;
            tx.commit()?;
        }
        Ok((notifications, issue_links))
    }

    /// Prune old terminal dispatch history while preserving rows still used by admission or supervision.
    pub(crate) fn prune_terminal_dispatches(&self, now_ms: i64) -> Result<usize> {
        let statuses = [
            AgentDispatchStatus::Merged,
            AgentDispatchStatus::Abandoned,
            AgentDispatchStatus::Done,
            AgentDispatchStatus::Failed,
        ]
        .into_iter()
        .map(|status| format!("'{}'", status.as_str()))
        .collect::<Vec<_>>()
        .join(",");
        let cutoff = now_ms.saturating_sub(DISPATCH_TERMINAL_RETENTION_SECS.saturating_mul(1000));
        let conn = self.conn();
        let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
        let (ids, children) = {
            let query = format!(
                "SELECT d.id FROM agent_dispatches d
                   WHERE d.status IN ({statuses}) AND d.task_kind IS NULL
                     AND COALESCE(d.status_changed_at_ms,d.exited_at_ms,d.dispatched_at_ms) < ?1
                     AND NOT EXISTS (SELECT 1 FROM worktrees w WHERE w.worktree=d.worktree_path)
                     AND NOT EXISTS (SELECT 1 FROM autopilot_runs a WHERE a.dispatch_id=d.id
                                     AND a.state NOT IN ('done','stopped'))"
            );
            let mut stmt = tx.prepare(&query)?;
            let rows = stmt.query_map(params![cutoff], |row| row.get::<_, i64>(0))?;
            let ids = rows.collect::<rusqlite::Result<Vec<_>>>()?;
            let mut stmt = tx.prepare(
                "SELECT parent_id, id FROM agent_dispatches WHERE parent_id IS NOT NULL",
            )?;
            let rows =
                stmt.query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)))?;
            let children = rows
                .collect::<rusqlite::Result<Vec<_>>>()?
                .into_iter()
                .fold(
                    HashMap::<i64, Vec<i64>>::new(),
                    |mut children, (parent, child)| {
                        children.entry(parent).or_default().push(child);
                        children
                    },
                );
            (ids, children)
        };
        let mut candidates: HashSet<i64> = ids.into_iter().collect();
        loop {
            let retained: HashSet<i64> = candidates
                .iter()
                .copied()
                .filter(|id| {
                    children.get(id).is_none_or(|children| {
                        children.iter().all(|child| candidates.contains(child))
                    })
                })
                .collect();
            if retained.len() == candidates.len() {
                break;
            }
            candidates = retained;
        }
        let mut ids: Vec<i64> = candidates.into_iter().collect();
        ids.sort_unstable();
        for id in &ids {
            tx.execute(
                "DELETE FROM agent_dispatch_notes WHERE dispatch_id=?1",
                params![id],
            )?;
            tx.execute(
                "DELETE FROM pipeline_validations WHERE dispatch_id=?1",
                params![id],
            )?;
            tx.execute("DELETE FROM agent_dispatches WHERE id=?1", params![id])?;
        }
        tx.commit()?;
        Ok(ids.len())
    }

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
        let (pruned_removed_notifications, pruned_issue_links) = self
            .prune_removed_worktree_rows(ORPHAN_CACHE_SWEEP_MAX_KEYS)
            .map_err(map_busy_any)?;
        let pruned_dispatches = self
            .prune_terminal_dispatches(now_secs.saturating_mul(1000))
            .map_err(map_busy_any)?;
        let mut report = MaintenanceReport {
            pruned_notifications,
            pruned_removed_notifications,
            pruned_issue_links,
            pruned_dispatches,
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

fn map_busy_any(error: anyhow::Error) -> anyhow::Error {
    match error.downcast::<SqlError>() {
        Ok(sql) => map_busy(sql),
        Err(other) => other,
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
    use crate::store::NotificationStore;
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

    fn notification(db: &Db, issue: &str, worktree: &str, read: i64, created: i64) {
        db.conn()
            .execute(
                "INSERT INTO notifications(kind,issue_id,message,created_at_ms,read,worktree_path) \
             VALUES('fixture',?1,'fixture',?2,?3,?4)",
                params![issue, created, read, worktree],
            )
            .unwrap();
    }

    #[test]
    fn removed_worktree_read_notifications_prune_and_unread_survive() {
        let (dir, db) = temp_db();
        let gone = dir.path().join("gone").to_string_lossy().into_owned();
        let registered = dir.path().join("registered").to_string_lossy().into_owned();
        let ancestor = dir.path().join("ancestor").to_string_lossy().into_owned();
        let registered_child = format!("{ancestor}/child");
        let existing = dir.path().join("existing");
        std::fs::create_dir(&existing).unwrap();
        let existing = existing.to_string_lossy().into_owned();
        db.conn()
            .execute(
                "INSERT INTO worktrees(worktree,repo_path,location) VALUES(?1,?2,'local')",
                params![registered, registered],
            )
            .unwrap();
        db.conn()
            .execute(
                "INSERT INTO worktrees(worktree,repo_path,location) VALUES(?1,?2,'remote')",
                params![registered_child, registered],
            )
            .unwrap();
        let now = 2_000_000_000;
        notification(&db, "read-gone", &gone, 1, now);
        notification(&db, "unread-gone", &gone, 0, now);
        notification(&db, "registered", &registered, 1, now);
        notification(&db, "existing", &existing, 1, now);
        notification(&db, "ancestor", &ancestor, 1, now);
        notification(&db, "empty", "", 1, now);

        assert_eq!(db.prune_removed_worktree_rows(0).unwrap(), (0, 0));
        let (removed_notifications, removed_links) = db
            .prune_removed_worktree_rows(ORPHAN_CACHE_SWEEP_MAX_KEYS)
            .unwrap();
        assert_eq!((removed_notifications, removed_links), (1, 0));
        let unread: i64 = db
            .conn()
            .query_row(
                "SELECT count(*) FROM notifications WHERE issue_id='unread-gone'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(unread, 1);
        let retained_read: i64 = db
            .conn()
            .query_row(
                "SELECT count(*) FROM notifications WHERE read=1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(retained_read, 4);
    }

    #[test]
    fn unread_removed_notification_is_pruned_once_read() {
        let (dir, db) = temp_db();
        let gone = dir.path().join("gone").to_string_lossy().into_owned();
        notification(&db, "unread", &gone, 0, 2_000_000_000);
        assert_eq!(db.prune_removed_worktree_rows(8).unwrap(), (0, 0));
        db.conn()
            .execute(
                "UPDATE notifications SET read=1 WHERE issue_id='unread'",
                [],
            )
            .unwrap();
        assert_eq!(db.prune_removed_worktree_rows(8).unwrap(), (1, 0));
    }

    #[test]
    fn issue_links_of_removed_worktrees_are_pruned() {
        let (dir, db) = temp_db();
        let gone = dir.path().join("gone").to_string_lossy().into_owned();
        let registered = dir.path().join("registered").to_string_lossy().into_owned();
        let remote = dir.path().join("remote").to_string_lossy().into_owned();
        let existing = dir.path().join("existing");
        std::fs::create_dir(&existing).unwrap();
        let existing = existing.to_string_lossy().into_owned();
        db.conn()
            .execute(
                "INSERT INTO worktrees(worktree,location) VALUES(?1,'local'),(?2,'remote')",
                params![registered, remote],
            )
            .unwrap();
        for (path, id) in [
            (&gone, "gone"),
            (&registered, "registered"),
            (&remote, "remote"),
            (&existing, "existing"),
        ] {
            db.conn()
                .execute(
                    "INSERT INTO issue_links(worktree_path,issue_id,linked_at) VALUES(?1,?2,1)",
                    params![path, id],
                )
                .unwrap();
        }
        assert_eq!(db.prune_removed_worktree_rows(8).unwrap(), (0, 1));
        let count: i64 = db
            .conn()
            .query_row("SELECT count(*) FROM issue_links", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 3);
    }

    fn dispatch(
        db: &Db,
        worktree: &str,
        status: &str,
        dispatched: i64,
        changed: Option<i64>,
        task_kind: Option<&str>,
        parent_id: Option<i64>,
    ) -> i64 {
        db.conn().execute(
            "INSERT INTO agent_dispatches(issue_id,worktree_path,agent_name,dispatched_at_ms,status,status_changed_at_ms,task_kind,parent_id) \
             VALUES(?1,?2,'fixture',?3,?4,?5,?6,?7)",
            params![worktree, worktree, dispatched, status, changed, task_kind, parent_id],
        ).unwrap();
        db.conn().last_insert_rowid()
    }

    #[test]
    fn terminal_dispatch_retention_is_ninety_days() {
        let (_dir, db) = temp_db();
        let now = 2_000_000_000_000_i64;
        let old = now - 200 * 24 * 60 * 60 * 1000;
        let done_old = dispatch(&db, "/gone/old", "done", old, None, None, None);
        let done_89 = dispatch(
            &db,
            "/gone/recent",
            "done",
            now - 89 * 24 * 60 * 60 * 1000,
            None,
            None,
            None,
        );
        for status in ["running", "queued", "waiting_human", "pr_open", "unknown"] {
            dispatch(
                &db,
                &format!("/gone/{status}"),
                status,
                old,
                None,
                None,
                None,
            );
        }
        let changed_recent = dispatch(
            &db,
            "/gone/changed",
            "done",
            old,
            Some(now - 10 * 24 * 60 * 60 * 1000),
            None,
            None,
        );
        assert_eq!(db.prune_terminal_dispatches(now).unwrap(), 1);
        assert!(db.get_dispatch(done_old).unwrap().is_none());
        assert!(db.get_dispatch(done_89).unwrap().is_some());
        assert!(db.get_dispatch(changed_recent).unwrap().is_some());
        let remaining: i64 = db
            .conn()
            .query_row("SELECT count(*) FROM agent_dispatches", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(remaining, 7);
    }

    #[test]
    fn pruned_dispatch_takes_notes_and_validations() {
        let (_dir, db) = temp_db();
        let now = 2_000_000_000_000_i64;
        let old = now - 91 * 24 * 60 * 60 * 1000;
        let expired = dispatch(&db, "/gone/expired", "failed", old, None, None, None);
        db.conn()
            .execute(
                "INSERT INTO worktrees(worktree) VALUES('/registered/retained')",
                [],
            )
            .unwrap();
        let retained = dispatch(&db, "/registered/retained", "failed", old, None, None, None);
        db.conn().execute("INSERT INTO agent_dispatch_notes(dispatch_id,created_at_ms,text) VALUES(?1,1,'note'),(?2,1,'note')", params![expired, retained]).unwrap();
        db.conn().execute("INSERT INTO pipeline_validations(dispatch_id,task,commit_sha,class,ran_at_ms) VALUES(?1,'task','sha','class',1),(?2,'task','sha','class',1)", params![expired, retained]).unwrap();
        assert_eq!(db.prune_terminal_dispatches(now).unwrap(), 1);
        let notes: i64 = db
            .conn()
            .query_row("SELECT count(*) FROM agent_dispatch_notes", [], |row| {
                row.get(0)
            })
            .unwrap();
        let validations: i64 = db
            .conn()
            .query_row("SELECT count(*) FROM pipeline_validations", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!((notes, validations), (1, 1));
    }

    #[test]
    fn dispatch_retention_keeps_referenced_rows() {
        let (_dir, db) = temp_db();
        let now = 2_000_000_000_000_i64;
        let old = now - 91 * 24 * 60 * 60 * 1000;
        let registered_path = "/registered/wt";
        db.conn()
            .execute(
                "INSERT INTO worktrees(worktree) VALUES(?1)",
                [registered_path],
            )
            .unwrap();
        let registered = dispatch(&db, registered_path, "done", old, None, None, None);
        let autopilot = dispatch(&db, "/gone/autopilot", "done", old, None, None, None);
        db.conn().execute("INSERT INTO autopilot_runs(provider,account,issue_id,repo_root,state,dispatch_id,created_at,updated_at) VALUES('p','a','i','/repo','working',?1,1,1)", [autopilot]).unwrap();
        let parent = dispatch(&db, "/gone/parent", "done", old, None, None, None);
        dispatch(&db, "/gone/child", "running", old, None, None, Some(parent));
        let review = dispatch(&db, "/gone/review", "done", old, None, Some("review"), None);
        assert_eq!(db.prune_terminal_dispatches(now).unwrap(), 0);
        for id in [registered, autopilot, parent, review] {
            assert!(db.get_dispatch(id).unwrap().is_some());
        }
    }

    #[test]
    fn terminal_dispatch_status_coverage_is_exhaustive() {
        let statuses = [
            AgentDispatchStatus::Merged,
            AgentDispatchStatus::Abandoned,
            AgentDispatchStatus::Done,
            AgentDispatchStatus::Failed,
        ]
        .map(AgentDispatchStatus::as_str);
        assert_eq!(statuses, ["merged", "abandoned", "done", "failed"]);
        for status in [
            AgentDispatchStatus::Queued,
            AgentDispatchStatus::Spawning,
            AgentDispatchStatus::Running,
            AgentDispatchStatus::WaitingHuman,
            AgentDispatchStatus::PrOpen,
            AgentDispatchStatus::Merged,
            AgentDispatchStatus::Abandoned,
            AgentDispatchStatus::Done,
            AgentDispatchStatus::Failed,
            AgentDispatchStatus::Unknown,
        ] {
            let exhaustive_terminal = match status {
                AgentDispatchStatus::Merged
                | AgentDispatchStatus::Abandoned
                | AgentDispatchStatus::Done
                | AgentDispatchStatus::Failed => true,
                AgentDispatchStatus::Queued
                | AgentDispatchStatus::Spawning
                | AgentDispatchStatus::Running
                | AgentDispatchStatus::WaitingHuman
                | AgentDispatchStatus::PrOpen
                | AgentDispatchStatus::Unknown => false,
            };
            assert_eq!(status.is_terminal(), exhaustive_terminal);
        }
    }

    #[test]
    fn terminal_dispatch_retention_keeps_all_ancestors_of_non_candidate_child() {
        let (_dir, db) = temp_db();
        let now = 2_000_000_000_000_i64;
        let old = now - 91 * 24 * 60 * 60 * 1000;
        let grandparent = dispatch(&db, "/gone/grandparent", "done", old, None, None, None);
        let parent = dispatch(
            &db,
            "/gone/parent",
            "done",
            old,
            None,
            None,
            Some(grandparent),
        );
        let child = dispatch(&db, "/gone/child", "running", old, None, None, Some(parent));

        assert_eq!(db.prune_terminal_dispatches(now).unwrap(), 0);
        for id in [grandparent, parent, child] {
            assert!(db.get_dispatch(id).unwrap().is_some());
        }
    }

    #[test]
    fn maintain_classifies_busy_retention_as_vacuum_busy() {
        let (dir, db) = temp_db();
        db.conn().busy_timeout(Duration::from_millis(10)).unwrap();
        let locker = Connection::open(dir.path().join("state.db")).unwrap();
        locker.execute_batch("BEGIN IMMEDIATE").unwrap();
        let result = db.maintain(2_000_000_000, MaintenanceStep::Nothing);
        assert!(matches!(
            result,
            Err(error) if matches!(error.downcast_ref::<VacuumError>(), Some(VacuumError::Busy))
        ));
        locker.execute_batch("ROLLBACK").unwrap();
    }

    #[test]
    fn anyhow_wrapped_sqlite_busy_is_classified() {
        let (dir, db) = temp_db();
        db.conn().busy_timeout(Duration::from_millis(10)).unwrap();
        let locker = Connection::open(dir.path().join("state.db")).unwrap();
        locker.execute_batch("BEGIN IMMEDIATE").unwrap();
        let error = db.prune_removed_worktree_rows(1).unwrap_err();
        let error = map_busy_any(error);
        assert!(matches!(
            error.downcast_ref::<VacuumError>(),
            Some(VacuumError::Busy)
        ));
        locker.execute_batch("ROLLBACK").unwrap();
    }

    #[test]
    fn maintain_reports_retention_counts() {
        let (dir, db) = temp_db();
        let now = 2_000_000_000_i64;
        let gone = dir.path().join("gone").to_string_lossy().into_owned();
        notification(&db, "read", &gone, 1, now);
        db.conn()
            .execute(
                "INSERT INTO issue_links(worktree_path,issue_id,linked_at) VALUES(?1,'issue',1)",
                [&gone],
            )
            .unwrap();
        dispatch(
            &db,
            "/gone/dispatch",
            "done",
            now * 1000 - 91 * 24 * 60 * 60 * 1000,
            None,
            None,
            None,
        );
        let report = db.maintain(now, MaintenanceStep::Nothing).unwrap();
        assert_eq!(report.pruned_removed_notifications, 1);
        assert_eq!(report.pruned_issue_links, 1);
        assert_eq!(report.pruned_dispatches, 1);
    }

    #[test]
    fn notification_retention_constant_is_thirty_days() {
        assert_eq!(NOTIFICATION_READ_RETENTION_SECS, 30 * 24 * 60 * 60);
    }
}
