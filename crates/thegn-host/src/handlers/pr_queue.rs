//! The PR queue's in-app surface: off-loop spawners, the channel drain that
//! patches panel rows live, and the section's key table.
//!
//! Deliberately the same shape as [`crate::handlers::merge_queue`] — every
//! mutation runs on `spawn_blocking` and reports back over a channel with a
//! waker pulse, so the compositor loop never touches the network or the DB.

use std::path::PathBuf;
use tokio::sync::mpsc as tokio_mpsc;

use termwiz::terminal::TerminalWaker;
use thegn_core::config::Config;
use thegn_core::db::{Db, PrQueueRow};
use thegn_core::store::WorktreeAuxStore;

use crate::hydrate::RefreshKind;
use crate::pr_driver::{self, PrItem, PrOutcome};
use crate::toast::Toasts;

pub(crate) type PrqTx = tokio_mpsc::UnboundedSender<PrqMsg>;
pub(crate) type PrqRx = tokio_mpsc::UnboundedReceiver<PrqMsg>;

/// What the off-loop PR-queue work reports back.
pub(crate) enum PrqMsg {
    /// One driver status transition (the DB row is already written when this
    /// fires) — the loop patches the panel row in place for a live repaint.
    Step {
        key: String,
        number: u64,
        branch: String,
        status: String,
        detail: String,
    },
    /// A refresh pass finished; clears the inflight flag and toasts the summary.
    Done(Box<PrOutcome>),
    /// A one-line outcome from a one-shot mutation (add / remove / clear).
    Note(String),
    /// The pass (or a pre-pass step) failed outright.
    Failed(String),
}

/// The loop locals the channel drain mutates, borrowed for one pass.
pub(crate) struct PrqDrainCtx<'a> {
    pub model: &'a mut crate::chrome::FrameModel,
    pub toasts: &'a mut Toasts,
    pub notify_state: &'a crate::notify::NotifyState,
    pub event_bus: &'a thegn_core::event_bus::EventBus,
    pub inflight: &'a mut bool,
    pub want_model_refresh: &'a mut bool,
    pub loop_perf: &'a mut crate::perf::LoopPerf,
}

/// Route a settled transition to the notification bus + inbox, mirroring the
/// merge queue's `notify_queue`. The source ref is the PR key, so repeated
/// transitions on one pull request coalesce rather than piling up.
fn notify_prq(
    ctx: &mut PrqDrainCtx,
    kind: thegn_core::notification::NotificationKind,
    key: &str,
    worktree: &str,
    message: String,
) {
    let dec = ctx
        .notify_state
        .decide(kind.as_str(), key, &message, worktree);
    if dec.desktop {
        let n = thegn_core::notification::Notification {
            id: 0,
            kind,
            source_ref: key.to_string(),
            message: message.clone(),
            created_at_ms: thegn_core::util::now(),
            read: false,
            worktree_path: worktree.to_string(),
        };
        ctx.event_bus.publish_with_notification(
            &thegn_core::event_bus::Event::NotificationReceived { notification: n },
        );
    }
    ctx.notify_state.emit_sound(&dec);
    ctx.notify_state
        .emit_push(&dec, kind.as_str(), &message, "", worktree);
    if dec.record {
        let (k, src, wt, msg) = (
            kind.as_str(),
            key.to_string(),
            worktree.to_string(),
            message,
        );
        tokio::task::spawn_blocking(move || {
            use thegn_core::store::NotificationStore;
            let Ok(db) = Db::open() else { return };
            // best-effort: the inbox is a cache; the queue row is the record.
            let _ = db.put_notification(k, &src, &msg, &wt);
        });
    }
}

// ---------------------------------------------------------------------------
// Off-loop spawners
// ---------------------------------------------------------------------------

