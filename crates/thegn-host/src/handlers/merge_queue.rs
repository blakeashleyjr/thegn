//! Merge-queue (fold-actor) loop glue: the off-loop spawners for the batch
//! fold (`spawn_fold`) and the agent-driven queue drain (`spawn_drive`), the
//! loop-side drains of their result channels, and the panel section's action
//! keys (`a/A/x/l/r/c/D`). Extracted from the ratchet-pinned `run.rs`.
//!
//! Threading contract: every spawner runs its git/DB/agent work on
//! `spawn_blocking` and reports back on a tokio mpsc channel **plus a waker
//! pulse**; the `drain_*` functions run ON the loop and are I/O-free (inbox
//! records are themselves written on `spawn_blocking`).
//!
//! Quit-mid-drain: the fixing agent runs in its own process group with a
//! plain-thread watchdog, so if thegn exits the agent is orphaned (it keeps
//! running, unsupervised) and the queue row is left at a transient status
//! (`folding`/`agent_running`). That is accepted: re-adding or retrying the
//! row resets it to `queued` (the enqueue upsert), and hydration never
//! auto-resets transient rows because a concurrent CLI `merge drain`
//! legitimately owns them.

use std::path::{Path, PathBuf};

use termwiz::terminal::TerminalWaker;
use thegn_core::config::MergeQueueConfig;
use thegn_core::db::Db;
use thegn_core::merge_lifecycle::LifecycleEvent;
use thegn_core::notification::NotificationKind;
use thegn_core::store::WorktreeAuxStore;
use thegn_core::util;
use tokio::sync::mpsc as tokio_mpsc;

use crate::hydrate::RefreshKind;
use crate::integrate::{self, AttemptOutcome, FoldReport};
use crate::merge_driver::{self, DriveOutcome, QueueItem};
use crate::toast::{ToastKind, Toasts};

pub(crate) type DriveTx = tokio_mpsc::UnboundedSender<DriveMsg>;
pub(crate) type DriveRx = tokio_mpsc::UnboundedReceiver<DriveMsg>;
pub(crate) type FoldTx = tokio_mpsc::UnboundedSender<anyhow::Result<FoldReport>>;
pub(crate) type FoldRx = tokio_mpsc::UnboundedReceiver<anyhow::Result<FoldReport>>;

/// What the off-loop drive (or a one-shot queue mutation) reports back.
pub(crate) enum DriveMsg {
    /// One driver status transition (the DB row is already written when this
    /// fires) — the loop patches the panel row in place for a live repaint.
    Step {
        worktree: String,
        branch: String,
        status: String,
        detail: String,
    },
    /// The drain finished; clears the inflight flag and toasts the summary.
    Done(DriveOutcome),
    /// A one-line outcome from an off-loop queue mutation (add/land/…).
    Note(String),
    /// The drive (or a pre-drive step) failed outright.
    Failed(String),
}

/// The loop locals the channel drains mutate, borrowed for one drain pass.
pub(crate) struct DrainCtx<'a> {
    pub model: &'a mut crate::chrome::FrameModel,
    pub toasts: &'a mut Toasts,
    pub notify_state: &'a crate::notify::NotifyState,
    pub event_bus: &'a thegn_core::event_bus::EventBus,
    pub fold_inflight: &'a mut bool,
    pub want_model_refresh: &'a mut bool,
    pub dirty: &'a mut bool,
    pub loop_perf: &'a mut crate::perf::LoopPerf,
    // For the sidebar-folder lifecycle's `on_landed = remove/detach`: after a
    // land removes a worktree dir off-loop, reap the now-orphaned tab (panes +
    // session + focus) via `delete_groups`, exactly as a manual close does.
    pub session: &'a mut crate::session::Session,
    pub panes: &'a mut crate::panes::Panes,
    pub need_relayout: &'a mut bool,
    pub waker: &'a TerminalWaker,
}

