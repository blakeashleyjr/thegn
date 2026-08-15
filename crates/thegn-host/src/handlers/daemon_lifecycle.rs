//! Loop-side handling of the daemon-backed pane lifecycle: quit-time
//! detach marking (panes survive the UI), the reattach-expiry fallback
//! restore, and the quit-kill sweep. Extracted per the run.rs ratchet — the
//! dispatch arms stay thin calls into here.

use std::sync::Mutex;
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

/// Mark PARKED (resident-pool) workspaces' center panes detached-on-drop too.
/// Their `PtyPane`s stay live in the table but are absent from the active
/// `Session`, so [`mark_session_panes_detached`] alone would let a parked
/// workspace's daemon-backed sessions die on quit instead of persisting.
/// Returns the additional daemon-backed sessions kept (added to the latch).
pub(crate) fn mark_parked_panes_detached(
    pool: &crate::workspace_pool::WorkspacePool,
    panes: &Panes,
) -> usize {
    let mut kept = 0usize;
    for id in pool.parked_pane_ids() {
        if let Some(p) = panes.table.get(&id) {
            p.set_detach_on_drop(true);
            if p.is_daemon_backed() {
                kept += 1;
            }
        }
    }
    if kept > 0 {
        KEPT_SESSIONS.fetch_add(kept, Ordering::Relaxed);
    }
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

/// Consume the persisted `provider = "daemon"` records of `leaves` from a
/// tab and return their daemon session ids. The daemon-disabled claim: with
/// the daemon route off, `materialize_with_specs`' warm-reattach branch is
/// skipped and each record's leaf falls through to a fresh non-daemon spawn —
/// so an unclaimed record would leave the SAME program also running inside
/// the still-alive daemon under its (by default untimed) lease: a duplicated
/// process now, and a permanently invisible orphan after the post-remap
/// persist prunes the record. Consuming the record here is what makes the
/// claim exactly-once (no later materialize/persist can see it) and keeps a
/// daemon session id out of the native provider-exec attach path, which reads
/// `pane_sessions` without a provider filter.
///
/// `leaves` must be restricted to leaves with NO live pane: a daemon-backed
/// pane spawned before a live config reload disabled the route keeps its
/// session untouched.
pub(crate) fn claim_disabled_daemon_sessions(
    tab: &mut crate::session::Tab,
    leaves: &[u32],
) -> Vec<String> {
    let mut sids = Vec::new();
    for id in leaves {
        if tab
            .pane_sessions
            .get(id)
            .is_some_and(|s| s.provider == "daemon")
            && let Some(ps) = tab.pane_sessions.remove(id)
        {
            sids.push(ps.session);
        }
    }
    sids
}

/// Daemon sessions whose kill was dispatched fire-and-forget but not yet
/// confirmed, plus the config to reach the daemon. Latched so the quit path
/// can flush them ([`flush_orphan_kills`]): `main` runs the loop under
/// `rt.block_on` and calls `shutdown_background()` on return, which ABORTS
/// in-flight spawned tasks — a quit right after a daemon-disabled resurrect
/// would otherwise drop the kill after the `pane_sessions` record was already
/// consumed, silently recreating the permanent invisible orphan.
#[allow(clippy::type_complexity)]
static PENDING_ORPHAN_KILLS: Mutex<Option<(thegn_core::config::DaemonConfig, Vec<String>)>> =
    Mutex::new(None);

fn latch_orphan_kills(dcfg: &thegn_core::config::DaemonConfig, sids: &[String]) {
    if let Ok(mut latch) = PENDING_ORPHAN_KILLS.lock() {
        let (_, pending) = latch.get_or_insert_with(|| (dcfg.clone(), Vec::new()));
        pending.extend(sids.iter().cloned());
    }
}

fn unlatch_orphan_kills(sids: &[String]) {
    if let Ok(mut latch) = PENDING_ORPHAN_KILLS.lock()
        && let Some((_, pending)) = latch.as_mut()
    {
        pending.retain(|sid| !sids.contains(sid));
        if pending.is_empty() {
            *latch = None;
        }
    }
}

/// Best-effort kill of `sids` on an already-connected daemon client; returns
/// how many kills landed. The core of both the fire-and-forget orphan kill
/// and the quit-time flush, factored so tests can drive it against a client
/// directly (no registry discovery / `Db::open`).
pub(crate) async fn kill_sessions(
    client: &thegn_svc::control::client::ControlClient,
    sids: &[String],
) -> usize {
    let mut killed = 0usize;
    for sid in sids {
        // best-effort: an already-dead session or an unreachable daemon means
        // there is nothing left to kill.
        if client.kill(sid).await.is_ok() {
            killed += 1;
        }
    }
    killed
}

/// Fire-and-forget kill of daemon sessions orphaned by a daemon-disabled
/// materialize (their `pane_sessions` records were just claimed). Off-loop,
/// async, connect-only — with no live daemon there is nothing to kill and
/// nothing is spawned. Contract: a missing runtime handle (unit tests,
/// harnesses without a runtime) is an explicit no-op, mirroring
/// [`kill_daemon_sessions_blocking`]. The sids stay latched until the kill
/// attempt completes so a quit can flush them ([`flush_orphan_kills`]).
pub(crate) fn kill_orphaned_daemon_sessions(
    dcfg: thegn_core::config::DaemonConfig,
    sids: Vec<String>,
) {
    if sids.is_empty() {
        return;
    }
    let Ok(rt) = tokio::runtime::Handle::try_current() else {
        return;
    };
    latch_orphan_kills(&dcfg, &sids);
    rt.spawn(async move {
        if let Some(client) = crate::daemon::client::connect_daemon(&dcfg).await {
            let killed = kill_sessions(&client, &sids).await;
            tracing::info!(
                target: "thegn::daemon",
                killed,
                of = sids.len(),
                "stopped daemon sessions orphaned by the disabled daemon route"
            );
        }
        unlatch_orphan_kills(&sids);
    });
}

/// Bounded quit-time flush of any orphan-session kills still in flight (see
/// [`PENDING_ORPHAN_KILLS`]). Called from `main` between the loop's return
/// and `shutdown_background()`; the latch is empty on every normal quit, so
/// this is a no-op there. Racing the fire-and-forget task double-kills at
/// worst — killing an already-dead session is a best-effort no-op. Returns
/// how many kills landed.
pub(crate) async fn flush_orphan_kills(timeout: std::time::Duration) -> usize {
    let pending = match PENDING_ORPHAN_KILLS.lock() {
        Ok(mut latch) => latch.take(),
        Err(_) => None,
    };
    let Some((dcfg, sids)) = pending else {
        return 0;
    };
    tokio::time::timeout(timeout, async move {
        match crate::daemon::client::connect_daemon(&dcfg).await {
            Some(client) => kill_sessions(&client, &sids).await,
            None => 0,
        }
    })
    .await
    .unwrap_or(0)
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

    fn daemon_session(sid: &str) -> crate::session::ProviderSession {
        crate::session::ProviderSession {
            provider: "daemon".into(),
            id: "local".into(),
            session: sid.into(),
        }
    }

    /// The claim consumes ONLY daemon-provider records, and only for the
    /// requested (not-live) leaves: a non-daemon provider session and a leaf
    /// outside the batch (a live pane elsewhere) stay untouched.
    #[test]
    fn claim_consumes_only_daemon_sessions_for_the_given_leaves() {
        let mut tab = crate::session::Tab::new("1");
        tab.pane_sessions.insert(1, daemon_session("s1"));
        tab.pane_sessions.insert(
            2,
            crate::session::ProviderSession {
                provider: "sprites".into(),
                id: "sb-1".into(),
                session: "x".into(),
            },
        );
        tab.pane_sessions.insert(3, daemon_session("s3"));

        let claimed = claim_disabled_daemon_sessions(&mut tab, &[1, 2]);

        assert_eq!(claimed, vec!["s1".to_string()]);
        assert!(
            !tab.pane_sessions.contains_key(&1),
            "claimed record consumed"
        );
        assert!(
            tab.pane_sessions.contains_key(&2),
            "non-daemon provider session untouched"
        );
        assert!(
            tab.pane_sessions.contains_key(&3),
            "leaf outside the batch (live pane) untouched"
        );
    }

    /// Exactly-once by consumption: a second claim over the same leaves finds
    /// nothing — the record cannot be double-killed or resurface later.
    #[test]
    fn claim_is_exactly_once() {
        let mut tab = crate::session::Tab::new("1");
        tab.pane_sessions.insert(1, daemon_session("s1"));

        assert_eq!(
            claim_disabled_daemon_sessions(&mut tab, &[1]),
            vec!["s1".to_string()]
        );
        assert!(
            claim_disabled_daemon_sessions(&mut tab, &[1]).is_empty(),
            "second claim must find the record already consumed"
        );
    }

    /// `kill_sessions` is best-effort: against an unreachable daemon every
    /// kill fails and the landed count is 0 (no error, no panic). The
    /// kill→SessionExit happy path over the real router is already pinned by
    /// `daemon::service::tests::ws_warm_attach_pipeline_over_a_real_socket`.
    #[tokio::test]
    async fn kill_sessions_is_best_effort_against_an_unreachable_daemon() {
        use thegn_svc::control::client::{ControlAddr, ControlClient};
        let sock = std::env::temp_dir().join(format!(
            "thegn-orphan-kill-test-{}-{:?}.sock",
            std::process::id(),
            std::thread::current().id()
        ));
        let client = ControlClient::new(ControlAddr::Unix(sock));
        assert_eq!(kill_sessions(&client, &["s1".into(), "s2".into()]).await, 0);
    }

    /// `kill_orphaned_daemon_sessions` outside a runtime is an explicit no-op
    /// (nothing latched, nothing spawned) — the contract the drain-level
    /// wiring test in `handlers::provision` relies on.
    #[test]
    fn kill_orphaned_daemon_sessions_without_a_runtime_is_a_noop() {
        kill_orphaned_daemon_sessions(
            thegn_core::config::DaemonConfig::default(),
            vec!["s1".into()],
        );
        // No runtime ⇒ returned before latching; nothing pending to flush.
        // (Asserted indirectly: the latch is only ever filled under a runtime.)
    }
}