/// Kick one refresh pass off the loop: collect the repo's active rows, run
/// [`pr_driver::drive_queue`] (which talks to the forge and may dispatch a
/// headless agent), and stream every transition back.
pub(crate) fn spawn_drive(tx: &PrqTx, waker: &TerminalWaker, cfg: Config, any_path: PathBuf) {
    let tx = tx.clone();
    let waker = waker.clone();
    tokio::task::spawn_blocking(move || {
        let send = |m: PrqMsg| {
            if tx.send(m).is_ok() {
                let _ = waker.wake();
            }
        };
        let Some(root) = crate::integrate::main_checkout(&any_path) else {
            send(PrqMsg::Failed("not inside a git repository".into()));
            return;
        };
        // Repo-resolved inside the blocking task: the loop must not pay for the
        // git call that derives the workspace slug.
        let pq = cfg.repo_pr_queue(&root);
        let db = match Db::open() {
            Ok(d) => d,
            Err(e) => {
                send(PrqMsg::Failed(format!("db: {e}")));
                return;
            }
        };
        let items: Vec<PrItem> = pr_driver::rows_for_repo(&db, &root)
            .iter()
            .filter(|r| !matches!(r.status.as_str(), "merged" | "closed" | "needs_human"))
            .map(PrItem::from)
            .collect();
        if items.is_empty() {
            send(PrqMsg::Done(Box::default()));
            return;
        }
        let forges = crate::forge_handle::get();
        let forge = forges.for_loc(&thegn_core::remote::GitLoc::from_db(
            &root.to_string_lossy(),
            None,
        ));
        let out = pr_driver::drive_queue(&pq, &cfg, forge, &root, &db, items, |s| {
            send(PrqMsg::Step {
                key: s.key.to_string(),
                number: s.number,
                branch: s.branch.to_string(),
                status: s.status.to_string(),
                detail: s.detail.to_string(),
            });
        });
        send(PrqMsg::Done(Box::new(out)));
    });
}

/// Queue the current worktree's pull request, off the loop (it asks the forge).
pub(crate) fn spawn_add(tx: &PrqTx, waker: &TerminalWaker, worktree: PathBuf) {
    let tx = tx.clone();
    let waker = waker.clone();
    tokio::task::spawn_blocking(move || {
        let send = |m: PrqMsg| {
            if tx.send(m).is_ok() {
                let _ = waker.wake();
            }
        };
        let Some(root) = crate::integrate::main_checkout(&worktree) else {
            send(PrqMsg::Failed("not inside a git repository".into()));
            return;
        };
        let wt = worktree.to_string_lossy().into_owned();
        let loc = thegn_core::remote::GitLoc::from_db(&wt, None);
        let forges = crate::forge_handle::get();
        let forge = forges.for_loc(&loc);
        let pr = match forge.pr_status(&loc, thegn_core::forge::PrRef::Current) {
            Ok(pr) => pr,
            Err(e) => {
                send(PrqMsg::Failed(e.describe()));
                return;
            }
        };
        let db = match Db::open() {
            Ok(d) => d,
            Err(e) => {
                send(PrqMsg::Failed(format!("db: {e}")));
                return;
            }
        };
        match db.enqueue_pr(
            &root.to_string_lossy(),
            pr.number,
            Some(&wt),
            &pr.head_ref_name,
            &pr.base_ref_name,
            forge.id(),
        ) {
            Ok(()) => send(PrqMsg::Note(format!("Queued PR #{}", pr.number))),
            Err(e) => send(PrqMsg::Failed(format!("could not queue: {e}"))),
        }
    });
}

/// Remove one row, or clear the repo's queue, off the loop.
pub(crate) fn spawn_mutate(tx: &PrqTx, waker: &TerminalWaker, any_path: PathBuf, what: Mutation) {
    let tx = tx.clone();
    let waker = waker.clone();
    tokio::task::spawn_blocking(move || {
        let send = |m: PrqMsg| {
            if tx.send(m).is_ok() {
                let _ = waker.wake();
            }
        };
        let Some(root) = crate::integrate::main_checkout(&any_path) else {
            send(PrqMsg::Failed("not inside a git repository".into()));
            return;
        };
        let db = match Db::open() {
            Ok(d) => d,
            Err(e) => {
                send(PrqMsg::Failed(format!("db: {e}")));
                return;
            }
        };
        let root_s = root.to_string_lossy().into_owned();
        let msg = match what {
            Mutation::Remove { key, number } => {
                let _ = db.remove_pr_entry(&key);
                format!("Removed PR #{number} from the queue")
            }
            Mutation::Clear => {
                let n = db.clear_pr_queue(&root_s).unwrap_or(0);
                format!("PR queue cleared ({n} removed)")
            }
            // Re-queue: the same "watch it again" reset the CLI's `add` performs,
            // which is how a `needs_human` row is re-armed.
            Mutation::Rewatch {
                number,
                worktree,
                branch,
                base,
            } => {
                let _ = db.enqueue_pr(
                    &root_s,
                    number,
                    worktree.as_deref(),
                    &branch,
                    &base,
                    "github",
                );
                format!("Watching PR #{number} again")
            }
        };
        send(PrqMsg::Note(msg));
    });
}

