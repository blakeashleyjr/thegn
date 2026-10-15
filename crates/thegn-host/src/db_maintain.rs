//! Bounded database maintenance on the existing background disk worker.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use termwiz::terminal::TerminalWaker;
use thegn_core::db::Db;
use thegn_core::{MaintenanceStep, VacuumError, plan_maintenance};

const MAX_AUTO_ATTEMPTS: u32 = 3;
const MIB: u64 = 1024 * 1024;

struct NoticeTarget {
    tx: tokio::sync::mpsc::UnboundedSender<crate::hydrate::RefreshKind>,
    waker: Option<TerminalWaker>,
}

fn notice_target() -> &'static Mutex<Option<NoticeTarget>> {
    static TARGET: OnceLock<Mutex<Option<NoticeTarget>>> = OnceLock::new();
    TARGET.get_or_init(|| Mutex::new(None))
}

static ARMED: AtomicBool = AtomicBool::new(false);
static AUTO_ATTEMPTS: AtomicU32 = AtomicU32::new(0);
static TOO_LARGE_NOTED: AtomicBool = AtomicBool::new(false);

pub(crate) fn install_notice(
    tx: tokio::sync::mpsc::UnboundedSender<crate::hydrate::RefreshKind>,
    waker: TerminalWaker,
) {
    if let Ok(mut target) = notice_target().lock() {
        *target = Some(NoticeTarget {
            tx,
            waker: Some(waker),
        });
    }
}

pub(crate) fn arm_after_first_frame() {
    ARMED.store(true, Ordering::Release);
}

fn toast(message: String) {
    if let Ok(target) = notice_target().lock()
        && let Some(target) = target.as_ref()
    {
        if let Err(error) = target.tx.send(crate::hydrate::RefreshKind::Toast {
            message,
            priority: thegn_core::notification::Priority::Info,
        }) {
            tracing::debug!(%error, "failed to send database maintenance toast");
        }
        if let Some(waker) = &target.waker
            && let Err(error) = waker.wake()
        {
            tracing::debug!(%error, "failed to wake after database maintenance toast");
        }
    }
}

/// Run one opportunistic step. Called only by the background disk worker.
/// This function is intentionally reachable only from `measure::disk::spawn_scan`; that
/// scheduler callback is a background worker and never runs on the event loop.
pub(crate) fn run_round(db: &Db) {
    run_round_with(
        db,
        ARMED.load(Ordering::Acquire),
        &AUTO_ATTEMPTS,
        &TOO_LARGE_NOTED,
        Duration::from_secs(5),
    );
}

fn run_round_with(
    db: &Db,
    armed: bool,
    attempts: &AtomicU32,
    too_large_noted: &AtomicBool,
    busy: Duration,
) {
    if !armed {
        return;
    }
    let space = match db.space() {
        Ok(space) => space,
        Err(error) => {
            tracing::warn!(%error, "could not inspect state database space");
            return;
        }
    };
    let left = MAX_AUTO_ATTEMPTS.saturating_sub(attempts.load(Ordering::Acquire));
    let step = plan_maintenance(&space, left);
    if let MaintenanceStep::TooLargeForAuto {
        live_bytes,
        free_bytes,
    } = step
        && !too_large_noted.swap(true, Ordering::AcqRel)
    {
        tracing::info!(target: "thegn::db", live_bytes, free_bytes, "state database is too large for automatic conversion; run 'thegn db vacuum'");
    }
    if step == MaintenanceStep::Convert {
        convert_round(db, space, attempts, busy, |db, busy| {
            let pruned = db.prune_notifications(thegn_core::NOTIFICATION_READ_RETENTION_SECS)?;
            if pruned > 0 {
                tracing::debug!(rows = pruned, "pruned expired read notifications");
            }
            db.convert_to_incremental(busy)
        });
        return;
    }
    match db.maintain(thegn_core::util::now(), step) {
        Ok(report) if report.pruned_notifications > 0 || report.reclaimed_pages > 0 => {
            tracing::debug!(
                pruned_notifications = report.pruned_notifications,
                reclaimed_pages = report.reclaimed_pages,
                "state database maintenance completed"
            );
        }
        Ok(_) => {}
        Err(error) => tracing::warn!(%error, "state database maintenance failed"),
    }
}

