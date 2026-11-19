//! Bounded database maintenance on the existing background disk worker.
//!
//! Only the UI runs this: [`run_round`] is reachable solely from
//! `measure::disk::spawn_scan`, a background worker that never runs on the
//! event loop. The daemon never calls any of it. Two UIs never both maintain:
//! a pass runs under the exclusive `$XDG_STATE_HOME/thegn/locks/db-maintain.lock`,
//! which `thegn db vacuum` also takes.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use termwiz::terminal::TerminalWaker;
use thegn_core::db::Db;
use thegn_core::{
    DbSpace, HostRoom, INCREMENTAL_PAGES_PER_ROUND, INCREMENTAL_STEPS_PER_PASS, MaintenanceStep,
    VacuumError, plan_maintenance,
};

const MAX_AUTO_ATTEMPTS: u32 = 3;
const MIB: u64 = 1024 * 1024;
const PRUNE_SLICES_PER_ROUND: usize = 8;
const PRUNE_SLICE_ROWS: usize = 512;
const FIRST_PASS_DELAY: Duration = Duration::from_secs(120);
const PASS_INTERVAL: Duration = Duration::from_secs(600);
const SEMANTIC_REST: Duration = Duration::from_secs(6 * 3600);
const STEP_SLOW_MS: u64 = 250;
const BIG_RECLAIM_BYTES: u64 = 64 * MIB;
const LOCK_RETRY: Duration = Duration::from_millis(100);

struct NoticeTarget {
    tx: tokio::sync::mpsc::UnboundedSender<crate::hydrate::RefreshKind>,
    waker: Option<TerminalWaker>,
}

fn notice_target() -> &'static Mutex<Option<NoticeTarget>> {
    static TARGET: OnceLock<Mutex<Option<NoticeTarget>>> = OnceLock::new();
    TARGET.get_or_init(|| Mutex::new(None))
}

/// Process-wide maintenance state. Instants are milliseconds on the
/// process-monotonic clock ([`mono_now_ms`]); tests inject their own.
struct PassState {
    /// `0` = not armed; otherwise the arm instant plus one.
    armed_at_ms: AtomicU64,
    /// No pass starts before this instant.
    next_pass_at_ms: AtomicU64,
    /// Automatic conversion attempts used by this process.
    attempts: AtomicU32,
    too_large_noted: AtomicBool,
    low_disk_noted: AtomicBool,
    busy_noted: AtomicBool,
    prune_cursor: AtomicI64,
    /// The semantic prune rests until this instant after a full wrap.
    semantic_rest_until_ms: AtomicU64,
}

impl PassState {
    const fn new() -> Self {
        Self {
            armed_at_ms: AtomicU64::new(0),
            next_pass_at_ms: AtomicU64::new(0),
            attempts: AtomicU32::new(0),
            too_large_noted: AtomicBool::new(false),
            low_disk_noted: AtomicBool::new(false),
            busy_noted: AtomicBool::new(false),
            prune_cursor: AtomicI64::new(0),
            semantic_rest_until_ms: AtomicU64::new(0),
        }
    }

    /// Record the first arm instant; later calls keep it.
    fn arm(&self, now_ms: u64) {
        // `Err` = already armed: the first frame's instant stands.
        if self
            .armed_at_ms
            .compare_exchange(
                0,
                now_ms.saturating_add(1),
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            tracing::trace!(target: "thegn::db", "database maintenance already armed");
        }
    }

    /// Armed, at least [`FIRST_PASS_DELAY`] after arming, and not before the
    /// next scheduled pass.
    fn pass_due(&self, now_ms: u64) -> bool {
        let armed = self.armed_at_ms.load(Ordering::Acquire);
        if armed == 0 {
            return false;
        }
        let first = (armed - 1).saturating_add(duration_ms(FIRST_PASS_DELAY));
        now_ms >= first && now_ms >= self.next_pass_at_ms.load(Ordering::Acquire)
    }

    fn schedule_next(&self, now_ms: u64) {
        self.next_pass_at_ms.store(
            now_ms.saturating_add(duration_ms(PASS_INTERVAL)),
            Ordering::Release,
        );
    }

    fn attempts_left(&self) -> u32 {
        MAX_AUTO_ATTEMPTS.saturating_sub(self.attempts.load(Ordering::Acquire))
    }
}

static STATE: PassState = PassState::new();

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn mono_now_ms() -> u64 {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    duration_ms(EPOCH.get_or_init(Instant::now).elapsed())
}

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

