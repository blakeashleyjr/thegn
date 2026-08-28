//! Attach-on-open: a worktree tab's missing panes open onto the worktree's
//! LIVE daemon agent sessions instead of blank shells (THE-85).
//!
//! # Why this exists
//!
//! `thegn session open --agent <role> --worktree W` opens a headless daemon
//! session whose `SessionInfo.worktree` carries the worktree path. The
//! compositor knew nothing of it: opening that worktree spawned a fresh login
//! shell per missing leaf (the "blank tab where my running agent should be"
//! bug), and the session stayed invisible until a manual `thegn attach`. The
//! warm-reattach branch of `materialize_with_specs` only covers sessions a
//! pane has *persisted* (`pane_sessions`) — a CLI-opened session has no such
//! record, which is the only reason it was invisible.
//!
//! # The door it reuses
//!
//! There is exactly ONE way a daemon session becomes a pane:
//! [`crate::panes::Panes::spawn_daemon_backed`] with `attach = Some(session)`.
//! The probe here rides the existing spec pipeline: the off-loop workers
//! (materialize + prewarm) list the daemon's sessions beside the launch-spec
//! resolve they already own and ship the live targets on the same
//! `SpecBatch`; the loop-side drain plans them onto the tab's missing leaves
//! and `materialize_with_specs` attaches through that one door — so an
//! attached pane is byte-for-byte an ordinary daemon pane (same relay, same
//! reconnect ladder, same `SessionFallback` degrade). Surplus sessions (more
//! live agents than missing leaves) graft in as splits via
//! [`super::adopt::graft`] — the `--adopt` mechanism — capped at the per-tab
//! pane limit, with the overflow named in the status line rather than
//! silently dropped.
//!
//! Everything side-effecting lives in the workers and the drain; this
//! module's decision logic ([`live_for_worktree`], [`plan`]) is pure and
//! unit-tested.

use crate::compositor::Rect;
use crate::panes::Panes;
use crate::session::Session;
use thegn_svc::control::SessionInfo;

/// Per-tab pane cap — mirrors the local `MAX_PANES` consts in `run.rs`'s
/// new-pane/split handlers (left in place; this copy feeds the attach budget
/// so surplus grafts cannot crowd a tab past the same limit).
pub(crate) const MAX_PANES_PER_TAB: usize = 16;

/// One live daemon session worth attaching to: the session id for the
/// `ExecOpen::Attach` door, and the program the daemon recorded at open (the
/// agent's name) for the pane label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AttachTarget {
    pub session: String,
    pub program: String,
}

/// The attach decision for one batch: which live session takes which missing
/// leaf, and which ones split in after materialize.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AttachPlan {
    /// `(leaf id, target)` — rides into `materialize_with_specs`, which
    /// attaches through the leaf's normal daemon branch.
    pub assignments: Vec<(u32, AttachTarget)>,
    /// Beyond the missing leaves, within the split budget: grafted in as
    /// splits after materialize, newest first.
    pub surplus: Vec<AttachTarget>,
}

/// Live (un-exited, not already shown) daemon sessions for ONE worktree,
/// newest first. Pure.
///
/// `shown` is every daemon session id a pane of this compositor is already
/// displaying — the `AlreadyShown` rule (`super::adopt`): a session is never
/// attached twice. The workers probe with an empty `shown` (they cannot see
/// this process's panes); the drain-side plan re-dedups against
/// `panes.table` before planning.
pub(crate) fn live_for_worktree(
    sessions: &[SessionInfo],
    worktree: &str,
    shown: &[String],
) -> Vec<AttachTarget> {
    let mut rows: Vec<&SessionInfo> = sessions
        .iter()
        // Tombstones (finished sessions held readable for their roster) are
        // not attachable: `exited_at_ms` set means the child is gone.
        .filter(|s| s.exited_at_ms.is_none())
        .filter(|s| s.worktree.as_deref() == Some(worktree))
        .filter(|s| !shown.contains(&s.id))
        .collect();
    // Newest first: the most recent session is the one most likely still
    // streaming, so it wins the primary leaf. Stable sort — equal timestamps
    // keep the daemon's roster order.
    rows.sort_by_key(|s| std::cmp::Reverse(s.created_at_ms));
    rows.into_iter()
        .map(|s| AttachTarget {
            session: s.id.clone(),
            program: s.program.clone(),
        })
        .collect()
}