/// A one-shot queue mutation.
pub(crate) enum Mutation {
    Remove {
        key: String,
        number: u64,
    },
    Clear,
    Rewatch {
        number: u64,
        worktree: Option<String>,
        branch: String,
        base: String,
    },
}

// ---------------------------------------------------------------------------
// Channel drain
// ---------------------------------------------------------------------------

/// Drain everything the off-loop work reported, patching panel rows in place so
/// a running pass paints live instead of waiting for the next model tick.
pub(crate) fn drain_msgs(rx: &mut PrqRx, ctx: &mut PrqDrainCtx) {
    while let Ok(msg) = rx.try_recv() {
        ctx.loop_perf.tick(crate::perf::WakeSource::Fold);
        let now = std::time::Instant::now();
        match msg {
            PrqMsg::Step {
                key,
                number,
                branch,
                status,
                detail,
            } => {
                apply_step(&mut ctx.model.panel, &key, &status, &detail);
                // Only settled transitions are worth a toast; the transient ones
                // are already visible as a live row change.
                let wt = ctx
                    .model
                    .panel
                    .pr_queue
                    .iter()
                    .find(|r| r.key == key)
                    .and_then(|r| r.worktree.clone())
                    .unwrap_or_default();
                use thegn_core::notification::NotificationKind as K;
                match status.as_str() {
                    "merged" => {
                        ctx.toasts
                            .success(format!("PR #{number} ({branch}) merged"), now);
                        notify_prq(
                            ctx,
                            K::PrQueueMerged,
                            &key,
                            &wt,
                            format!("PR #{number} ({branch}) merged"),
                        );
                    }
                    "ready" => notify_prq(
                        ctx,
                        K::PrQueueReady,
                        &key,
                        &wt,
                        format!("PR #{number} ({branch}) is ready to merge"),
                    ),
                    "needs_human" => {
                        ctx.toasts.info_ttl(
                            format!("PR #{number} needs you — {detail}"),
                            now,
                            std::time::Duration::from_secs(8),
                        );
                        notify_prq(
                            ctx,
                            K::PrQueueNeedsHuman,
                            &key,
                            &wt,
                            format!("PR #{number} needs you — {detail}"),
                        );
                    }
                    _ => {}
                }
            }
            PrqMsg::Done(out) => {
                *ctx.inflight = false;
                for w in &out.warnings {
                    ctx.toasts
                        .info_ttl(w.clone(), now, std::time::Duration::from_secs(8));
                }
                let total = out.merged.len()
                    + out.ready.len()
                    + out.blocked.len()
                    + out.needs_human.len()
                    + out.dropped.len();
                let msg = if total == 0 {
                    "PR queue: nothing to refresh".to_string()
                } else {
                    format!(
                        "PR queue: {} merged, {} ready, {} blocked, {} need a human",
                        out.merged.len(),
                        out.ready.len(),
                        out.blocked.len(),
                        out.needs_human.len()
                    )
                };
                if out.needs_human.is_empty() {
                    ctx.toasts.success(msg, now);
                } else {
                    ctx.toasts
                        .info_ttl(msg, now, std::time::Duration::from_secs(6));
                }
                *ctx.want_model_refresh = true;
            }
            PrqMsg::Note(m) => {
                ctx.toasts.success(m, now);
                *ctx.want_model_refresh = true;
            }
            PrqMsg::Failed(m) => {
                *ctx.inflight = false;
                ctx.toasts
                    .info_ttl(m, now, std::time::Duration::from_secs(8));
                *ctx.want_model_refresh = true;
            }
        }
    }
}