fn convert_round(
    db: &Db,
    space: thegn_core::DbSpace,
    attempts: &AtomicU32,
    busy: Duration,
    convert: impl FnOnce(&Db, Duration) -> anyhow::Result<thegn_core::ConvertOutcome>,
) {
    let attempt = match attempts.fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
        (current < MAX_AUTO_ATTEMPTS).then_some(current + 1)
    }) {
        Ok(previous) => previous + 1,
        Err(_) => return,
    };
    let before_mb = space.file_bytes().div_ceil(MIB);
    toast(format!("Compacting the state database ({before_mb} MB)..."));
    match convert(db, busy) {
        Ok(outcome) => {
            let after_mb = outcome.after.file_bytes().div_ceil(MIB);
            toast(format!(
                "State database compacted: {before_mb} MB -> {after_mb} MB"
            ));
            tracing::info!(
                before_bytes = outcome.before.file_bytes(),
                after_bytes = outcome.after.file_bytes(),
                "converted state database to incremental auto-vacuum"
            );
        }
        Err(error)
            if error
                .downcast_ref::<VacuumError>()
                .is_some_and(|e| matches!(e, VacuumError::Busy)) =>
        {
            toast("State database busy; compaction will retry later".into());
            tracing::debug!(attempt, %error, "state database conversion is busy; will retry on a later disk round");
        }
        Err(error) => {
            attempts.store(MAX_AUTO_ATTEMPTS, Ordering::Release);
            tracing::warn!(%error, "could not convert state database to incremental auto-vacuum");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use tempfile::Builder;
    use thegn_core::AutoVacuum;
    use thegn_core::store::NotificationStore;

    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn temp_db() -> (tempfile::TempDir, Db) {
        let dir = Builder::new().tempdir().unwrap();
        let path = dir.path().join("state.db");
        (dir, Db::open_at(&path).unwrap())
    }

    #[test]
    fn round_is_inert_until_armed() {
        let _guard = TEST_LOCK.lock().unwrap();
        let (_dir, db) = temp_db();
        let message = "x".repeat(8192);
        let ids: Vec<i64> = (0..1100)
            .map(|index| {
                db.put_notification("fixture", &index.to_string(), &message, "")
                    .unwrap()
            })
            .collect();
        for id in ids {
            db.delete_notification(id).unwrap();
        }
        let before = db.space().unwrap();
        assert!(before.free_bytes() >= 8 * MIB);
        run_round_with(
            &db,
            false,
            &AtomicU32::new(0),
            &AtomicBool::new(false),
            Duration::from_millis(20),
        );
        assert_eq!(db.space().unwrap().freelist_count, before.freelist_count);
    }

    #[test]
    fn busy_conversion_is_retried_at_most_three_times() {
        let _guard = TEST_LOCK.lock().unwrap();
        let (_dir, db) = temp_db();
        let space = thegn_core::DbSpace {
            page_size: 4096,
            page_count: 1,
            freelist_count: 0,
            auto_vacuum: AutoVacuum::None,
        };
        let attempts = AtomicU32::new(0);
        let convert = |_: &Db, _: Duration| -> anyhow::Result<thegn_core::ConvertOutcome> {
            Err(VacuumError::Busy.into())
        };
        for _ in 0..MAX_AUTO_ATTEMPTS {
            convert_round(&db, space, &attempts, Duration::from_millis(20), convert);
        }
        convert_round(&db, space, &attempts, Duration::from_millis(20), convert);
        assert_eq!(attempts.load(Ordering::Acquire), MAX_AUTO_ATTEMPTS);
    }

    #[test]
    fn convert_round_sends_start_and_done_toasts() {
        let _guard = TEST_LOCK.lock().unwrap();
        let (_dir, db) = temp_db();
        let before = db.space().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        if let Ok(mut target) = notice_target().lock() {
            *target = Some(NoticeTarget { tx, waker: None });
        }
        convert_round(
            &db,
            before,
            &AtomicU32::new(0),
            Duration::from_millis(20),
            |db, _busy| {
                let report = db.maintain(thegn_core::util::now(), MaintenanceStep::Convert)?;
                Ok(report.conversion.unwrap())
            },
        );
        for _ in 0..2 {
            assert!(matches!(
                rx.try_recv(),
                Ok(crate::hydrate::RefreshKind::Toast {
                    priority: thegn_core::notification::Priority::Info,
                    ..
                })
            ));
        }
    }
}