impl DrainCtx<'_> {
    /// Reap any tab whose worktree dir vanished (an `on_landed = remove/detach`
    /// land). No-op when nothing was removed. Kept here so both drains share it.
    fn reap_removed_tabs(&mut self) {
        if crate::merge_lifecycle::reconcile_removed_tabs(self.session, self.panes, self.waker) {
            *self.need_relayout = true;
            *self.want_model_refresh = true;
        }
    }
}

// ---------------------------------------------------------------------------
// Off-loop spawners
// ---------------------------------------------------------------------------

/// Kick a one-shot batch fold (the `integrate` action) off the loop. The fold
/// does git plumbing plus an optional multi-second test-gate, so it must never
/// run on the loop; the result comes back on `fold_tx` and pulses the waker.
/// `any_path` is any path inside the repo (the runner resolves the main
/// checkout itself).
pub(crate) fn spawn_fold(
    fold_tx: &FoldTx,
    waker: &TerminalWaker,
    mq: MergeQueueConfig,
    any_path: PathBuf,
) {
    let tx = fold_tx.clone();
    let waker = waker.clone();
    tokio::task::spawn_blocking(move || {
        let r = integrate::fold_active_repo(&mq, &any_path);
        if tx.send(r).is_ok() {
            let _ = waker.wake();
        }
    });
}

/// Kick the agent-driven queue drain (`merge drain`) off the loop: collect the
/// repo's pending queue rows, run [`merge_driver::drive_queue`] (which may
/// dispatch headless fixing agents), and stream every status transition back
/// as a [`DriveMsg::Step`] so the panel repaints live.
pub(crate) fn spawn_drive(
    drive_tx: &DriveTx,
    waker: &TerminalWaker,
    mq: MergeQueueConfig,
    any_path: PathBuf,
) {
    let tx = drive_tx.clone();
    let waker = waker.clone();
    tokio::task::spawn_blocking(move || {
        let send = |m: DriveMsg| {
            if tx.send(m).is_ok() {
                let _ = waker.wake();
            }
        };
        let Some(root) = integrate::main_checkout(&any_path) else {
            send(DriveMsg::Failed("not inside a git repository".into()));
            return;
        };
        let db = match Db::open() {
            Ok(d) => d,
            Err(e) => {
                send(DriveMsg::Failed(format!("db: {e}")));
                return;
            }
        };
        let items: Vec<QueueItem> = merge_driver::rows_for_repo(&db, &root)
            .into_iter()
            .filter(|r| r.status != "landed" && r.status != "ready")
            .map(|r| QueueItem {
                worktree: r.worktree,
                branch: r.branch,
            })
            .collect();
        if items.is_empty() {
            send(DriveMsg::Done(DriveOutcome::default()));
            return;
        }
        let out = merge_driver::drive_queue(&mq, &root, &db, items, |s| {
            send(DriveMsg::Step {
                worktree: s.worktree.to_string(),
                branch: s.branch.to_string(),
                status: s.status.to_string(),
                detail: s.detail.to_string(),
            });
        });
        send(DriveMsg::Done(out));
    });
}

// ---------------------------------------------------------------------------
// Action dispatch (the loop's Integrate / DrainMergeQueue arms)
// ---------------------------------------------------------------------------

/// Guards shared by both fold-actor dispatches: the master switch and the
/// single inflight flag (batch fold and queue drain mutate the same target
/// ref, so they are mutually exclusive by construction). Returns whether the
/// caller may proceed (the flag is already set when it does).
fn arm_fold(enabled: bool, fold_inflight: &mut bool, toasts: &mut Toasts, verb: &str) -> bool {
    let now = std::time::Instant::now();
    if !enabled {
        toasts.push(
            ToastKind::Info,
            "Merge queue disabled — set [merge_queue] enabled = true".to_string(),
            now,
            std::time::Duration::from_secs(6),
        );
        return false;
    }
    if *fold_inflight {
        toasts.push(
            ToastKind::Info,
            "Already integrating…".to_string(),
            now,
            std::time::Duration::from_secs(3),
        );
        return false;
    }
    *fold_inflight = true;
    toasts.success(format!("{verb}…"), now);
    true
}

