//! Loop-side handling of the daemon-backed pane lifecycle: quit-time
//! detach marking (panes survive the UI), the reattach-expiry fallback
//! restore, and the quit-kill sweep. Extracted per the run.rs ratchet — the
//! dispatch arms stay thin calls into here.

use std::sync::atomic::{AtomicUsize, Ordering};

use crate::panes::Panes;
use crate::session::Session;

/// Daemon-backed sessions deliberately left running by the last quit. Written
/// by [`mark_session_panes_detached`], read by `main` AFTER the compositor
/// tears down the alt screen — the "kept N sessions running" exit line can't
/// be printed from inside the raw-mode UI.
static KEPT_SESSIONS: AtomicUsize = AtomicUsize::new(0);

/// Sessions kept running by the quit that just returned (0 = none).
pub(crate) fn kept_sessions() -> usize {
    KEPT_SESSIONS.load(Ordering::Relaxed)
}

/// Quit is a detach, not a kill: mark every **center-tree** pane
/// detached-on-drop so daemon-backed sessions keep running and the next
/// launch warm-reattaches them. Ephemeral panes (pins/drawer/corner) are
/// in-process and die with the compositor regardless; anything else falling
/// off the table keeps the kill-on-drop default. Returns the number of
/// daemon-backed sessions being kept (also latched for the exit message).
pub(crate) fn mark_session_panes_detached(session: &Session, panes: &Panes) -> usize {
    let mut kept = 0usize;
    for g in &session.worktrees {
        for tab in &g.tabs {
            for id in tab.center.pane_ids() {
                if let Some(p) = panes.table.get(&id) {
                    p.set_detach_on_drop(true);
                    if p.is_daemon_backed() {
                        kept += 1;
                    }
                }
            }
        }
    }
    KEPT_SESSIONS.store(kept, Ordering::Relaxed);
    kept
}

/// Quit-and-kill: best-effort kill of every daemon-backed session owned by a
/// live pane, waited on (bounded) so the kills land before the process exits
/// — the post-return `shutdown_background()` would abort fire-and-forget
/// tasks. Runs ON the loop thread, but only on the quit path: a one-time,
/// bounded wait, not a steady-state stall. Returns how many kills landed.
pub(crate) fn kill_daemon_sessions_blocking(
    panes: &Panes,
    dcfg: &thegn_core::config::DaemonConfig,
    timeout: std::time::Duration,
) -> usize {
    let sids: Vec<String> = panes
        .table
        .values()
        .filter(|p| p.is_daemon_backed())
        .filter_map(|p| p.provider_session().map(|ps| ps.session))
        .collect();
    if sids.is_empty() {
        return 0;
    }
    let Ok(rt) = tokio::runtime::Handle::try_current() else {
        return 0;
    };
    let (done_tx, done_rx) = std::sync::mpsc::channel::<usize>();
    let dcfg = dcfg.clone();
    // Multi-thread runtime: the task runs on a worker while this thread waits.
    rt.spawn(async move {
        let mut killed = 0usize;
        // Connect-only: with no live daemon there is nothing to kill, and
        // spawning one here as a side effect would be absurd.
        if let Some(client) = crate::daemon::client::connect_daemon(&dcfg).await {
            for sid in &sids {
                if client.kill(sid).await.is_ok() {
                    killed += 1;
                }
            }
        }
        let _ = done_tx.send(killed);
    });
    done_rx.recv_timeout(timeout).unwrap_or(0)
}

/// A daemon pane's warm reattach found its persisted session gone (lease
/// expired / daemon restarted — e.g. after a reboot) and the relay degraded
/// to a fresh session. Apply the pane's stashed restore payload: repaint the
/// persisted scrollback tail and arm the relaunch overlay for the recorded
/// foreground command — the same shape the host-pane resurrect path gives.
pub(crate) fn handle_session_fallback(ctx: &mut crate::pty_drain::DrainCtx<'_>, id: u32) {
    let Some(p) = ctx.panes.table.get_mut(&id) else {
        return;
    };
    let restore = p.take_fallback_restore();
    let mut relaunch = None;
    if let Some(r) = restore {
        if !r.scrollback.is_empty() {
            p.repaint_scrollback(&r.scrollback);
        }
        relaunch = r.relaunch.filter(|s| !s.is_empty());
        if let Some(cmd) = relaunch.clone() {
            p.set_pending_relaunch(Some(cmd));
        }
    }
    ctx.model.status = if relaunch.is_some() {
        "Persistent session expired; press Enter to relaunch (Esc for a shell)".into()
    } else {
        "Persistent session expired; opened a fresh shell".into()
    };
    if ctx.visible.contains(&id) {
        ctx.dirty_panes.insert(id);
    }
    // The status line (and a possible relaunch overlay) are chrome.
    *ctx.dirty = true;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pane::PaneEvent;
    use crate::session::{GroupKind, WorktreeGroup};
    use tokio::sync::mpsc as tokio_mpsc;

    #[test]
    fn mark_detached_touches_center_panes_only_and_counts_daemon_backed() {
        let mut session = Session {
            id: "s1".into(),
            worktrees: vec![WorktreeGroup::new("app/home", GroupKind::Home, "/tmp/app")],
            active: 0,
        };
        let (tx, _rx) = tokio_mpsc::channel::<PaneEvent>(16);
        let panes = Panes::new(tx);
        // No live panes → nothing kept; the latch still records the count.
        session.worktrees[0].tabs[0].center = crate::center::CenterTree::Leaf(1);
        assert_eq!(mark_session_panes_detached(&session, &panes), 0);
        assert_eq!(kept_sessions(), 0);
    }
}