/// Arm maintenance; the first pass waits [`FIRST_PASS_DELAY`] from here.
pub(crate) fn arm_after_first_frame() {
    STATE.arm(mono_now_ms());
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

fn maintenance_lock_path() -> std::path::PathBuf {
    thegn_core::util::xdg_state_home().join("thegn/locks/db-maintain.lock")
}

/// Open the lock file (creating it `0600` in an owner-only directory) and try
/// to take it exclusively. `Ok(None)` = another holder has it.
fn try_lock_at(path: &std::path::Path) -> std::io::Result<Option<std::fs::File>> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
        crate::platform::restrict_dir_owner_only(dir);
    }
    // Refuse a planted symlink: the lock must be a plain file we own.
    if std::fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink()) {
        return Err(std::io::Error::other(format!(
            "refusing symlinked maintenance lock {}",
            path.display()
        )));
    }
    let file = crate::platform::open_lock_file(path)?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(error)) => Err(error),
    }
}

/// Exclusive maintenance lock (`$XDG_STATE_HOME/thegn/locks/db-maintain.lock`,
/// created 0600 in an owner-only dir). `None` = another thegn holds it (or the
/// lock could not be opened, which also means "do not maintain now").
pub(crate) fn try_maintenance_lock() -> Option<std::fs::File> {
    match try_lock_at(&maintenance_lock_path()) {
        Ok(lock) => lock,
        Err(error) => {
            tracing::debug!(target: "thegn::db", %error, "could not open the database maintenance lock");
            None
        }
    }
}

/// Same as [`try_maintenance_lock`], waiting up to `wait` (the CLI path).
pub(crate) fn maintenance_lock_within(wait: Duration) -> anyhow::Result<std::fs::File> {
    maintenance_lock_at_within(&maintenance_lock_path(), wait)
}

pub(crate) fn maintenance_lock_at_within(
    path: &std::path::Path,
    wait: Duration,
) -> anyhow::Result<std::fs::File> {
    let deadline = Instant::now() + wait;
    loop {
        if let Some(lock) = try_lock_at(path)? {
            return Ok(lock);
        }
        let now = Instant::now();
        if now >= deadline {
            anyhow::bail!("another thegn is maintaining the state database; retry");
        }
        std::thread::sleep(LOCK_RETRY.min(deadline - now));
    }
}

/// Measure free space on the DB filesystem and the temp dir
/// (`thegn_metrics::disk_space`; `None` = unknown, never blocking).
pub(crate) fn host_room(db_path: &std::path::Path) -> HostRoom {
    let free = |path: &std::path::Path| thegn_metrics::disk_space(path).map(|(_, avail, _)| avail);
    HostRoom {
        db_fs_free: free(db_path),
        tmp_free: free(&std::env::temp_dir()),
    }
}

fn room_for(db: &Db) -> HostRoom {
    db.file_path()
        .map(|path| host_room(&path))
        .unwrap_or_default()
}

/// Run one opportunistic pass when due. Called only by the background disk worker.
/// This function is intentionally reachable only from `measure::disk::spawn_scan`; that
/// scheduler callback is a background worker and never runs on the event loop.
pub(crate) fn run_round(db: &Db) {
    run_round_with(
        db,
        &STATE,
        mono_now_ms(),
        try_maintenance_lock,
        || room_for(db),
        Duration::ZERO,
    );
}

/// Gate, lock, then one pass. Returns whether a pass ran.
fn run_round_with(
    db: &Db,
    state: &PassState,
    now_ms: u64,
    lock: impl FnOnce() -> Option<std::fs::File>,
    room: impl FnOnce() -> HostRoom,
    busy: Duration,
) -> bool {
    if !state.pass_due(now_ms) {
        return false;
    }
    // Schedule first: a held lock or a failed pass defers one interval.
    state.schedule_next(now_ms);
    let Some(_lock) = lock() else {
        tracing::debug!(target: "thegn::db", "another thegn holds the maintenance lock; deferring");
        return false;
    };
    run_pass(db, state, now_ms, room(), busy);
    true
}