/// The `integrate` action: batch-fold every eligible branch (no queue, no
/// agent — CLI symmetry with `thegn integrate`).
pub(crate) fn dispatch_integrate(
    enabled: bool,
    fold_inflight: &mut bool,
    toasts: &mut Toasts,
    fold_tx: &FoldTx,
    waker: &TerminalWaker,
    mq: MergeQueueConfig,
    any_path: PathBuf,
) {
    if arm_fold(enabled, fold_inflight, toasts, "Integrating") {
        spawn_fold(fold_tx, waker, mq, any_path);
    }
}

/// The `merge-drain` action: drain the queue with the full agent autopilot
/// (CLI symmetry with `thegn merge drain`).
pub(crate) fn dispatch_drain(
    enabled: bool,
    fold_inflight: &mut bool,
    toasts: &mut Toasts,
    drive_tx: &DriveTx,
    waker: &TerminalWaker,
    mq: MergeQueueConfig,
    any_path: PathBuf,
) {
    if arm_fold(enabled, fold_inflight, toasts, "Draining merge queue") {
        spawn_drive(drive_tx, waker, mq, any_path);
    }
}

// ---------------------------------------------------------------------------
// Loop-side channel drains
// ---------------------------------------------------------------------------

/// Drain batch-fold results: report what landed/deferred and re-hydrate so the
/// advanced target tip and cleared activity dots show immediately.
pub(crate) fn drain_fold_results(rx: &mut FoldRx, ctx: &mut DrainCtx) {
    while let Ok(result) = rx.try_recv() {
        ctx.loop_perf.tick(crate::perf::WakeSource::Fold);
        *ctx.fold_inflight = false;
        let now = std::time::Instant::now();
        match result {
            Ok(r) => {
                let msg = if r.deferred.is_empty() {
                    format!("Integrated: {} landed", r.landed.len())
                } else {
                    format!(
                        "Integrated: {} landed, {} deferred",
                        r.landed.len(),
                        r.deferred.len()
                    )
                };
                let landed = !r.landed.is_empty();
                ctx.toasts.success(msg, now);
                *ctx.want_model_refresh = true;
                // A batch fold's `persist` may have removed landed worktrees.
                if landed {
                    ctx.reap_removed_tabs();
                }
            }
            Err(e) => {
                ctx.toasts.push(
                    ToastKind::Info,
                    format!("Integrate failed: {e}"),
                    now,
                    std::time::Duration::from_secs(6),
                );
            }
        }
        *ctx.dirty = true;
    }
}

/// Drain drive messages: patch the panel row in place (live repaint, no wait
/// for the model tick), toast the settled transitions, and route them to the
/// notification inbox.
pub(crate) fn drain_drive_msgs(rx: &mut DriveRx, ctx: &mut DrainCtx) {
    while let Ok(msg) = rx.try_recv() {
        ctx.loop_perf.tick(crate::perf::WakeSource::Fold);
        let now = std::time::Instant::now();
        match msg {
            DriveMsg::Step {
                worktree,
                branch,
                status,
                detail,
            } => {
                apply_step(&mut ctx.model.panel, &worktree, &branch, &status, &detail);
                match status.as_str() {
                    "landed" => {
                        ctx.toasts.success(format!("Landed {branch}"), now);
                        notify_queue(
                            ctx,
                            NotificationKind::QueueLanded,
                            &worktree,
                            format!("merge queue: {branch} landed"),
                        );
                        *ctx.want_model_refresh = true;
                        // The drive's `apply` may have removed this worktree.
                        ctx.reap_removed_tabs();
                    }
                    "ready" => {
                        ctx.toasts
                            .success(format!("{branch} ready — gated green"), now);
                        notify_queue(
                            ctx,
                            NotificationKind::QueueReady,
                            &worktree,
                            format!("merge queue: {branch} ready to land"),
                        );
                        *ctx.want_model_refresh = true;
                    }
                    "needs_human" => {
                        ctx.toasts.push(
                            ToastKind::Info,
                            format!("{branch} needs a human — {detail}"),
                            now,
                            std::time::Duration::from_secs(6),
                        );
                        notify_queue(
                            ctx,
                            NotificationKind::QueueNeedsHuman,
                            &worktree,
                            format!("merge queue: {branch} needs a human — {detail}"),
                        );
                        *ctx.want_model_refresh = true;
                    }
                    "deferred" | "gate_failed" => *ctx.want_model_refresh = true,
                    _ => {}
                }
            }
            DriveMsg::Done(out) => {
                *ctx.fold_inflight = false;
                let total =
                    out.landed.len() + out.ready.len() + out.deferred.len() + out.needs_human.len();
                let msg = if total == 0 {
                    "Merge queue: nothing to drain".to_string()
                } else {
                    format!(
                        "Drained: {} landed, {} ready, {} deferred, {} need a human",
                        out.landed.len(),
                        out.ready.len(),
                        out.deferred.len(),
                        out.needs_human.len()
                    )
                };
                if out.deferred.is_empty() && out.needs_human.is_empty() {
                    ctx.toasts.success(msg, now);
                } else {
                    ctx.toasts
                        .push(ToastKind::Info, msg, now, std::time::Duration::from_secs(6));
                }
                *ctx.want_model_refresh = true;
                // A land (`l`) records `landed` here without a Step; its `apply`
                // may have removed the worktree.
                if !out.landed.is_empty() {
                    ctx.reap_removed_tabs();
                }
            }
            DriveMsg::Note(msg) => {
                ctx.toasts.info(msg, now);
                *ctx.want_model_refresh = true;
            }
            DriveMsg::Failed(e) => {
                *ctx.fold_inflight = false;
                ctx.toasts.push(
                    ToastKind::Info,
                    format!("Merge queue: {e}"),
                    now,
                    std::time::Duration::from_secs(6),
                );
            }
        }
        *ctx.dirty = true;
    }
}

