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
use thegn_core::config::Config;
use thegn_core::db::Db;
use thegn_core::merge_lifecycle::LifecycleEvent;
use thegn_core::notification::NotificationKind;
use thegn_core::store::WorktreeAuxStore;
use thegn_core::util;
use tokio::sync::mpsc as tokio_mpsc;

use crate::hydrate::RefreshKind;
use crate::integrate::{self, AttemptOutcome, FoldReport};
use crate::merge_driver::{self, DriveOutcome, QueueItem};
use crate::toast::Toasts;

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
    // land removes a worktree dir off-loop, probe for the orphaned tab on a
    // worker and deliver a typed completion to the compositor.
    pub session: &'a crate::session::Session,
    pub waker: &'a TerminalWaker,
}

impl DrainCtx<'_> {
    /// Reap any tab whose worktree dir vanished (an `on_landed = remove/detach`
    /// land). No-op when nothing was removed. Kept here so both drains share it.
    fn reap_removed_tabs(&mut self) {
        crate::merge_lifecycle::spawn_reconcile_removed_tabs(
            self.session,
            Some(self.waker.clone()),
        );
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
pub(crate) fn spawn_fold(fold_tx: &FoldTx, waker: &TerminalWaker, cfg: Config, any_path: PathBuf) {
    let tx = fold_tx.clone();
    let waker = waker.clone();
    tokio::task::spawn_blocking(move || {
        let r = integrate::fold_active_repo(&cfg, &any_path);
        if tx.send(r).is_ok() {
            let _ = waker.wake(); // best-effort: waker pulse: an input nudge must never fail the calling path
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
    cfg: Config,
    any_path: PathBuf,
) {
    let tx = drive_tx.clone();
    let waker = waker.clone();
    tokio::task::spawn_blocking(move || {
        let send = |m: DriveMsg| {
            if tx.send(m).is_ok() {
                let _ = waker.wake(); // best-effort: waker pulse: an input nudge must never fail the calling path
            }
        };
        let Some(root) = integrate::main_checkout(&any_path) else {
            send(DriveMsg::Failed("not inside a git repository".into()));
            return;
        };
        // Repo-resolved, inside the blocking task: the loop must not pay for the
        // git call that derives the workspace slug.
        let mq = cfg.repo_merge_queue(&root);
        let db = match Db::open() {
            Ok(d) => d,
            Err(e) => {
                send(DriveMsg::Failed(format!("db: {e}")));
                return;
            }
        };
        // The fold runs in the target repo's object store; a remote target must
        // be drained on its own host (see the guidance).
        if let Some(msg) = crate::merge_ops::remote_target_guard(&db, &root) {
            send(DriveMsg::Failed(msg));
            return;
        }
        let items: Vec<QueueItem> = merge_driver::rows_for_repo(&db, &root)
            .into_iter()
            .filter(|r| r.status != "landed" && r.status != "ready")
            .map(|r| QueueItem {
                worktree: r.worktree,
                branch: r.branch,
                location: r.location,
                agent_attempts: r.agent_attempts,
            })
            .collect();
        if items.is_empty() {
            send(DriveMsg::Done(DriveOutcome::default()));
            return;
        }
        let out = merge_driver::drive_queue(&mq, &cfg, &root, &db, items, |s| {
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
        toasts.info_ttl(
            "Merge queue disabled — set [merge_queue] enabled = true".to_string(),
            now,
            std::time::Duration::from_secs(6),
        );
        return false;
    }
    if *fold_inflight {
        toasts.info_ttl(
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

/// The `sweep-merged` action: collect merged worktrees whose grace period is up
/// (CLI symmetry with `thegn merge sweep`). Not gated by `fold_inflight` — it
/// removes already-landed worktrees and never touches the target ref, so it
/// cannot race a fold for it.
pub(crate) fn dispatch_sweep_merged(
    enabled: bool,
    toasts: &mut Toasts,
    cfg: Config,
    any_path: PathBuf,
) {
    let now = std::time::Instant::now();
    if !enabled {
        toasts.info_ttl(
            "Merge queue disabled — set [merge_queue] enabled = true".to_string(),
            now,
            std::time::Duration::from_secs(6),
        );
        return;
    }
    toasts.success("Sweeping merged worktrees…".to_string(), now);
    // Off-loop: stats worktrees and shells out to git.
    crate::merge_sweep::spawn(cfg, any_path);
}

/// The `integrate` action: batch-fold the queued branches (no agent — CLI
/// symmetry with `thegn integrate`). Widened to every eligible branch only by
/// `[merge_queue] require_enqueue = false`; `fold_active_repo` applies that
/// guard, so this keypress cannot land a branch nobody nominated.
pub(crate) fn dispatch_integrate(
    enabled: bool,
    fold_inflight: &mut bool,
    toasts: &mut Toasts,
    fold_tx: &FoldTx,
    waker: &TerminalWaker,
    cfg: Config,
    any_path: PathBuf,
) {
    if arm_fold(enabled, fold_inflight, toasts, "Integrating") {
        spawn_fold(fold_tx, waker, cfg, any_path);
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
    cfg: Config,
    any_path: PathBuf,
) {
    if arm_fold(enabled, fold_inflight, toasts, "Draining merge queue") {
        spawn_drive(drive_tx, waker, cfg, any_path);
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
                ctx.toasts.info_ttl(
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
                        ctx.toasts.info_ttl(
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
                // A configured-but-unresolvable agent turns the drain into
                // "defer everything"; without this it looks like a clean no-op.
                for w in &out.warnings {
                    ctx.toasts
                        .info_ttl(w.clone(), now, std::time::Duration::from_secs(8));
                }
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
                        .info_ttl(msg, now, std::time::Duration::from_secs(6));
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
                ctx.toasts.info_ttl(
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
                location: String::new(),
                agent_attempts: 0,
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
    let dec = crate::notify::route(
        ctx.notify_state,
        kind.as_str(),
        worktree,
        &message,
        worktree,
    );
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
    let (kind, wt, msg) = (kind.as_str(), worktree.to_string(), message);
    let routed = dec.clone();
    tokio::task::spawn_blocking(move || {
        let Ok(db) = Db::open() else { return };
        if routed.record {
            // best-effort: the inbox is a cache; the queue row is the record.
            let _ = crate::automation_events::insert_routed(
                &db,
                kind,
                &wt,
                &msg,
                &wt,
                Default::default(),
                &routed,
                false,
            );
        }
        // Preserve the typed merge edge even when notification routing drops
        // the accompanying human-facing queue notification.
        if kind == "queue_landed" {
            let origin = crate::automation_events::take_merge_origin(&db, &wt);
            crate::automation_events::submit_fact(
                thegn_core::automation::AutomationEventKind::MergeLanded,
                format!("merge:{wt}"),
                Some(wt.clone()),
                Some(msg),
                crate::automation_events::EventFacts {
                    origin,
                    ..Default::default()
                },
            );
        }
    });
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
    // Only the cheap global `enabled` flag is read on the loop; the repo-scoped
    // resolution happens inside the spawn_blocking closures below, because
    // deriving the workspace slug shells out to git (see `repo::repo_name`) and
    // the loop must never block on I/O.
    let mq = ctx.cfg.clone();
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
            let mq = mq.clone();
            tokio::task::spawn_blocking(move || {
                // Dequeue + un-file, so removing from the queue also pulls the
                // worktree out of its "Merging"/"Needs attention" folder.
                note.send(match Db::open() {
                    Ok(db) => match crate::merge_ops::dequeue_worktree(&mq, &db, Path::new(&wt)) {
                        Ok(()) => "Removed from queue".to_string(),
                        Err(e) => format!("Remove failed: {e}"),
                    },
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
            // Under `expire` a landed row IS the grace-period clock, so dropping
            // it alone would strand its worktree in `merged_folder` with nothing
            // left to sweep it. Clearing therefore means "collect them now" —
            // which is also what the gesture reads as. `sweep` still refuses to
            // touch a merged worktree that has become dirty again.
            let sweep_cfg = ctx.cfg.clone();
            let sweep_root = ctx.active_wt.clone();
            tokio::task::spawn_blocking(move || {
                let n = landed.len();
                let swept = crate::integrate::main_checkout(&sweep_root)
                    .map(|root| crate::merge_sweep::sweep(&sweep_cfg, &root, true))
                    .unwrap_or_default();
                let ok = Db::open().map(|db| {
                    landed
                        .iter()
                        .filter(|wt| db.remove_merge_entry(wt).is_ok())
                        .count()
                });
                let tail = if swept.collected.is_empty() {
                    String::new()
                } else {
                    format!(", removed {} worktree(s)", swept.collected.len())
                };
                let kept = if swept.kept_dirty.is_empty() {
                    String::new()
                } else {
                    format!("; kept {} with uncommitted changes", swept.kept_dirty.len())
                };
                note.send(match ok {
                    Ok(k) if k == n => format!("Cleared {n} landed row(s){tail}{kept}"),
                    Ok(k) => format!("Cleared {k}/{n} landed row(s){tail}{kept}"),
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

// ---------------------------------------------------------------------------
// Sidebar context-menu actions
// ---------------------------------------------------------------------------

/// A merge-queue action fired from the sidebar's row / workspace context menu.
/// Mirrors the panel's `a/A/x/l/r/c/D`, but keyed by an explicit path rather
/// than the panel cursor so the two surfaces behave identically.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SidebarMq {
    /// Queue the target worktree's branch (panel `a`).
    Add,
    /// Remove the target worktree from the queue (panel `x`).
    Remove,
    /// Land a `ready` worktree — fold + gate + CAS (panel `l`).
    Land,
    /// Requeue a blocked worktree back to `queued` (panel `r`).
    Retry,
    /// Queue every eligible branch in the target's repo (panel `A`).
    AddAll,
    /// Clear the target repo's whole queue (CLI `merge clear`).
    Clear,
    /// Drain the target repo's queue with the agent autopilot (panel `D`).
    Drain,
}

/// Run a sidebar merge-queue action. `ctx.active_wt` is the target worktree
/// (per-row actions) or any path inside the repo (workspace-wide actions).
/// Every mutation runs on `spawn_blocking`, reports via [`NoteWire`] (toast +
/// model refresh), and — for Land/Drain — respects the shared `fold_inflight`.
pub(crate) fn sidebar_action(action: SidebarMq, ctx: MqKeyCtx) {
    if !ctx.cfg.merge_queue.enabled {
        ctx.model.status = "Merge queue disabled — set [merge_queue] enabled = true".into();
        return;
    }
    // Only the cheap global `enabled` flag is read on the loop; the repo-scoped
    // resolution happens inside the spawn_blocking closures below, because
    // deriving the workspace slug shells out to git (see `repo::repo_name`) and
    // the loop must never block on I/O.
    let mq = ctx.cfg.clone();
    let note = NoteWire {
        drive_tx: ctx.drive_tx.clone(),
        refresh_tx: ctx.refresh_tx.clone(),
        waker: ctx.waker.clone(),
    };
    let wt = ctx.active_wt.clone();
    match action {
        SidebarMq::Add => {
            ctx.model.status = "Merge queue: queueing worktree…".into();
            tokio::task::spawn_blocking(move || note.send(add_worktree(&mq, &wt)));
        }
        SidebarMq::AddAll => {
            ctx.model.status = "Merge queue: queueing all eligible branches…".into();
            tokio::task::spawn_blocking(move || note.send(add_all(&mq, &wt)));
        }
        SidebarMq::Retry => {
            // enqueue is an upsert that resets the row to `queued`, so requeuing a
            // present row is exactly the add path.
            ctx.model.status = "Merge queue: requeueing…".into();
            tokio::task::spawn_blocking(move || note.send(add_worktree(&mq, &wt)));
        }
        SidebarMq::Remove => {
            let wt_s = wt.to_string_lossy().to_string();
            // Optimistic: drop the row now; the refresh confirms.
            ctx.model.panel.merge_queue.retain(|r| r.worktree != wt_s);
            let mq = mq.clone();
            tokio::task::spawn_blocking(move || {
                // Dequeue + un-file: leaving the queue also leaves the folder.
                note.send(match Db::open() {
                    Ok(db) => {
                        match crate::merge_ops::dequeue_worktree(&mq, &db, Path::new(&wt_s)) {
                            Ok(()) => "Removed from queue".to_string(),
                            Err(e) => format!("Remove failed: {e}"),
                        }
                    }
                    Err(e) => format!("Remove failed: {e}"),
                });
            });
        }
        SidebarMq::Land => {
            // A land is a fold+gate+CAS — exclusive with any running drain.
            if *ctx.fold_inflight {
                ctx.model.status = "Merge queue: a drain is already running".into();
                return;
            }
            *ctx.fold_inflight = true;
            let cfg = ctx.cfg.clone();
            let wt_s = wt.to_string_lossy().to_string();
            ctx.toasts
                .success("Landing…".to_string(), std::time::Instant::now());
            tokio::task::spawn_blocking(move || note.send_msg(land_ready(&cfg, &wt_s)));
        }
        SidebarMq::Clear => {
            ctx.model.status = "Merge queue: clearing…".into();
            let mq = mq.clone();
            tokio::task::spawn_blocking(move || note.send(clear_repo_note(&wt, &mq)));
        }
        SidebarMq::Drain => {
            dispatch_drain(
                true, // enabled checked above
                ctx.fold_inflight,
                ctx.toasts,
                ctx.drive_tx,
                ctx.waker,
                mq,
                wt,
            );
        }
    }
}

/// Clear every queue row for the repo `any_path` belongs to (the workspace
/// menu's "Clear merge queue"). Mirrors the CLI `thegn merge clear`.
fn clear_repo_note(any_path: &Path, cfg: &Config) -> String {
    let Some(root) = integrate::main_checkout(any_path) else {
        return "Clear failed: not inside a git repository".into();
    };
    let db = match Db::open() {
        Ok(d) => d,
        Err(e) => return format!("Clear failed: {e}"),
    };
    match crate::merge_ops::clear_repo(cfg, &db, &root) {
        Ok(0) => "Merge queue already empty".into(),
        Ok(n) => format!("Cleared {n} queued branch(es)"),
        Err(e) => format!("Clear failed: {e}"),
    }
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
        let _ = self.drive_tx.send(msg); // best-effort: send: the consumer may be gone; a closed channel is the consumer going away
        let _ = self.refresh_tx.send(RefreshKind::Model); // best-effort: send: the consumer may be gone; a closed channel is the consumer going away
        let _ = self.waker.wake(); // best-effort: waker pulse: an input nudge must never fail the calling path
    }
}

/// Enqueue the branch a worktree is on (the section's `a`). Mirrors
/// `cmd/merge.rs::add`'s single-worktree arm.
fn add_worktree(cfg: &Config, wt: &Path) -> String {
    let Some(root) = integrate::main_checkout(wt) else {
        return "Add failed: not inside a git repository".into();
    };
    // Resolved HERE, off the event loop: the per-repo layer needs the repo root,
    // and deriving the workspace slug shells out to git.
    let mq = &cfg.repo_merge_queue(&root);
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
fn add_all(cfg: &Config, any_path: &Path) -> String {
    let Some(root) = integrate::main_checkout(any_path) else {
        return "Add failed: not inside a git repository".into();
    };
    let mq = &cfg.repo_merge_queue(&root);
    let target = integrate::resolve_target(mq, &root);
    let override_gpg = cfg.repo_git(&root).override_gpg;
    let cands = match integrate::candidate_branches(mq, &root, &target, override_gpg) {
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
    let db = Db::open().ok(); // best-effort: cache: queue record writes only; the land outcome is reported regardless
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
            crate::merge_lifecycle::apply(
                &cfg.repo_merge_queue(&root),
                db,
                &root,
                wt,
                branch,
                event,
            );
        }
    };
    match outcome {
        AttemptOutcome::Landed { commit, resyncs } => {
            // In-app, a checkout on the target is fast-forwarded by the ref
            // watcher (`git_watch::spawn_main_checkout_heal`); anything the fold
            // could not sync is real uncommitted work, so log it rather than
            // interrupting with a toast the user can't act on mid-drain.
            for r in &resyncs {
                if !matches!(r.outcome, thegn_core::util::ResyncOutcome::Healed) {
                    tracing::warn!(
                        target: "thegn::merge",
                        path = %r.path.display(),
                        "checkout of the target was left stale by the fold"
                    );
                }
            }
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
        AttemptOutcome::Conflict {
            paths,
            submodule_conflicts,
        } => {
            let detail =
                crate::integrate::conflict_details(&paths, &submodule_conflicts).join("\n");
            record("deferred", None, Some(&detail));
            lifecycle(LifecycleEvent::Failed, &branch);
            DriveMsg::Failed(format!(
                "{branch} conflicts: {}",
                detail.replace('\n', ", ")
            ))
        }
        AttemptOutcome::GateFailed { log } => {
            // Keep the gate output on the row: "breaks build" alone never told
            // the user which test failed.
            let detail = if log.trim().is_empty() {
                "breaks build".to_string()
            } else {
                format!("breaks build\n{}", log.trim())
            };
            record("gate_failed", None, Some(&detail));
            lifecycle(LifecycleEvent::Failed, &branch);
            DriveMsg::Failed(format!("{branch} breaks the build (gate red)"))
        }
        AttemptOutcome::GateError { reason, log } => {
            // The gate could not run — an environment fact, not a verdict about
            // the branch, so it gets its own state and never reaches the agent.
            let detail = if log.trim().is_empty() {
                reason.clone()
            } else {
                format!("{reason}\n{}", log.trim())
            };
            record("gate_error", None, Some(&detail));
            lifecycle(LifecycleEvent::Failed, &branch);
            DriveMsg::Failed(format!("{branch} was NOT gated — {reason}"))
        }
        AttemptOutcome::Unreachable { detail } => {
            record("deferred", None, Some(&detail));
            lifecycle(LifecycleEvent::Failed, &branch);
            DriveMsg::Failed(format!("{branch}: {detail}"))
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
            location: String::new(),
            agent_attempts: 0,
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