fn run_pass(db: &Db, state: &PassState, now_ms: u64, room: HostRoom, busy: Duration) {
    // 1. Semantic/cache prune, resting after a full wrap.
    if now_ms >= state.semantic_rest_until_ms.load(Ordering::Acquire)
        && prune_round(db, &state.prune_cursor, PRUNE_SLICES_PER_ROUND)
    {
        state.semantic_rest_until_ms.store(
            now_ms.saturating_add(duration_ms(SEMANTIC_REST)),
            Ordering::Release,
        );
    }
    // 2. State-file retention.
    let gc = crate::state_gc::run_pass(thegn_core::util::now());
    if gc != crate::state_gc::GcReport::default() {
        tracing::debug!(target: "thegn::db", pty_notes = gc.pty_notes, db_copies = gc.db_copies, "state retention pass");
    }
    // Bounded row retention runs once per pass, before planning, so a
    // conversion compacts the already-pruned rows and an incremental drain
    // sees the pages the prunes freed.
    retention(db, thegn_core::util::now());
    // 3. Plan.
    let space = match db.space() {
        Ok(space) => space,
        Err(error) => {
            tracing::warn!(%error, "could not inspect state database space");
            return;
        }
    };
    let step = plan_maintenance(&space, room, state.attempts_left());
    apply_step(db, state, space, step, room, busy);
}

/// Steps 4-7 of a pass: the one reclamation step the planner chose.
fn apply_step(
    db: &Db,
    state: &PassState,
    space: DbSpace,
    step: MaintenanceStep,
    room: HostRoom,
    busy: Duration,
) {
    match step {
        MaintenanceStep::Convert => {
            convert_round(db, space, state, busy, |db, busy| {
                db.convert_to_incremental(busy)
            });
        }
        MaintenanceStep::Incremental { .. } => {
            drain_incremental(db, space, room, timed_incremental_step);
        }
        MaintenanceStep::LowDisk {
            need_bytes,
            free_bytes,
        } => {
            if !state.low_disk_noted.swap(true, Ordering::AcqRel) {
                toast(format!(
                    "State database not compacted: needs {} MB free, {} MB available; thegn retries automatically",
                    need_bytes.div_ceil(MIB),
                    free_bytes / MIB
                ));
                tracing::info!(target: "thegn::db", need_bytes, free_bytes, "state database conversion skipped: low disk");
            }
        }
        MaintenanceStep::TooLargeForAuto {
            live_bytes,
            free_bytes,
        } => {
            if !state.too_large_noted.swap(true, Ordering::AcqRel) {
                toast(format!(
                    "State database is {} MB ({} MB unused); run 'thegn db vacuum' while thegn is idle",
                    space.file_bytes().div_ceil(MIB),
                    free_bytes.div_ceil(MIB)
                ));
                tracing::info!(target: "thegn::db", live_bytes, free_bytes, "state database is too large for automatic conversion; run 'thegn db vacuum'");
            }
        }
        MaintenanceStep::Nothing => {}
    }
}

/// Bounded retention prunes (`Db::maintain` with no reclamation step).
fn retention(db: &Db, now_secs: i64) {
    match db.maintain(now_secs, MaintenanceStep::Nothing) {
        Ok(report)
            if report.pruned_notifications > 0
                || report.pruned_removed_notifications > 0
                || report.pruned_issue_links > 0
                || report.pruned_dispatches > 0
                || report.pruned_merge_events > 0
                || report.pruned_approval_effects > 0 =>
        {
            tracing::debug!(
                pruned_notifications = report.pruned_notifications,
                pruned_removed_notifications = report.pruned_removed_notifications,
                pruned_issue_links = report.pruned_issue_links,
                pruned_dispatches = report.pruned_dispatches,
                pruned_merge_events = report.pruned_merge_events,
                pruned_approval_effects = report.pruned_approval_effects,
                "state database retention completed"
            );
        }
        Ok(_) => {}
        Err(error) => tracing::warn!(%error, "state database retention failed"),
    }
}

fn timed_incremental_step(db: &Db, pages: u32) -> (anyhow::Result<u64>, Duration) {
    let started = Instant::now();
    let result = db.incremental_vacuum(pages);
    (result, started.elapsed())
}

/// Why an incremental drain stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DrainStop {
    /// [`INCREMENTAL_STEPS_PER_PASS`] steps ran.
    StepCap,
    /// A step freed nothing.
    Exhausted,
    /// A step took longer than [`STEP_SLOW_MS`].
    Slow,
    /// Another connection held the lock.
    Busy,
    /// Free space fell under the planner's threshold.
    BelowThreshold,
    /// Any other error.
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DrainOutcome {
    steps: u32,
    freed_pages: u64,
    stop: DrainStop,
}