/// Patch (or insert) the panel's queue row for `worktree` so a drive step
/// paints on the very next frame instead of waiting for the model tick. Pure —
/// unit-tested below.
pub(crate) fn apply_step(
    panel: &mut crate::panel::PanelData,
    worktree: &str,
    branch: &str,
    status: &str,
    detail: &str,
) {
    let now = util::now();
    let row = match panel
        .merge_queue
        .iter_mut()
        .find(|r| r.worktree == worktree)
    {
        Some(r) => r,
        None => {
            // A drain kicked off before hydration caught up (e.g. CLI-enqueued
            // moments ago): materialize the row so progress is still visible.
            panel.merge_queue.push(thegn_core::db::MergeQueueRow {
                worktree: worktree.to_string(),
                branch: branch.to_string(),
                target_branch: String::new(),
                status: String::new(),
                queued_at: now,
                updated_at: now,
                result_oid: None,
                conflict_paths: None,
                error_detail: None,
            });
            panel.merge_queue.last_mut().expect("just pushed")
        }
    };
    row.status = status.to_string();
    row.updated_at = now;
    match status {
        "landed" | "ready" => {
            if !detail.is_empty() {
                row.result_oid = Some(detail.to_string());
            }
            row.conflict_paths = None;
            row.error_detail = None;
        }
        "deferred" => {
            row.conflict_paths = (!detail.is_empty()).then(|| detail.to_string());
            row.error_detail = None;
        }
        "gate_failed" | "needs_human" | "agent_running" => {
            row.error_detail = (!detail.is_empty()).then(|| detail.to_string());
        }
        _ => {}
    }
}

/// Route a settled queue transition to the notification machinery: rules/DND
/// decide desktop + sound; the inbox record is written off-loop.
fn notify_queue(ctx: &mut DrainCtx, kind: NotificationKind, worktree: &str, message: String) {
    let dec = ctx
        .notify_state
        .decide(kind.as_str(), worktree, &message, worktree);
    if dec.desktop {
        let n = thegn_core::notification::Notification {
            id: 0,
            kind,
            source_ref: worktree.to_string(),
            message: message.clone(),
            created_at_ms: util::now(),
            read: false,
            worktree_path: worktree.to_string(),
        };
        ctx.event_bus.publish_with_notification(
            &thegn_core::event_bus::Event::NotificationReceived { notification: n },
        );
    }
    ctx.notify_state.emit_sound(&dec);
    if dec.record {
        let (kind, wt, msg) = (kind.as_str(), worktree.to_string(), message);
        tokio::task::spawn_blocking(move || {
            use thegn_core::store::NotificationStore;
            let Ok(db) = Db::open() else { return };
            // best-effort: the inbox is a cache; the queue row is the record.
            let _ = db.put_notification(kind, &wt, &msg, &wt);
        });
    }
}