/// Patch one panel row in place. Pure over `PanelData` so it is unit-testable
/// without a model tick.
pub(crate) fn apply_step(
    panel: &mut crate::panel::PanelData,
    key: &str,
    status: &str,
    detail: &str,
) {
    if let Some(row) = panel.pr_queue.iter_mut().find(|r| r.key == key) {
        row.status = status.to_string();
        row.detail = (!detail.is_empty()).then(|| detail.to_string());
    }
}

// ---------------------------------------------------------------------------
// Section keys
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PrqAction {
    /// Queue the current worktree's PR.
    Add,
    /// Drop the cursor row.
    Remove,
    /// Re-arm a settled row (`needs_human`) so the next pass looks again.
    Rewatch,
    /// Empty the repo's queue.
    Clear,
    /// Run one refresh pass now.
    Refresh,
    /// Open the cursor row's PR in a browser.
    OpenInBrowser,
}

/// The section's key table. `Err` carries the status-line hint for a key that
/// doesn't apply to the cursor row. Pure, so the hint row in
/// `panel::sections::pr_queue` can be checked against it.
pub(crate) fn row_action_for(
    key: char,
    row_status: Option<&str>,
) -> Result<PrqAction, &'static str> {
    match key {
        'a' => Ok(PrqAction::Add),
        'D' => Ok(PrqAction::Refresh),
        'c' => Ok(PrqAction::Clear),
        'x' => row_status
            .map(|_| PrqAction::Remove)
            .ok_or("PR queue: no row selected"),
        'o' => row_status
            .map(|_| PrqAction::OpenInBrowser)
            .ok_or("PR queue: no row selected"),
        'r' => match row_status {
            None => Err("PR queue: no row selected"),
            // Only a settled row needs re-arming; an active one is already
            // being looked at every pass.
            Some("needs_human" | "merged" | "closed") => Ok(PrqAction::Rewatch),
            Some(_) => Err("PR queue: this row is already being watched"),
        },
        _ => Err(""),
    }
}

/// The loop locals the section keys need, borrowed for one keypress.
pub(crate) struct PrqKeyCtx<'a> {
    pub model: &'a mut crate::chrome::FrameModel,
    pub cfg: &'a Config,
    /// The active tab's worktree path (the `a` target and the repo anchor).
    pub active_wt: PathBuf,
    pub refresh_tx: &'a tokio_mpsc::UnboundedSender<RefreshKind>,
    pub waker: &'a TerminalWaker,
    pub tx: &'a PrqTx,
    pub inflight: &'a mut bool,
    pub toasts: &'a mut Toasts,
}