/// Up to [`INCREMENTAL_STEPS_PER_PASS`] incremental-vacuum steps, then a WAL
/// truncate when anything was freed.
fn drain_incremental(
    db: &Db,
    before: DbSpace,
    room: HostRoom,
    mut step: impl FnMut(&Db, u32) -> (anyhow::Result<u64>, Duration),
) -> DrainOutcome {
    let mut outcome = DrainOutcome {
        steps: 0,
        freed_pages: 0,
        stop: DrainStop::StepCap,
    };
    while outcome.steps < INCREMENTAL_STEPS_PER_PASS {
        let (result, elapsed) = step(db, INCREMENTAL_PAGES_PER_ROUND);
        outcome.steps += 1;
        match result {
            Ok(0) => {
                outcome.stop = DrainStop::Exhausted;
                break;
            }
            Ok(freed) => outcome.freed_pages += freed,
            Err(error)
                if error
                    .downcast_ref::<VacuumError>()
                    .is_some_and(|e| matches!(e, VacuumError::Busy)) =>
            {
                tracing::debug!(target: "thegn::db", "incremental vacuum busy; next pass retries");
                outcome.stop = DrainStop::Busy;
                break;
            }
            Err(error) => {
                tracing::warn!(target: "thegn::db", %error, "incremental vacuum step failed");
                outcome.stop = DrainStop::Failed;
                break;
            }
        }
        if duration_ms(elapsed) > STEP_SLOW_MS {
            outcome.stop = DrainStop::Slow;
            break;
        }
        match db.space() {
            Ok(space)
                if matches!(
                    plan_maintenance(&space, room, 0),
                    MaintenanceStep::Incremental { .. }
                ) => {}
            Ok(_) => {
                outcome.stop = DrainStop::BelowThreshold;
                break;
            }
            Err(error) => {
                tracing::warn!(target: "thegn::db", %error, "could not inspect state database space");
                outcome.stop = DrainStop::Failed;
                break;
            }
        }
    }
    if outcome.freed_pages > 0 {
        match db.checkpoint_truncate() {
            Ok(truncated) => {
                tracing::debug!(target: "thegn::db", truncated, "WAL checkpoint after incremental vacuum");
            }
            Err(error) => tracing::debug!(target: "thegn::db", %error, "WAL checkpoint failed"),
        }
        if let Ok(after) = db.space() {
            let shrank = before.file_bytes().saturating_sub(after.file_bytes());
            if shrank >= BIG_RECLAIM_BYTES {
                toast(format!(
                    "State database compacted: {} MB -> {} MB",
                    before.file_bytes().div_ceil(MIB),
                    after.file_bytes().div_ceil(MIB)
                ));
            }
        }
        tracing::debug!(target: "thegn::db", steps = outcome.steps, freed_pages = outcome.freed_pages, stop = ?outcome.stop, "incremental vacuum pass");
    }
    outcome
}

/// Walk up to `slices` semantic/cache prune slices. Returns whether the cursor
/// wrapped (a slice scanned nothing), i.e. a full sweep finished.
fn prune_round(db: &Db, cursor: &AtomicI64, slices: usize) -> bool {
    let mut scanned_entities = 0;
    let mut completed_slices = 0;
    let mut wrapped = false;
    for _ in 0..slices {
        let after_rowid = cursor.load(Ordering::Acquire);
        match db.prune_stale_cache_slice(after_rowid, PRUNE_SLICE_ROWS) {
            Ok((scanned, next_rowid)) => {
                completed_slices += 1;
                scanned_entities += scanned;
                if scanned == 0 {
                    cursor.store(0, Ordering::Release);
                    wrapped = true;
                    break;
                }
                cursor.store(next_rowid, Ordering::Release);
            }
            Err(error) => {
                tracing::warn!(target: "thegn::db", %error, "stale cache prune step failed");
                break;
            }
        }
    }
    if completed_slices > 0 {
        tracing::debug!(
            target: "thegn::db",
            slices = completed_slices,
            scanned_entities,
            wrapped,
            "stale cache prune step completed"
        );
    }
    wrapped
}