// ---------------------------------------------------------------------------
// Panel section action keys
// ---------------------------------------------------------------------------

/// What one section key resolves to, given the cursor row's status. Kept pure
/// so the status×key matrix is unit-testable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MqAction {
    AddCurrent,
    AddAll,
    Remove,
    Land,
    Retry,
    ClearLanded,
    Drain,
}

/// The section's key table. `Err` carries the status-line hint for a key that
/// doesn't apply to the cursor row.
pub(crate) fn row_action_for(
    key: char,
    row_status: Option<&str>,
) -> Result<MqAction, &'static str> {
    match key {
        'a' => Ok(MqAction::AddCurrent),
        'A' => Ok(MqAction::AddAll),
        'D' => Ok(MqAction::Drain),
        'c' => Ok(MqAction::ClearLanded),
        'x' => row_status
            .map(|_| MqAction::Remove)
            .ok_or("Merge queue: no row selected"),
        'l' => match row_status {
            None => Err("Merge queue: no row selected"),
            Some("ready") => Ok(MqAction::Land),
            Some(_) => Err("Merge queue: only a ready (gated green) branch can be landed"),
        },
        'r' => match row_status {
            None => Err("Merge queue: no row selected"),
            Some("deferred" | "gate_failed" | "needs_human") => Ok(MqAction::Retry),
            Some(_) => Err("Merge queue: retry applies to deferred / gate-failed / needs-human"),
        },
        _ => Err(""),
    }
}

/// The loop locals the section keys need, borrowed for one keypress.
pub(crate) struct MqKeyCtx<'a> {
    pub model: &'a mut crate::chrome::FrameModel,
    pub cfg: &'a thegn_core::config::Config,
    /// The active tab's worktree path (the `a` add target and the repo anchor).
    pub active_wt: PathBuf,
    pub refresh_tx: &'a tokio_mpsc::UnboundedSender<RefreshKind>,
    pub waker: &'a TerminalWaker,
    pub drive_tx: &'a DriveTx,
    pub fold_inflight: &'a mut bool,
    pub toasts: &'a mut Toasts,
}