/// Handle one of the section's action keys on the row under the cursor. Returns
/// whether the key was consumed.
pub(crate) fn section_key(key: char, cursor: usize, ctx: PrqKeyCtx) -> bool {
    let row: Option<PrQueueRow> = ctx.model.panel.pr_queue.get(cursor).cloned();
    let action = match row_action_for(key, row.as_ref().map(|r| r.status.as_str())) {
        Ok(a) => a,
        Err("") => return false,
        Err(hint) => {
            ctx.toasts.info_ttl(
                hint.to_string(),
                std::time::Instant::now(),
                std::time::Duration::from_secs(4),
            );
            return true;
        }
    };

    match action {
        PrqAction::Add => spawn_add(ctx.tx, ctx.waker, ctx.active_wt.clone()),
        PrqAction::Refresh => {
            if *ctx.inflight {
                ctx.toasts.info_ttl(
                    "Already refreshing…".to_string(),
                    std::time::Instant::now(),
                    std::time::Duration::from_secs(3),
                );
                return true;
            }
            *ctx.inflight = true;
            spawn_drive(ctx.tx, ctx.waker, ctx.cfg.clone(), ctx.active_wt.clone());
        }
        PrqAction::Clear => spawn_mutate(ctx.tx, ctx.waker, ctx.active_wt.clone(), Mutation::Clear),
        PrqAction::Remove => {
            if let Some(r) = row {
                spawn_mutate(
                    ctx.tx,
                    ctx.waker,
                    ctx.active_wt.clone(),
                    Mutation::Remove {
                        key: r.key,
                        number: r.number,
                    },
                );
            }
        }
        PrqAction::Rewatch => {
            if let Some(r) = row {
                spawn_mutate(
                    ctx.tx,
                    ctx.waker,
                    ctx.active_wt.clone(),
                    Mutation::Rewatch {
                        number: r.number,
                        worktree: r.worktree,
                        branch: r.branch,
                        base: r.base_branch,
                    },
                );
            }
        }
        PrqAction::OpenInBrowser => {
            if let Some(r) = row {
                let wt = r
                    .worktree
                    .clone()
                    .unwrap_or_else(|| ctx.active_wt.to_string_lossy().into_owned());
                let branch = r.branch.clone();
                tokio::task::spawn_blocking(move || {
                    let loc = thegn_core::remote::GitLoc::from_db(&wt, None);
                    // best-effort: opening a browser can fail for a dozen
                    // environmental reasons and none should disturb the loop.
                    let _ = crate::forge_handle::get()
                        .for_loc(&loc)
                        .open_in_browser(&loc, Some(&branch));
                });
            }
        }
    }
    let _ = ctx.refresh_tx;
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(key: &str, number: u64, status: &str) -> PrQueueRow {
        PrQueueRow {
            key: key.into(),
            repo_root: "/repo".into(),
            number,
            worktree: Some("/w".into()),
            branch: "feat".into(),
            base_branch: "main".into(),
            forge: "github".into(),
            status: status.into(),
            blocker: None,
            detail: None,
            agent_attempts: 0,
            last_head_oid: None,
            queued_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn queue_wide_keys_need_no_row() {
        for (k, want) in [
            ('a', PrqAction::Add),
            ('D', PrqAction::Refresh),
            ('c', PrqAction::Clear),
        ] {
            assert_eq!(row_action_for(k, None), Ok(want), "{k}");
        }
    }

    #[test]
    fn row_keys_report_an_empty_selection_rather_than_acting() {
        for k in ['x', 'o', 'r'] {
            match row_action_for(k, None) {
                Err(hint) => assert!(hint.contains("no row selected"), "{k}: {hint}"),
                Ok(a) => panic!("{k} acted with no row: {a:?}"),
            }
        }
    }

    #[test]
    fn rewatch_applies_only_to_a_settled_row() {
        for s in ["needs_human", "merged", "closed"] {
            assert_eq!(row_action_for('r', Some(s)), Ok(PrqAction::Rewatch), "{s}");
        }
        for s in ["watching", "blocked_ci", "agent_running", "ready"] {
            assert!(
                row_action_for('r', Some(s)).is_err(),
                "{s} is already watched; re-arming it is a no-op"
            );
        }
    }

    #[test]
    fn an_unbound_key_is_not_consumed() {
        // Empty hint = "not ours", so the loop can fall through to other
        // handlers rather than swallowing the key.
        assert_eq!(row_action_for('z', Some("watching")), Err(""));
        assert_eq!(row_action_for('q', None), Err(""));
    }

    #[test]
    fn every_hinted_key_is_dispatchable() {
        // The section's hint row advertises these; a key shown but not handled
        // would be a lie to the reader.
        for k in ['a', 'x', 'r', 'c', 'D', 'o'] {
            let with_row = row_action_for(k, Some("needs_human"));
            assert!(with_row.is_ok(), "{k} is advertised but not dispatchable");
        }
    }

    #[test]
    fn apply_step_patches_the_matching_row_only() {
        let mut panel = crate::panel::PanelData {
            pr_queue: vec![row("/repo#1", 1, "watching"), row("/repo#2", 2, "watching")],
            ..Default::default()
        };
        apply_step(&mut panel, "/repo#2", "blocked_ci", "failing: clippy");
        assert_eq!(panel.pr_queue[0].status, "watching", "untouched");
        assert_eq!(panel.pr_queue[1].status, "blocked_ci");
        assert_eq!(panel.pr_queue[1].detail.as_deref(), Some("failing: clippy"));
        // An empty detail clears rather than storing "".
        apply_step(&mut panel, "/repo#2", "ready", "");
        assert_eq!(panel.pr_queue[1].detail, None);
        // An unknown key is a no-op, not a panic.
        apply_step(&mut panel, "/repo#99", "merged", "x");
        assert_eq!(panel.pr_queue.len(), 2);
    }
}
