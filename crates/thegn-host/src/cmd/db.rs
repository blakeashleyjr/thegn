//! State database maintenance command.

use anyhow::Result;
use clap::Subcommand;
use serde::Serialize;
use std::time::Duration;
use thegn_core::db::Db;
use thegn_core::outln;
use thegn_core::{
    AutoVacuum, DbSpace, MaintenanceStep, NOTIFICATION_READ_RETENTION_SECS, msg, plan_maintenance,
};

const CHUNK_PAGES: u32 = 1024;
const MIB: u64 = 1024 * 1024;

#[derive(Clone, Debug, Subcommand)]
pub enum Action {
    /// Reclaim unused pages from the state database.
    Vacuum {
        /// Show the planned operation without writing to the database.
        #[arg(long)]
        dry_run: bool,
        /// Emit one machine-readable JSON report.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct VacuumReport {
    pub before: DbSpace,
    pub after: DbSpace,
    pub mode_before: AutoVacuum,
    pub mode_after: AutoVacuum,
    pub freed_bytes: u64,
    pub pruned_notifications: usize,
    pub dry_run: bool,
}

pub fn run(action: Action) -> Result<()> {
    match action {
        Action::Vacuum { dry_run, json } => {
            let startup_pruned = if dry_run {
                0
            } else {
                match Db::open_read_only()? {
                    Some(db) => db.expired_read_notification_count(thegn_core::util::now())?,
                    None => 0,
                }
            };
            let db = if dry_run {
                Db::open_read_only()?
                    .ok_or_else(|| anyhow::anyhow!("state database does not exist yet"))?
            } else {
                Db::open()?
            };
            let before = db.space()?;
            if !json {
                outln!(
                    "before: {} MB ({} MB reclaimable, auto_vacuum {})",
                    before.file_bytes().div_ceil(MIB),
                    before.free_bytes().div_ceil(MIB),
                    mode_name(before.auto_vacuum)
                );
            }
            let mut report = vacuum_report(&db, dry_run)?;
            report.pruned_notifications += startup_pruned;
            if json && dry_run {
                msg::info(&format!(
                    "plan: {}",
                    plan_name(plan_maintenance(&before, 1))
                ));
            }
            if json {
                return super::emit_json(&report);
            }
            if dry_run {
                outln!("plan: {}", plan_name(plan_maintenance(&before, 1)));
            } else {
                outln!(
                    "after: {} MB (freed {} MB, auto_vacuum {})",
                    report.after.file_bytes().div_ceil(MIB),
                    report.freed_bytes.div_ceil(MIB),
                    mode_name(report.mode_after)
                );
                outln!(
                    "pruned {} expired read notifications",
                    report.pruned_notifications
                );
            }
            Ok(())
        }
    }
}

/// Execute or preview the selected maintenance action and return its emitted report.
pub fn vacuum_report(db: &Db, dry_run: bool) -> Result<VacuumReport> {
    let before = db.space()?;
    if dry_run {
        return Ok(VacuumReport {
            before,
            after: before,
            mode_before: before.auto_vacuum,
            mode_after: before.auto_vacuum,
            freed_bytes: 0,
            pruned_notifications: 0,
            dry_run: true,
        });
    }

    let (after, pruned_notifications) = if before.auto_vacuum != AutoVacuum::Incremental {
        msg::info(
            "converting to incremental auto-vacuum (full VACUUM; a running thegn pauses DB writes until it finishes)...",
        );
        let pruned = db.prune_notifications(NOTIFICATION_READ_RETENTION_SECS)?;
        let conversion = db
            .convert_to_incremental(Duration::from_secs(30))
            .map_err(|error| {
                if error
                    .downcast_ref::<thegn_core::VacuumError>()
                    .is_some_and(|e| matches!(e, thegn_core::VacuumError::Busy))
                {
                    anyhow::anyhow!("the state database is busy; retry")
                } else {
                    error
                }
            })?;
        (conversion.after, pruned)
    } else {
        let pruned = db.prune_notifications(NOTIFICATION_READ_RETENTION_SECS)?;
        let total = db.space()?.freelist_count;
        let mut freed = 0_u64;
        loop {
            let chunk = db.incremental_vacuum(CHUNK_PAGES).map_err(|error| {
                if error
                    .downcast_ref::<thegn_core::VacuumError>()
                    .is_some_and(|e| matches!(e, thegn_core::VacuumError::Busy))
                {
                    anyhow::anyhow!("the state database is busy; retry")
                } else {
                    error
                }
            })?;
            freed += chunk;
            if chunk == 0 {
                break;
            }
            msg::info(&format!("freed {freed}/{total} pages"));
        }
        (db.space()?, pruned)
    };
    Ok(VacuumReport {
        before,
        after,
        mode_before: before.auto_vacuum,
        mode_after: after.auto_vacuum,
        freed_bytes: before.file_bytes().saturating_sub(after.file_bytes()),
        pruned_notifications,
        dry_run: false,
    })
}

fn mode_name(mode: AutoVacuum) -> &'static str {
    match mode {
        AutoVacuum::None => "none",
        AutoVacuum::Full => "full",
        AutoVacuum::Incremental => "incremental",
    }
}

