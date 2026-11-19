//! State database maintenance command.

use anyhow::{Context, Result};
use clap::Subcommand;
use serde::Serialize;
use std::time::Duration;
use thegn_core::db::Db;
use thegn_core::outln;
use thegn_core::{
    AutoVacuum, DbSpace, HostRoom, MaintenanceStep, NOTIFICATION_READ_RETENTION_SECS, msg,
    plan_maintenance,
};

const CHUNK_PAGES: u32 = 1024;
const MIB: u64 = 1024 * 1024;
/// How long `thegn db vacuum` waits for a UI's maintenance pass to finish.
const LOCK_WAIT: Duration = Duration::from_secs(30);

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
    pub pruned_semantic_rows: u64,
    pub pruned_cache_keys: usize,
    pub dry_run: bool,
    /// Why a conversion was refused (low disk), with the need/free numbers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refused: Option<Refusal>,
}

/// A conversion `thegn db vacuum` declined to start.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Refusal {
    pub reason: &'static str,
    pub need_bytes: u64,
    pub free_bytes: u64,
}

pub fn run(action: Action) -> Result<()> {
    match action {
        Action::Vacuum { dry_run, json } => {
            let startup_pruned = if dry_run {
                0
            } else {
                match Db::open_read_only_current() {
                    Ok(Some(db)) => db.expired_read_notification_count(thegn_core::util::now())?,
                    Ok(None) => 0,
                    Err(error)
                        if matches!(
                            error.downcast_ref::<thegn_core::db::ReadOnlyOpenError>(),
                            Some(thegn_core::db::ReadOnlyOpenError::WalWithoutIndex { .. })
                        ) =>
                    {
                        tracing::debug!(%error, "falling back to immutable notification pre-count");
                        match Db::open_read_only()? {
                            Some(db) => {
                                db.expired_read_notification_count(thegn_core::util::now())?
                            }
                            None => 0,
                        }
                    }
                    Err(error) => return Err(error),
                }
            };
            let db = if dry_run {
                Db::open_read_only_current()?
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
            let room = db_room(&db);
            let mut report = vacuum_report(&db, dry_run)?;
            report.pruned_notifications += startup_pruned;
            if json && dry_run {
                msg::info(&format!(
                    "plan: {}",
                    plan_name(plan_maintenance(&before, room, 1))
                ));
            }
            if json {
                return super::emit_json(&report);
            }
            if dry_run {
                outln!("plan: {}", plan_name(plan_maintenance(&before, room, 1)));
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
                outln!(
                    "pruned {} stale semantic rows and {} orphaned cache keys",
                    report.pruned_semantic_rows,
                    report.pruned_cache_keys
                );
            }
            Ok(())
        }
    }
}

fn db_room(db: &Db) -> HostRoom {
    db.file_path()
        .map(|path| crate::db_maintain::host_room(&path))
        .unwrap_or_default()
}

/// Execute or preview the selected maintenance action and return its emitted report.
/// A real run first takes the shared maintenance lock (waiting at most
/// [`LOCK_WAIT`]) so it never overlaps a UI's automatic pass.
pub fn vacuum_report(db: &Db, dry_run: bool) -> Result<VacuumReport> {
    vacuum_report_with(
        db,
        dry_run,
        || crate::db_maintain::maintenance_lock_within(LOCK_WAIT),
        db_room(db),
    )
}

fn vacuum_report_with(
    db: &Db,
    dry_run: bool,
    lock: impl FnOnce() -> Result<std::fs::File>,
    room: HostRoom,
) -> Result<VacuumReport> {
    let before = db.space()?;
    if dry_run {
        return Ok(VacuumReport {
            before,
            after: before,
            mode_before: before.auto_vacuum,
            mode_after: before.auto_vacuum,
            freed_bytes: 0,
            pruned_notifications: 0,
            pruned_semantic_rows: 0,
            pruned_cache_keys: 0,
            dry_run: true,
            refused: None,
        });
    }
    let _lock = lock()?;
    let before = db.space()?;

    // A running UI instance may prune concurrently; the bounded pruning
    // transactions are idempotent and SQLite's busy timeout handles overlap.
    let mut progress_slices = 0_usize;
    let (pruned_semantic_rows, pruned_cache_keys) = db
        .prune_stale_caches_to_end(4096, &mut |scanned| {
            progress_slices += 1;
            if progress_slices.is_multiple_of(64) {
                msg::info(&format!("scanned {scanned} semantic rows"));
            }
        })
        .context("pruning stale cache rows")
        .map_err(map_busy)?;

    // Manual runs ignore the automatic size ceiling and attempt budget; only
    // free space can refuse a conversion.
    let mut refused = None;
    let (after, pruned_notifications) = if before.auto_vacuum != AutoVacuum::Incremental
        && let Some((need_bytes, free_bytes)) = conversion_shortfall(&before, room)
    {
        msg::warn(&format!(
            "not converting: needs {} MB free, {} MB available",
            need_bytes.div_ceil(MIB),
            free_bytes / MIB
        ));
        refused = Some(Refusal {
            reason: "low_disk",
            need_bytes,
            free_bytes,
        });
        let pruned = db.prune_notifications(NOTIFICATION_READ_RETENTION_SECS)?;
        (db.space()?, pruned)
    } else if before.auto_vacuum != AutoVacuum::Incremental {
        msg::info(
            "converting to incremental auto-vacuum (full VACUUM; a running thegn pauses DB writes until it finishes)...",
        );
        let pruned = db.prune_notifications(NOTIFICATION_READ_RETENTION_SECS)?;
        let conversion = db
            .convert_to_incremental(Duration::from_secs(30))
            .map_err(map_busy)?;
        (conversion.after, pruned)
    } else {
        let pruned = db.prune_notifications(NOTIFICATION_READ_RETENTION_SECS)?;
        let total = db.space()?.freelist_count;
        let mut freed = 0_u64;
        loop {
            let chunk = db.incremental_vacuum(CHUNK_PAGES).map_err(map_busy)?;
            freed += chunk;
            if chunk == 0 {
                break;
            }
            msg::info(&format!("freed {freed}/{total} pages"));
        }
        if freed > 0 {
            match db.checkpoint_truncate() {
                Ok(truncated) => tracing::debug!(truncated, "WAL checkpoint after vacuum"),
                Err(error) => tracing::debug!(%error, "WAL checkpoint after vacuum failed"),
            }
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
        pruned_semantic_rows,
        pruned_cache_keys,
        dry_run: false,
        refused,
    })
}

/// The free space a manual conversion lacks, as `(need, free)`. Uses the
/// planner's `LowDisk` verdict; a legacy DB over the automatic ceiling (which
/// the planner reports as `TooLargeForAuto`) gets the same two checks here.
fn conversion_shortfall(space: &DbSpace, room: HostRoom) -> Option<(u64, u64)> {
    match plan_maintenance(space, room, 1) {
        MaintenanceStep::LowDisk {
            need_bytes,
            free_bytes,
        } => Some((need_bytes, free_bytes)),
        MaintenanceStep::TooLargeForAuto { .. } => {
            let need = thegn_core::conversion_needs_bytes(space);
            match (room.db_fs_free, room.tmp_free) {
                (Some(free), _) if free < need => Some((need, free)),
                (_, Some(free)) if free < space.live_bytes() => Some((space.live_bytes(), free)),
                _ => None,
            }
        }
        _ => None,
    }
}

fn map_busy(error: anyhow::Error) -> anyhow::Error {
    let vacuum_busy = error
        .downcast_ref::<thegn_core::VacuumError>()
        .is_some_and(|error| matches!(error, thegn_core::VacuumError::Busy));
    let sqlite_busy = error.chain().any(|cause| {
        cause
            .downcast_ref::<rusqlite::Error>()
            .is_some_and(|error| match error {
                rusqlite::Error::SqliteFailure(code, _) => matches!(
                    code.code,
                    rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                ),
                _ => false,
            })
    });
    if vacuum_busy || sqlite_busy {
        anyhow::anyhow!("the state database is busy; retry")
    } else {
        error
    }
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
        MaintenanceStep::LowDisk {
            need_bytes,
            free_bytes,
        } => format!(
            "no conversion: needs {} MB free, {} MB available",
            need_bytes.div_ceil(MIB),
            free_bytes / MIB
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::store::{NotificationStore, SemanticStore};

    fn seed_gone_semantic_row(db: &Db) {
        db.replace_file_entities(
            "/gone/wt/a.rs",
            &[thegn_core::store::SemEntityRow {
                id: "gone-entity".into(),
                file: "/gone/wt/a.rs".into(),
                name: "gone".into(),
                kind: thegn_core::semantic::EntityKind::Function,
                start_line: 1,
                end_line: 1,
                source_hash: "hash".into(),
            }],
        )
        .unwrap();
    }

    fn fixture() -> (tempfile::TempDir, Db) {
        let dir = tempfile::Builder::new().tempdir().unwrap();
        let db = Db::open_at(&dir.path().join("state.db")).unwrap();
        (dir, db)
    }

    #[test]
    fn dry_run_report_counts_pages_held_in_the_wal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        drop(Db::open_at(&path).unwrap());
        let writer = rusqlite::Connection::open(&path).unwrap();
        writer
            .execute_batch(
                "PRAGMA wal_autocheckpoint = 0;
                 PRAGMA wal_checkpoint(TRUNCATE);
                 CREATE TABLE wal_pages(payload BLOB NOT NULL);
                 WITH RECURSIVE rows(n) AS (
                     SELECT 1 UNION ALL SELECT n + 1 FROM rows WHERE n < 200
                 )
                 INSERT INTO wal_pages(payload) SELECT randomblob(1024) FROM rows;",
            )
            .unwrap();

        let current = Db::open_read_only_current_at(&path).unwrap().unwrap();
        let current_report = vacuum_report(&current, true).unwrap();
        let immutable = Db::open_read_only_at(&path).unwrap().unwrap();
        let immutable_report = vacuum_report(&immutable, true).unwrap();

        assert!(
            current_report.before.page_count > immutable_report.before.page_count,
            "WAL-aware dry-run page count {} must exceed immutable page count {}",
            current_report.before.page_count,
            immutable_report.before.page_count
        );
        drop(immutable);
        drop(current);
        drop(writer);
    }

    #[test]
    fn dry_run_writes_nothing() {
        let (_dir, db) = fixture();
        let before = db.space().unwrap();
        db.put_notification("assigned", "fixture", "still here", "")
            .unwrap();
        seed_gone_semantic_row(&db);
        let with_notification = db.space().unwrap();
        let report = vacuum_report(&db, true).unwrap();
        assert_eq!(report.before, with_notification);
        assert_eq!(report.after, with_notification);
        assert_eq!(db.space().unwrap(), with_notification);
        assert_eq!(db.get_all_notifications(10).unwrap().len(), 1);
        assert_eq!(db.entities_under("/gone/wt").unwrap().len(), 1);
        assert_eq!(report.pruned_semantic_rows, 0);
        assert_eq!(report.pruned_cache_keys, 0);
        assert_eq!(before.auto_vacuum, AutoVacuum::Incremental);
        assert!(report.dry_run);
    }

    #[test]
    fn vacuum_prunes_stale_semantic_rows_before_reclaim() {
        let (dir, db) = fixture();
        seed_gone_semantic_row(&db);

        let report = vacuum_report_with(&db, false, temp_lock(&dir), HostRoom::default()).unwrap();

        assert!(report.pruned_semantic_rows >= 1);
        assert!(db.entities_under("/gone/wt").unwrap().is_empty());
        let encoded = serde_json::to_value(&report).unwrap();
        assert!(encoded.get("pruned_semantic_rows").is_some());
        assert!(encoded.get("pruned_cache_keys").is_some());
    }

    #[test]
    fn vacuum_json_reports_incremental_reclamation() {
        let (dir, db) = fixture();
        let report = vacuum_report_with(&db, false, temp_lock(&dir), HostRoom::default()).unwrap();
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
        let report = vacuum_report_with(&db, false, temp_lock(&dir), HostRoom::default()).unwrap();
        assert_eq!(report.mode_before, AutoVacuum::None);
        assert_eq!(report.mode_after, AutoVacuum::Incremental);
        let encoded = serde_json::to_value(&report).unwrap();
        assert_eq!(encoded["mode_before"], "none");
        assert_eq!(encoded["mode_after"], "incremental");
    }

    fn legacy_fixture() -> (tempfile::TempDir, Db) {
        let dir = tempfile::Builder::new().tempdir().unwrap();
        let path = dir.path().join("state.db");
        drop(Db::open_at(&path).unwrap());
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch("PRAGMA auto_vacuum=NONE; VACUUM;")
            .unwrap();
        drop(conn);
        let db = Db::open_at(&path).unwrap();
        (dir, db)
    }

    fn temp_lock(dir: &tempfile::TempDir) -> impl FnOnce() -> Result<std::fs::File> + '_ {
        move || {
            crate::db_maintain::maintenance_lock_at_within(
                &dir.path().join("locks/db-maintain.lock"),
                Duration::from_millis(50),
            )
        }
    }

    #[test]
    fn vacuum_fails_when_another_thegn_holds_the_maintenance_lock() {
        let (dir, db) = fixture();
        seed_gone_semantic_row(&db);
        let holder = crate::db_maintain::maintenance_lock_at_within(
            &dir.path().join("locks/db-maintain.lock"),
            Duration::ZERO,
        )
        .unwrap();
        let error =
            vacuum_report_with(&db, false, temp_lock(&dir), HostRoom::default()).unwrap_err();
        assert_eq!(
            error.to_string(),
            "another thegn is maintaining the state database; retry"
        );
        // Nothing was pruned while locked out.
        assert_eq!(db.entities_under("/gone/wt").unwrap().len(), 1);
        drop(holder);
        assert!(vacuum_report_with(&db, false, temp_lock(&dir), HostRoom::default()).is_ok());
        // A dry run never needs the lock.
        let _held = crate::db_maintain::maintenance_lock_at_within(
            &dir.path().join("locks/db-maintain.lock"),
            Duration::ZERO,
        )
        .unwrap();
        assert!(vacuum_report_with(&db, true, temp_lock(&dir), HostRoom::default()).is_ok());
    }

    #[test]
    fn vacuum_refuses_conversion_on_low_disk_and_reports_it_in_json() {
        let (dir, db) = legacy_fixture();
        let short = HostRoom {
            db_fs_free: Some(1),
            tmp_free: None,
        };
        let report = vacuum_report_with(&db, false, temp_lock(&dir), short).unwrap();
        assert_eq!(report.mode_after, AutoVacuum::None);
        let need = thegn_core::conversion_needs_bytes(&report.before);
        assert_eq!(
            report.refused,
            Some(Refusal {
                reason: "low_disk",
                need_bytes: need,
                free_bytes: 1,
            })
        );
        let encoded = serde_json::to_value(&report).unwrap();
        assert_eq!(encoded["refused"]["reason"], "low_disk");
        assert_eq!(encoded["refused"]["need_bytes"], need);
        assert_eq!(encoded["refused"]["free_bytes"], 1);
        assert_eq!(encoded["mode_after"], "none");

        // With room it converts and the field is absent.
        let report = vacuum_report_with(&db, false, temp_lock(&dir), HostRoom::default()).unwrap();
        assert_eq!(report.mode_after, AutoVacuum::Incremental);
        assert!(
            serde_json::to_value(&report)
                .unwrap()
                .get("refused")
                .is_none()
        );
    }

    #[test]
    fn manual_conversion_checks_room_above_the_auto_ceiling() {
        let big = DbSpace {
            page_size: 4096,
            page_count: 300 * 256,
            freelist_count: 0,
            auto_vacuum: AutoVacuum::None,
        };
        assert_eq!(conversion_shortfall(&big, HostRoom::default()), None);
        let short = HostRoom {
            db_fs_free: None,
            tmp_free: Some(MIB),
        };
        assert_eq!(
            conversion_shortfall(&big, short),
            Some((big.live_bytes(), MIB))
        );
    }

    #[test]
    fn plan_name_covers_low_disk() {
        assert_eq!(
            plan_name(MaintenanceStep::LowDisk {
                need_bytes: 3 * MIB,
                free_bytes: MIB,
            }),
            "no conversion: needs 3 MB free, 1 MB available"
        );
    }

    #[test]
    fn report_retention_uses_the_core_policy() {
        assert_eq!(NOTIFICATION_READ_RETENTION_SECS, 30 * 24 * 60 * 60);
    }
}