/// Handle one of the section's action keys (`a/A/x/l/r/c/D`) on the queue row
/// under the cursor. Returns whether the key was consumed. Every mutation runs
/// on `spawn_blocking`, reports its outcome as a [`DriveMsg::Note`] (or
/// `Done`/`Failed` for a land), and kicks a model refresh.
pub(crate) fn section_key(key: char, cursor: usize, ctx: MqKeyCtx) -> bool {
    let row = ctx.model.panel.merge_queue.get(cursor);
    let action = match row_action_for(key, row.map(|r| r.status.as_str())) {
        Ok(a) => a,
        Err(hint) => {
            if hint.is_empty() {
                return false;
            }
            ctx.model.status = hint.to_string();
            return true;
        }
    };
    if !ctx.cfg.merge_queue.enabled {
        ctx.model.status = "Merge queue disabled — set [merge_queue] enabled = true".into();
        return true;
    }
    let mq = ctx.cfg.merge_queue.clone();
    let note = NoteWire {
        drive_tx: ctx.drive_tx.clone(),
        refresh_tx: ctx.refresh_tx.clone(),
        waker: ctx.waker.clone(),
    };
    match action {
        MqAction::AddCurrent => {
            let wt = ctx.active_wt.clone();
            ctx.model.status = "Merge queue: queueing current worktree…".into();
            tokio::task::spawn_blocking(move || note.send(add_worktree(&mq, &wt)));
        }
        MqAction::AddAll => {
            let wt = ctx.active_wt.clone();
            ctx.model.status = "Merge queue: queueing all eligible branches…".into();
            tokio::task::spawn_blocking(move || note.send(add_all(&mq, &wt)));
        }
        MqAction::Remove => {
            let Some(wt) = row.map(|r| r.worktree.clone()) else {
                return true;
            };
            // Optimistic: drop the row now; the refresh confirms.
            ctx.model.panel.merge_queue.retain(|r| r.worktree != wt);
            tokio::task::spawn_blocking(move || {
                note.send(match Db::open().and_then(|db| db.remove_merge_entry(&wt)) {
                    Ok(()) => "Removed from queue".to_string(),
                    Err(e) => format!("Remove failed: {e}"),
                });
            });
        }
        MqAction::Land => {
            // A land is a fold+gate+CAS — exclusive with any running drain.
            if *ctx.fold_inflight {
                ctx.model.status = "Merge queue: a drain is already running".into();
                return true;
            }
            let Some(wt) = row.map(|r| r.worktree.clone()) else {
                return true;
            };
            *ctx.fold_inflight = true;
            let cfg = ctx.cfg.clone();
            ctx.toasts
                .success("Landing…".to_string(), std::time::Instant::now());
            tokio::task::spawn_blocking(move || note.send_msg(land_ready(&cfg, &wt)));
        }
        MqAction::Retry => {
            let Some((wt, branch, target)) = row.map(|r| {
                (
                    r.worktree.clone(),
                    r.branch.clone(),
                    r.target_branch.clone(),
                )
            }) else {
                return true;
            };
            // Optimistic: back to queued (the enqueue upsert does exactly this).
            apply_step(&mut ctx.model.panel, &wt, &branch, "queued", "");
            tokio::task::spawn_blocking(move || {
                note.send(
                    match Db::open().and_then(|db| db.enqueue_merge(&wt, &branch, &target)) {
                        Ok(()) => format!("Requeued {branch}"),
                        Err(e) => format!("Retry failed: {e}"),
                    },
                );
            });
        }
        MqAction::ClearLanded => {
            let landed: Vec<String> = ctx
                .model
                .panel
                .merge_queue
                .iter()
                .filter(|r| r.status == "landed")
                .map(|r| r.worktree.clone())
                .collect();
            if landed.is_empty() {
                ctx.model.status = "Merge queue: nothing landed to clear".into();
                return true;
            }
            ctx.model.panel.merge_queue.retain(|r| r.status != "landed");
            tokio::task::spawn_blocking(move || {
                let n = landed.len();
                let ok = Db::open().map(|db| {
                    landed
                        .iter()
                        .filter(|wt| db.remove_merge_entry(wt).is_ok())
                        .count()
                });
                note.send(match ok {
                    Ok(k) if k == n => format!("Cleared {n} landed row(s)"),
                    Ok(k) => format!("Cleared {k}/{n} landed row(s)"),
                    Err(e) => format!("Clear failed: {e}"),
                });
            });
        }
        MqAction::Drain => {
            dispatch_drain(
                true, // enabled checked above
                ctx.fold_inflight,
                ctx.toasts,
                ctx.drive_tx,
                ctx.waker,
                mq,
                ctx.active_wt.clone(),
            );
        }
    }
    true
}

/// The off-loop mutation helpers' way back to the loop: a `DriveMsg` (toast)
/// plus a model-refresh kick, each with a waker pulse.
struct NoteWire {
    drive_tx: DriveTx,
    refresh_tx: tokio_mpsc::UnboundedSender<RefreshKind>,
    waker: TerminalWaker,
}

impl NoteWire {
    fn send(&self, note: String) {
        self.send_msg(DriveMsg::Note(note));
    }
    fn send_msg(&self, msg: DriveMsg) {
        let _ = self.drive_tx.send(msg);
        let _ = self.refresh_tx.send(RefreshKind::Model);
        let _ = self.waker.wake();
    }
}