/// Zip the newest targets onto the missing leaves, in order; the rest are
/// surplus, capped at `max_new_panes` — how many panes the attach may still
/// ADD beyond the tab's post-materialize tree. The caller passes the tab's
/// remaining headroom under [`MAX_PANES_PER_TAB`], so
/// `existing + assignments + surplus <= MAX_PANES_PER_TAB` holds (assignments
/// ride leaves the tree already counts). Pure.
pub(crate) fn plan(leaves: &[u32], targets: Vec<AttachTarget>, max_new_panes: usize) -> AttachPlan {
    let take = leaves.len().min(targets.len());
    let assignments = leaves
        .iter()
        .zip(targets.iter())
        .map(|(leaf, t)| (*leaf, t.clone()))
        .collect();
    let mut surplus: Vec<AttachTarget> = targets.into_iter().skip(take).collect();
    surplus.truncate(max_new_panes);
    AttachPlan {
        assignments,
        surplus,
    }
}

/// Probe the daemon for `worktree`'s live sessions. **Connect-only** —
/// [`crate::daemon::client::connect_daemon`], never `ensure_daemon`: asking a
/// question must not spawn a daemon as a side effect. Any failure (no daemon,
/// socket error, a session list that won't decode) yields an empty vec —
/// fresh shells are the honest fallback; logged at debug.
///
/// The spec workers call this off-loop inside the ambient runtime:
/// `handle.block_on(probe(...))` from their `spawn_blocking` closure.
pub(crate) async fn probe(
    dcfg: &thegn_core::config::DaemonConfig,
    worktree: &str,
    shown: Vec<String>,
) -> Vec<AttachTarget> {
    let Some(client) = crate::daemon::client::connect_daemon(dcfg).await else {
        tracing::debug!(
            target: "thegn::daemon",
            worktree = %worktree,
            "attach probe: no live daemon; worktree opens on fresh shells"
        );
        return Vec::new();
    };
    match client.sessions().await {
        Ok(sessions) => live_for_worktree(&sessions, worktree, &shown),
        Err(e) => {
            tracing::debug!(
                target: "thegn::daemon",
                worktree = %worktree,
                "attach probe failed; worktree opens on fresh shells: {e}"
            );
            Vec::new()
        }
    }
}