fn convert_round(
    db: &Db,
    space: DbSpace,
    state: &PassState,
    busy: Duration,
    convert: impl FnOnce(&Db, Duration) -> anyhow::Result<thegn_core::ConvertOutcome>,
) {
    let attempt =
        match state
            .attempts
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                (current < MAX_AUTO_ATTEMPTS).then_some(current + 1)
            }) {
            Ok(previous) => previous + 1,
            Err(_) => return,
        };
    let before_mb = space.file_bytes().div_ceil(MIB);
    // Announce only the first attempt: a busy retry every pass must not
    // become a toast storm.
    if attempt == 1 {
        toast(format!("Compacting the state database ({before_mb} MB)..."));
    }
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
            if !state.busy_noted.swap(true, Ordering::AcqRel) {
                toast("State database busy; compaction will retry later".into());
            }
            tracing::debug!(attempt, %error, "state database conversion is busy; will retry on a later pass");
        }
        Err(error) => {
            state.attempts.store(MAX_AUTO_ATTEMPTS, Ordering::Release);
            tracing::warn!(%error, "could not convert state database to incremental auto-vacuum");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::Builder;
    use thegn_core::AutoVacuum;
    use thegn_core::semantic::EntityKind;
    use thegn_core::store::{NotificationStore, SemEntityRow, SemanticStore};

    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// The instant a state armed at 0 first becomes due.
    const DUE: u64 = 120_000;
    const INTERVAL: u64 = 600_000;

    fn temp_db() -> (tempfile::TempDir, Db) {
        let dir = Builder::new().tempdir().unwrap();
        let path = dir.path().join("state.db");
        (dir, Db::open_at(&path).unwrap())
    }

    fn armed_state() -> PassState {
        let state = PassState::new();
        state.arm(0);
        state
    }

    fn free_lock() -> Option<std::fs::File> {
        tempfile::tempfile().ok()
    }

    fn round(db: &Db, state: &PassState, now_ms: u64) -> bool {
        run_round_with(
            db,
            state,
            now_ms,
            free_lock,
            HostRoom::default,
            Duration::ZERO,
        )
    }

    fn gone_entity(db: &Db, id: &str) {
        db.replace_file_entities(
            "/gone/wt/a.rs",
            &[SemEntityRow {
                id: id.into(),
                file: "/gone/wt/a.rs".into(),
                name: id.into(),
                kind: EntityKind::Function,
                start_line: 1,
                end_line: 1,
                source_hash: "hash".into(),
            }],
        )
        .unwrap();
    }

    /// An incremental DB with at least 8 MiB on its freelist.
    fn reclaimable_db() -> (tempfile::TempDir, Db) {
        let (dir, db) = temp_db();
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
        assert!(db.space().unwrap().free_bytes() >= 8 * MIB);
        (dir, db)
    }

    fn legacy_db(dir: &tempfile::TempDir) -> Db {
        let path = dir.path().join("state.db");
        drop(Db::open_at(&path).unwrap());
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "PRAGMA auto_vacuum=NONE; VACUUM;
             CREATE TABLE fixture(data BLOB);
             INSERT INTO fixture(data) VALUES(zeroblob(1048576));",
        )
        .unwrap();
        drop(conn);
        Db::open_at(&path).unwrap()
    }

    fn install_test_notice() -> tokio::sync::mpsc::UnboundedReceiver<crate::hydrate::RefreshKind> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        if let Ok(mut target) = notice_target().lock() {
            *target = Some(NoticeTarget { tx, waker: None });
        }
        rx
    }

    fn drain_toasts(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<crate::hydrate::RefreshKind>,
    ) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(kind) = rx.try_recv() {
            if let crate::hydrate::RefreshKind::Toast { message, .. } = kind {
                out.push(message);
            }
        }
        out
    }

    #[test]
    fn round_is_inert_until_armed() {
        let _guard = TEST_LOCK.lock().unwrap();
        let (_dir, db) = reclaimable_db();
        gone_entity(&db, "unarmed-entity");
        let before = db.space().unwrap();
        assert!(!round(&db, &PassState::new(), u64::MAX / 2));
        assert_eq!(db.space().unwrap().freelist_count, before.freelist_count);
        assert_eq!(db.entities_under("/gone/wt").unwrap().len(), 1);
    }

    #[test]
    fn gate_waits_for_the_first_delay_after_arm_then_the_interval() {
        let _guard = TEST_LOCK.lock().unwrap();
        let _state = crate::testenv::IsolatedState::new();
        let (_dir, db) = temp_db();
        let state = PassState::new();
        // Not before arm, however late.
        assert!(!state.pass_due(10 * DUE));
        state.arm(5_000);
        // A second arm keeps the first instant.
        state.arm(50_000);
        assert!(!round(&db, &state, 5_000 + DUE - 1));
        assert!(round(&db, &state, 5_000 + DUE));
        let first = 5_000 + DUE;
        assert!(!round(&db, &state, first + 1));
        assert!(!round(&db, &state, first + INTERVAL - 1));
        assert!(round(&db, &state, first + INTERVAL));
        assert_eq!(duration_ms(FIRST_PASS_DELAY), DUE);
        assert_eq!(duration_ms(PASS_INTERVAL), INTERVAL);
    }

    #[test]
    fn held_maintenance_lock_defers_the_pass_one_interval() {
        let _guard = TEST_LOCK.lock().unwrap();
        let _state = crate::testenv::IsolatedState::new();
        let (dir, db) = temp_db();
        gone_entity(&db, "locked-entity");
        let path = dir.path().join("locks/db-maintain.lock");
        let holder = try_lock_at(&path).unwrap().expect("first holder");
        let state = armed_state();
        let ran = run_round_with(
            &db,
            &state,
            DUE,
            || try_lock_at(&path).ok().flatten(),
            HostRoom::default,
            Duration::ZERO,
        );
        assert!(!ran);
        assert_eq!(db.entities_under("/gone/wt").unwrap().len(), 1);
        assert_eq!(
            state.next_pass_at_ms.load(Ordering::Acquire),
            DUE + INTERVAL
        );
        drop(holder);
        assert!(!round(&db, &state, DUE + INTERVAL - 1));
        assert!(round(&db, &state, DUE + INTERVAL));
        assert!(db.entities_under("/gone/wt").unwrap().is_empty());
    }

    #[test]
    fn maintenance_lock_within_times_out_with_the_retry_message() {
        let _guard = TEST_LOCK.lock().unwrap();
        let dir = Builder::new().tempdir().unwrap();
        let path = dir.path().join("locks/db-maintain.lock");
        let holder = try_lock_at(&path).unwrap().expect("first holder");
        let error = maintenance_lock_at_within(&path, Duration::from_millis(50)).unwrap_err();
        assert_eq!(
            error.to_string(),
            "another thegn is maintaining the state database; retry"
        );
        drop(holder);
        assert!(maintenance_lock_at_within(&path, Duration::from_millis(50)).is_ok());
        if let Some(mode) = thegn_core::fsperm::mode_bits(&path).unwrap() {
            assert_eq!(mode & 0o077, 0, "lock file must be owner-only");
        }
    }

    #[test]
    fn maintenance_lock_refuses_a_symlink() {
        let _guard = TEST_LOCK.lock().unwrap();
        let dir = Builder::new().tempdir().unwrap();
        let target = dir.path().join("elsewhere");
        std::fs::write(&target, b"").unwrap();
        let path = dir.path().join("db-maintain.lock");
        if crate::platform::symlink_file_for_test(&target, &path).is_err() {
            return;
        }
        assert!(try_lock_at(&path).is_err());
    }

    #[test]
    fn armed_round_prunes_orphan_semantic_rows() {
        let _guard = TEST_LOCK.lock().unwrap();
        let _state = crate::testenv::IsolatedState::new();
        let (_dir, db) = temp_db();
        gone_entity(&db, "armed-entity");
        assert!(round(&db, &armed_state(), DUE));
        assert!(db.entities_under("/gone/wt").unwrap().is_empty());
    }

    #[test]
    fn semantic_prune_rests_after_a_wrap() {
        let _guard = TEST_LOCK.lock().unwrap();
        let _state = crate::testenv::IsolatedState::new();
        let (_dir, db) = temp_db();
        let state = armed_state();
        // An empty table wraps on the first slice.
        run_pass(&db, &state, 1_000, HostRoom::default(), Duration::ZERO);
        let rest = duration_ms(SEMANTIC_REST);
        assert_eq!(
            state.semantic_rest_until_ms.load(Ordering::Acquire),
            1_000 + rest
        );
        gone_entity(&db, "resting-entity");
        run_pass(
            &db,
            &state,
            1_000 + rest - 1,
            HostRoom::default(),
            Duration::ZERO,
        );
        assert_eq!(db.entities_under("/gone/wt").unwrap().len(), 1);
        run_pass(
            &db,
            &state,
            1_000 + rest,
            HostRoom::default(),
            Duration::ZERO,
        );
        assert!(db.entities_under("/gone/wt").unwrap().is_empty());
    }

    #[test]
    fn prune_round_resets_cursor_at_terminal_and_bounds_slices() {
        let _guard = TEST_LOCK.lock().unwrap();
        let (_dir, db) = temp_db();
        let entities: Vec<SemEntityRow> = (0..=PRUNE_SLICE_ROWS)
            .map(|index| SemEntityRow {
                id: format!("entity-{index}"),
                file: "/gone/wt/a.rs".into(),
                name: format!("item-{index}"),
                kind: EntityKind::Function,
                start_line: 1,
                end_line: 1,
                source_hash: "hash".into(),
            })
            .collect();
        db.replace_file_entities("/gone/wt/a.rs", &entities)
            .unwrap();
        let cursor = AtomicI64::new(0);

        assert!(!prune_round(&db, &cursor, 1));
        assert!(cursor.load(Ordering::Acquire) > 0);
        assert_eq!(db.entities_under("/gone/wt").unwrap().len(), 1);

        let mut wrapped = false;
        for _ in 0..3 {
            if prune_round(&db, &cursor, 1) {
                wrapped = true;
                break;
            }
        }

        assert!(wrapped);
        assert_eq!(cursor.load(Ordering::Acquire), 0);
        assert!(db.entities_under("/gone/wt").unwrap().is_empty());
    }

    #[test]
    fn incremental_drain_stops_on_a_slow_step() {
        let _guard = TEST_LOCK.lock().unwrap();
        let (_dir, db) = reclaimable_db();
        let space = db.space().unwrap();
        let outcome = drain_incremental(&db, space, HostRoom::default(), |_, _| {
            (Ok(1), Duration::from_millis(STEP_SLOW_MS + 1))
        });
        assert_eq!(outcome.steps, 1);
        assert_eq!(outcome.stop, DrainStop::Slow);
    }

    #[test]
    fn incremental_drain_stops_on_busy_and_on_exhaustion() {
        let _guard = TEST_LOCK.lock().unwrap();
        let (_dir, db) = reclaimable_db();
        let space = db.space().unwrap();
        let busy = drain_incremental(&db, space, HostRoom::default(), |_, _| {
            (Err(VacuumError::Busy.into()), Duration::ZERO)
        });
        assert_eq!((busy.steps, busy.stop), (1, DrainStop::Busy));
        let empty = drain_incremental(&db, space, HostRoom::default(), |_, _| {
            (Ok(0), Duration::ZERO)
        });
        assert_eq!((empty.steps, empty.stop), (1, DrainStop::Exhausted));
    }

    #[test]
    fn incremental_drain_is_capped_per_pass() {
        let _guard = TEST_LOCK.lock().unwrap();
        let (_dir, db) = reclaimable_db();
        let space = db.space().unwrap();
        // The stub frees nothing for real, so the planner keeps saying
        // Incremental and only the step cap stops the loop.
        let outcome = drain_incremental(&db, space, HostRoom::default(), |_, _| {
            (Ok(1), Duration::ZERO)
        });
        assert_eq!(outcome.steps, INCREMENTAL_STEPS_PER_PASS);
        assert_eq!(outcome.stop, DrainStop::StepCap);
    }

    #[test]
    fn incremental_drain_reclaims_until_below_threshold_and_truncates_the_wal() {
        let _guard = TEST_LOCK.lock().unwrap();
        let (dir, db) = reclaimable_db();
        let before = db.space().unwrap();
        let outcome = drain_incremental(&db, before, HostRoom::default(), timed_incremental_step);
        assert!(outcome.freed_pages > 0);
        assert!(matches!(
            outcome.stop,
            DrainStop::Exhausted | DrainStop::BelowThreshold | DrainStop::Slow
        ));
        assert!(db.space().unwrap().freelist_count < before.freelist_count);
        let wal = dir.path().join("state.db-wal");
        if let Ok(meta) = std::fs::metadata(&wal) {
            assert_eq!(meta.len(), 0, "WAL truncated after the drain");
        }
    }

    #[test]
    fn busy_legacy_conversion_counts_attempts_without_a_toast_storm() {
        let _guard = TEST_LOCK.lock().unwrap();
        let dir = Builder::new().tempdir().unwrap();
        let db = legacy_db(&dir);
        let mut rx = install_test_notice();
        let locker = rusqlite::Connection::open(dir.path().join("state.db")).unwrap();
        locker.execute_batch("BEGIN IMMEDIATE").unwrap();
        let state = armed_state();
        for _ in 0..(MAX_AUTO_ATTEMPTS + 2) {
            let space = db.space().unwrap();
            assert_eq!(space.auto_vacuum, AutoVacuum::None);
            assert!(space.live_bytes() >= MIB);
            let step = plan_maintenance(&space, HostRoom::default(), state.attempts_left());
            apply_step(
                &db,
                &state,
                space,
                step,
                HostRoom::default(),
                Duration::ZERO,
            );
        }
        locker.execute_batch("ROLLBACK").unwrap();
        assert_eq!(state.attempts.load(Ordering::Acquire), MAX_AUTO_ATTEMPTS);
        assert_eq!(db.space().unwrap().auto_vacuum, AutoVacuum::None);
        let toasts = drain_toasts(&mut rx);
        assert_eq!(toasts.len(), 2, "one start and one busy toast: {toasts:?}");
    }

    #[test]
    fn low_disk_and_too_large_toast_once_per_process() {
        let _guard = TEST_LOCK.lock().unwrap();
        let (_dir, db) = temp_db();
        let mut rx = install_test_notice();
        let state = armed_state();
        let space = db.space().unwrap();
        let low = MaintenanceStep::LowDisk {
            need_bytes: 100 * MIB,
            free_bytes: 10 * MIB,
        };
        let large = MaintenanceStep::TooLargeForAuto {
            live_bytes: 300 * MIB,
            free_bytes: 5 * MIB,
        };
        for _ in 0..3 {
            apply_step(&db, &state, space, low, HostRoom::default(), Duration::ZERO);
            apply_step(
                &db,
                &state,
                space,
                large,
                HostRoom::default(),
                Duration::ZERO,
            );
        }
        let toasts = drain_toasts(&mut rx);
        assert_eq!(toasts.len(), 2, "{toasts:?}");
        assert_eq!(
            toasts[0],
            "State database not compacted: needs 100 MB free, 10 MB available; thegn retries automatically"
        );
        assert!(toasts[1].ends_with("(5 MB unused); run 'thegn db vacuum' while thegn is idle"));
        assert_eq!(state.attempts.load(Ordering::Acquire), 0);
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
        let state = PassState::new();
        let convert = |_: &Db, _: Duration| -> anyhow::Result<thegn_core::ConvertOutcome> {
            Err(VacuumError::Busy.into())
        };
        for _ in 0..MAX_AUTO_ATTEMPTS {
            convert_round(&db, space, &state, Duration::ZERO, convert);
        }
        convert_round(&db, space, &state, Duration::ZERO, convert);
        assert_eq!(state.attempts.load(Ordering::Acquire), MAX_AUTO_ATTEMPTS);
        assert_eq!(state.attempts_left(), 0);
    }

    #[test]
    fn convert_round_sends_start_and_done_toasts() {
        let _guard = TEST_LOCK.lock().unwrap();
        let (_dir, db) = temp_db();
        let before = db.space().unwrap();
        let mut rx = install_test_notice();
        convert_round(
            &db,
            before,
            &PassState::new(),
            Duration::from_millis(20),
            |db, _busy| {
                let report = db.maintain(thegn_core::util::now(), MaintenanceStep::Convert)?;
                Ok(report.conversion.unwrap())
            },
        );
        assert_eq!(drain_toasts(&mut rx).len(), 2);
    }

    #[test]
    fn armed_round_prunes_removed_worktree_read_notifications() {
        let _guard = TEST_LOCK.lock().unwrap();
        let _state = crate::testenv::IsolatedState::new();
        let (dir, db) = temp_db();
        let gone = dir.path().join("gone").to_string_lossy().into_owned();
        let read_id = db
            .put_notification("fixture", "read", "read", &gone)
            .unwrap();
        let unread_id = db
            .put_notification("fixture", "unread", "unread", &gone)
            .unwrap();
        db.mark_notification_read(read_id).unwrap();
        assert!(!round(&db, &PassState::new(), DUE));
        assert_eq!(db.get_all_notifications(10).unwrap().len(), 2);

        assert!(round(&db, &armed_state(), DUE));
        let after = db.get_all_notifications(10).unwrap();
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].id, unread_id);
    }

    #[test]
    fn host_room_measures_existing_paths() {
        let dir = Builder::new().tempdir().unwrap();
        let room = host_room(&dir.path().join("state.db"));
        if cfg!(any(target_os = "linux", target_os = "macos")) {
            assert!(room.db_fs_free.is_some());
            assert!(room.tmp_free.is_some());
        }
    }
}