fn plan_name(step: MaintenanceStep) -> String {
    match step {
        MaintenanceStep::Nothing => "no reclamation needed".into(),
        MaintenanceStep::Incremental { pages } => {
            format!("incremental vacuum (up to {pages} pages per round)")
        }
        MaintenanceStep::Convert => "convert to incremental auto-vacuum with a full VACUUM".into(),
        MaintenanceStep::TooLargeForAuto {
            live_bytes,
            free_bytes,
        } => format!(
            "manual conversion ({}, {} free)",
            live_bytes.div_ceil(MIB),
            free_bytes.div_ceil(MIB)
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::store::NotificationStore;

    fn fixture() -> (tempfile::TempDir, Db) {
        let dir = tempfile::Builder::new().tempdir().unwrap();
        let db = Db::open_at(&dir.path().join("state.db")).unwrap();
        (dir, db)
    }

    #[test]
    fn dry_run_writes_nothing() {
        let (_dir, db) = fixture();
        let before = db.space().unwrap();
        db.put_notification("assigned", "fixture", "still here", "")
            .unwrap();
        let with_notification = db.space().unwrap();
        let report = vacuum_report(&db, true).unwrap();
        assert_eq!(report.before, with_notification);
        assert_eq!(report.after, with_notification);
        assert_eq!(db.space().unwrap(), with_notification);
        assert_eq!(db.get_all_notifications(10).unwrap().len(), 1);
        assert_eq!(before.auto_vacuum, AutoVacuum::Incremental);
        assert!(report.dry_run);
    }

    #[test]
    fn vacuum_json_reports_incremental_reclamation() {
        let (_dir, db) = fixture();
        let report = vacuum_report(&db, false).unwrap();
        assert_eq!(report.mode_before, AutoVacuum::Incremental);
        assert_eq!(report.mode_after, AutoVacuum::Incremental);
        assert_eq!(report.after.auto_vacuum, AutoVacuum::Incremental);
        assert!(!report.dry_run);
        let encoded = serde_json::to_value(&report).unwrap();
        assert_eq!(encoded["mode_before"], "incremental");
        assert_eq!(encoded["mode_after"], "incremental");
        assert!(encoded.get("pruned_notifications").is_some());
    }

    #[test]
    fn vacuum_json_reports_conversion() {
        use rusqlite::Connection;

        let dir = tempfile::Builder::new().tempdir().unwrap();
        let path = dir.path().join("state.db");
        drop(Db::open_at(&path).unwrap());
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("PRAGMA auto_vacuum=NONE; VACUUM;")
            .unwrap();
        drop(conn);

        let db = Db::open_at(&path).unwrap();
        let report = vacuum_report(&db, false).unwrap();
        assert_eq!(report.mode_before, AutoVacuum::None);
        assert_eq!(report.mode_after, AutoVacuum::Incremental);
        let encoded = serde_json::to_value(&report).unwrap();
        assert_eq!(encoded["mode_before"], "none");
        assert_eq!(encoded["mode_after"], "incremental");
    }

    #[test]
    fn report_retention_uses_the_core_policy() {
        assert_eq!(NOTIFICATION_READ_RETENTION_SECS, 30 * 24 * 60 * 60);
    }
}