/// Graft the plan's surplus sessions into tab `(gi, ti)` as splits, newest
/// first — [`super::adopt::graft`], the `--adopt` mechanism, reused so the
/// split is again an ordinary daemon pane. Returns how many landed.
pub(crate) fn graft_surplus(
    surplus: &[AttachTarget],
    gi: usize,
    ti: usize,
    session: &mut Session,
    panes: &mut Panes,
    cfg: &thegn_core::config::Config,
    center: Rect,
) -> usize {
    let mut landed = 0usize;
    for target in surplus {
        if super::adopt::graft(&target.session, gi, ti, session, panes, cfg, center) {
            landed += 1;
        }
    }
    landed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sess(id: &str, wt: &str, created_ms: i64) -> SessionInfo {
        SessionInfo {
            id: id.to_string(),
            worktree: Some(wt.to_string()),
            program: "claude".into(),
            created_at_ms: created_ms,
            ..Default::default()
        }
    }

    fn ids(targets: &[AttachTarget]) -> Vec<&str> {
        targets.iter().map(|t| t.session.as_str()).collect()
    }

    #[test]
    fn only_live_sessions_of_the_named_worktree_survive() {
        let mut exited = sess("exited", "/wt/a", 2_000);
        exited.exited_at_ms = Some(4_000);
        let sessions = vec![
            sess("live", "/wt/a", 1_000),
            exited,
            sess("other-wt", "/wt/b", 3_000),
        ];
        assert_eq!(
            ids(&live_for_worktree(&sessions, "/wt/a", &[])),
            vec!["live"],
            "tombstones and other worktrees' sessions are not attachable"
        );
    }

    #[test]
    fn newest_session_first() {
        let sessions = vec![
            sess("older", "/wt/a", 1_000),
            sess("newest", "/wt/a", 9_000),
            sess("middle", "/wt/a", 5_000),
        ];
        assert_eq!(
            ids(&live_for_worktree(&sessions, "/wt/a", &[])),
            vec!["newest", "middle", "older"],
            "the most recent session wins the primary leaf"
        );
    }

    #[test]
    fn already_shown_sessions_are_not_attached_twice() {
        let sessions = vec![
            sess("shown", "/wt/a", 1_000),
            sess("hidden", "/wt/a", 2_000),
        ];
        assert_eq!(
            ids(&live_for_worktree(
                &sessions,
                "/wt/a",
                &["shown".to_string()]
            )),
            vec!["hidden"],
            "the AlreadyShown rule"
        );
    }

    #[test]
    fn plan_assigns_newest_target_to_the_first_leaf() {
        // Targets arrive newest-first from `live_for_worktree`; the plan zips
        // them onto the leaves in order.
        let targets = vec![
            AttachTarget {
                session: "t0".into(),
                program: "pi".into(),
            },
            AttachTarget {
                session: "t1".into(),
                program: "pi".into(),
            },
        ];
        let plan = plan(&[7u32, 9], targets, 16);
        assert_eq!(plan.assignments.len(), 2, "one target per missing leaf");
        assert_eq!(plan.assignments[0].0, 7, "newest target → first leaf");
        assert_eq!(plan.assignments[0].1.session, "t0");
        assert_eq!(plan.assignments[1].0, 9);
        assert!(plan.surplus.is_empty(), "no targets left over");
    }

    #[test]
    fn plan_caps_surplus_at_the_pane_budget() {
        let targets: Vec<AttachTarget> = (0..5)
            .map(|i| AttachTarget {
                session: format!("t{i}"),
                program: "pi".into(),
            })
            .collect();
        // One missing leaf, budget for two NEW panes: the newest takes the
        // leaf, exactly `max_new_panes` more split in, the overflow is the
        // caller's to name in the status line.
        let plan = plan(&[3u32], targets, 2);
        assert_eq!(plan.assignments.len(), 1);
        assert_eq!(plan.assignments[0].1.session, "t0");
        assert_eq!(
            ids(&plan.surplus),
            vec!["t1", "t2"],
            "surplus is newest-first and truncated to the budget"
        );
    }

    #[test]
    fn plan_handles_empty_leaves_and_empty_targets() {
        // No missing leaves: everything is surplus (within budget).
        let no_leaves = plan(
            &[],
            vec![AttachTarget {
                session: "t0".into(),
                program: "pi".into(),
            }],
            3,
        );
        assert!(no_leaves.assignments.is_empty());
        assert_eq!(ids(&no_leaves.surplus), vec!["t0"]);
        // No targets: nothing to do.
        let no_targets = plan(&[1u32, 2], Vec::new(), 3);
        assert!(no_targets.assignments.is_empty() && no_targets.surplus.is_empty());
        // Zero budget: the primary leaf still gets the newest session; no
        // splits.
        let no_budget = plan(
            &[1u32],
            vec![AttachTarget {
                session: "t0".into(),
                program: "pi".into(),
            }],
            0,
        );
        assert_eq!(no_budget.assignments.len(), 1);
        assert!(no_budget.surplus.is_empty());
    }
}