/// Enqueue the branch a worktree is on (the section's `a`). Mirrors
/// `cmd/merge.rs::add`'s single-worktree arm.
fn add_worktree(mq: &MergeQueueConfig, wt: &Path) -> String {
    let Some(root) = integrate::main_checkout(wt) else {
        return "Add failed: not inside a git repository".into();
    };
    let target = integrate::resolve_target(mq, &root);
    let branch = util::git_out(wt, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let Some(branch) = branch else {
        return "Add failed: not on a branch (detached HEAD?)".into();
    };
    if branch == target {
        return format!("Skipped {branch} — that's the target branch");
    }
    let db = match Db::open() {
        Ok(d) => d,
        Err(e) => return format!("Add failed: {e}"),
    };
    let wt_s = wt.to_string_lossy();
    match db.enqueue_merge(&wt_s, &branch, &target) {
        Ok(()) => {
            crate::merge_lifecycle::apply(mq, &db, &root, &wt_s, &branch, LifecycleEvent::Enqueued);
            format!("Queued {branch}")
        }
        Err(e) => format!("Add failed: {e}"),
    }
}

/// Enqueue every eligible worktree branch (the section's `A`). Mirrors
/// `cmd/merge.rs::add --all`.
fn add_all(mq: &MergeQueueConfig, any_path: &Path) -> String {
    let Some(root) = integrate::main_checkout(any_path) else {
        return "Add failed: not inside a git repository".into();
    };
    let target = integrate::resolve_target(mq, &root);
    let cands = match integrate::candidate_branches(mq, &root, &target) {
        Ok(c) => c,
        Err(e) => return format!("Add failed: {e}"),
    };
    let db = match Db::open() {
        Ok(d) => d,
        Err(e) => return format!("Add failed: {e}"),
    };
    let mut queued = 0usize;
    for (branch, wt) in &cands.worktrees {
        if db.enqueue_merge(wt, branch, &target).is_ok() {
            crate::merge_lifecycle::apply(mq, &db, &root, wt, branch, LifecycleEvent::Enqueued);
            queued += 1;
        }
    }
    if cands.skipped_dirty.is_empty() {
        format!("Queued {queued} branch(es)")
    } else {
        format!(
            "Queued {queued} branch(es); skipped {} dirty (set [merge_queue] snapshot_dirty = true)",
            cands.skipped_dirty.len()
        )
    }
}

/// Land a `ready` row (the section's `l`): the same fold/gate/CAS core as
/// `thegn merge land`, recording the outcome on the queue row. Returns the
/// terminal `DriveMsg` (a `Done` clears the inflight flag).
fn land_ready(cfg: &thegn_core::config::Config, wt: &str) -> DriveMsg {
    let (branch, _target, outcome) = match crate::cmd::land::land_branch(cfg, Path::new(wt)) {
        Ok(r) => r,
        Err(e) => return DriveMsg::Failed(format!("land: {e}")),
    };
    let db = Db::open().ok();
    let record = |status: &str, oid: Option<&str>, detail: Option<&str>| {
        if let Some(db) = &db {
            // best-effort: the DB is a cache; the ref move is the record.
            let _ = db.update_merge_status(wt, status, oid, detail, None);
        }
    };
    // Drive the sidebar-folder lifecycle for this worktree (no-op unless
    // organize_folders is on). Any removal is reaped by drain_drive_msgs.
    // `branch` is a param (not captured) so the arms can still move it below.
    let lifecycle = |event: LifecycleEvent, branch: &str| {
        if let (Some(db), Some(root)) = (&db, integrate::main_checkout(Path::new(wt))) {
            crate::merge_lifecycle::apply(&cfg.merge_queue, db, &root, wt, branch, event);
        }
    };
    match outcome {
        AttemptOutcome::Landed { commit } => {
            record("landed", Some(&commit), None);
            lifecycle(LifecycleEvent::Landed, &branch);
            DriveMsg::Done(DriveOutcome {
                landed: vec![branch],
                ..DriveOutcome::default()
            })
        }
        AttemptOutcome::UpToDate => {
            record("landed", None, Some("already merged"));
            lifecycle(LifecycleEvent::Landed, &branch);
            DriveMsg::Done(DriveOutcome {
                landed: vec![branch],
                ..DriveOutcome::default()
            })
        }
        AttemptOutcome::Conflict { paths } => {
            let detail = paths.join("\n");
            record("deferred", None, Some(&detail));
            lifecycle(LifecycleEvent::Failed, &branch);
            DriveMsg::Failed(format!(
                "{branch} conflicts: {}",
                detail.replace('\n', ", ")
            ))
        }
        AttemptOutcome::GateFailed { .. } => {
            record("gate_failed", None, Some("breaks build"));
            lifecycle(LifecycleEvent::Failed, &branch);
            DriveMsg::Failed(format!("{branch} breaks the build (gate red)"))
        }
        AttemptOutcome::Ready { tip } => {
            record("ready", Some(&tip), Some("gated green — awaiting land"));
            DriveMsg::Failed(format!("{branch} is ready but was not landed"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(worktree: &str, status: &str) -> thegn_core::db::MergeQueueRow {
        thegn_core::db::MergeQueueRow {
            worktree: worktree.into(),
            branch: format!("b-{status}"),
            target_branch: "main".into(),
            status: status.into(),
            queued_at: 1,
            updated_at: 1,
            result_oid: None,
            conflict_paths: None,
            error_detail: None,
        }
    }

    #[test]
    fn key_matrix_resolves_by_row_status() {
        use MqAction::*;
        // Row-independent keys.
        for (k, want) in [
            ('a', AddCurrent),
            ('A', AddAll),
            ('D', Drain),
            ('c', ClearLanded),
        ] {
            assert_eq!(row_action_for(k, None), Ok(want), "{k}");
            assert_eq!(row_action_for(k, Some("queued")), Ok(want), "{k}");
        }
        // Land: ready only.
        assert_eq!(row_action_for('l', Some("ready")), Ok(Land));
        assert!(row_action_for('l', Some("queued")).is_err());
        assert!(row_action_for('l', None).is_err());
        // Retry: the blocked statuses only.
        for s in ["deferred", "gate_failed", "needs_human"] {
            assert_eq!(row_action_for('r', Some(s)), Ok(Retry), "{s}");
        }
        assert!(row_action_for('r', Some("landed")).is_err());
        // Remove: any row, but a row is required.
        assert_eq!(row_action_for('x', Some("landed")), Ok(Remove));
        assert!(row_action_for('x', None).is_err());
        // Unknown keys are unconsumed (empty hint).
        assert_eq!(row_action_for('z', None), Err(""));
    }

    #[test]
    fn apply_step_patches_row_in_place() {
        let mut panel = crate::panel::PanelData::default();
        panel.merge_queue.push(row("/wt/a", "queued"));

        apply_step(&mut panel, "/wt/a", "b-queued", "folding", "");
        assert_eq!(panel.merge_queue[0].status, "folding");

        apply_step(
            &mut panel,
            "/wt/a",
            "b-queued",
            "deferred",
            "src/a.rs\nsrc/b.rs",
        );
        assert_eq!(panel.merge_queue[0].status, "deferred");
        assert_eq!(
            panel.merge_queue[0].conflict_paths.as_deref(),
            Some("src/a.rs\nsrc/b.rs")
        );

        apply_step(&mut panel, "/wt/a", "b-queued", "landed", "abc123");
        assert_eq!(panel.merge_queue[0].status, "landed");
        assert_eq!(panel.merge_queue[0].result_oid.as_deref(), Some("abc123"));
        // Landing clears the failure details.
        assert!(panel.merge_queue[0].conflict_paths.is_none());
        assert!(panel.merge_queue[0].error_detail.is_none());
    }

    #[test]
    fn apply_step_materializes_a_missing_row() {
        let mut panel = crate::panel::PanelData::default();
        apply_step(
            &mut panel,
            "/wt/new",
            "feat",
            "agent_running",
            "agent fixing (1/2)",
        );
        assert_eq!(panel.merge_queue.len(), 1);
        let r = &panel.merge_queue[0];
        assert_eq!(
            (r.worktree.as_str(), r.branch.as_str()),
            ("/wt/new", "feat")
        );
        assert_eq!(r.status, "agent_running");
        assert_eq!(r.error_detail.as_deref(), Some("agent fixing (1/2)"));
    }
}
