//! The two things the board cannot do for itself: sample the roster (off the
//! loop, because `Db` is not `Send` and a table read is I/O) and resolve a row
//! into somewhere the compositor can actually land.
//!
//! Both were the monitor's when the board was one of its tabs; they moved here
//! whole when the board became its own surface.

use termwiz::terminal::TerminalWaker;

use super::PipelineJump;

/// Sample the agent-dispatch roster off the loop and deliver it as
/// [`crate::hydrate::RefreshKind::Dispatches`].
///
/// Off-thread because `Db` is not `Send` and a table read is I/O — neither
/// belongs on the event loop. **Adds no wake source**: it is a one-shot task
/// that pulses the existing `TerminalWaker` once and exits, so the 0%-idle
/// contract is untouched whether the board is open or shut. Background QoS —
/// a board refresh is housekeeping, not the interactive path.
pub fn spawn_dispatch_sample(
    refresh_tx: &tokio::sync::mpsc::UnboundedSender<crate::hydrate::RefreshKind>,
    waker: &TerminalWaker,
    stage_order: Vec<String>,
) {
    let tx = refresh_tx.clone();
    let waker = waker.clone();
    tokio::task::spawn_blocking(move || {
        crate::platform::qos::set_self(crate::platform::qos::Qos::Background);
        use thegn_core::store::NotificationStore;
        // best-effort: the roster is a cache-side ledger and the board is a
        // view of it — an unavailable DB means "no update", never a crash.
        let rows = thegn_core::db::Db::open()
            .ok()
            .and_then(|db| db.list_dispatches().ok())
            .unwrap_or_default();
        let roster = crate::monitor_pipeline::DispatchRoster { rows, stage_order };
        if tx
            .send(crate::hydrate::RefreshKind::Dispatches(Box::new(roster)))
            .is_ok()
        {
            let _ = waker.wake();
        }
    });
}

/// Resolve a board-row jump into a sidebar row target.
///
/// Two tiers, in this order:
///
/// 1. **A live sidebar row** for the worktree. Reusing the sidebar's own rows as
///    the routing table (the `handlers::attention::next_target` precedent) lands
///    the worktree exactly where `↵` on its sidebar row would, cross-workspace
///    case included.
/// 2. **The registered-but-not-open case.** A worktree thegn knows about whose
///    workspace this instance has never opened has no sidebar row to answer for
///    it — but `FrameModel::sidebar_db_worktrees` knows where it lives, and the
///    dormant-workspace switch target synthesized from it
///    (`sidebar::worktree_groups`) flows through the same
///    `handlers::sidebar_activate::activate_row_target` door. Without this tier
///    the board could only report "no open worktree for …" about a worktree it
///    was perfectly able to open.
///
/// `None` only when both miss (the worktree was deleted under the board, or was
/// never registered); the caller says so rather than doing nothing silently.
///
/// Pane-level focus (jumping to the *session* running the stage, not just its
/// worktree) is phase 2: [`PipelineJump::session`] is carried for it and
/// deliberately unused here.
pub fn pipeline_target(
    jump: &PipelineJump,
    model: &crate::chrome::FrameModel,
) -> Option<crate::sidebar::RowTarget> {
    let live = model
        .sidebar_rows
        .iter()
        .find(|r| {
            r.kind == crate::sidebar::RowKind::Worktree
                && r.worktree_path.as_deref() == Some(jump.worktree.as_str())
                && r.tab_target.is_some()
        })
        .and_then(|r| r.tab_target.clone());
    if live.is_some() {
        return live;
    }
    model
        .sidebar_db_worktrees
        .iter()
        .find(|w| w.path == jump.worktree)
        .map(|w| crate::sidebar::RowTarget::Workspace {
            repo_path: w.repo_path.clone(),
            group: Some(w.tab_name.clone()),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chrome::FrameModel;
    use crate::sidebar::{DbWorktree, RowKind, RowTarget, SidebarRow};

    fn jump(path: &str) -> PipelineJump {
        PipelineJump {
            worktree: path.into(),
            session: Some("s-1".into()),
        }
    }

    fn db_row(path: &str) -> DbWorktree {
        DbWorktree {
            slug: "app".into(),
            branch: "feat".into(),
            repo_path: "/repo/app".into(),
            tab_name: "app/feat".into(),
            path: path.into(),
            folder_id: None,
            sandbox_backend: None,
            env_name: None,
            env_degraded: false,
        }
    }

    #[test]
    fn tier_one_resolves_a_live_worktree_row_to_its_tab_target() {
        let mut model = FrameModel::default();
        model.sidebar_rows.push(SidebarRow {
            worktree_path: Some("/wt/a".into()),
            tab_target: Some(RowTarget::Tab(2, 1)),
            ..SidebarRow::base(RowKind::Worktree, 1, "a", "app")
        });
        assert_eq!(
            pipeline_target(&jump("/wt/a"), &model),
            Some(RowTarget::Tab(2, 1))
        );
    }

    #[test]
    fn tier_two_opens_a_registered_worktree_that_has_no_row_yet() {
        // The gap this tier closes: a worktree in a workspace this instance has
        // never opened. It has no sidebar row, so tier 1 misses — but thegn
        // knows exactly where it is, and the dormant-workspace switch is a real
        // target, so the board opens it instead of reporting a miss.
        let mut model = FrameModel::default();
        model.sidebar_db_worktrees.push(db_row("/wt/dormant"));
        assert_eq!(
            pipeline_target(&jump("/wt/dormant"), &model),
            Some(RowTarget::Workspace {
                repo_path: "/repo/app".into(),
                group: Some("app/feat".into()),
            })
        );
    }

    #[test]
    fn tier_one_wins_when_the_worktree_is_both_open_and_registered() {
        // Every worktree with a live row is ALSO in the DB list; landing on the
        // open tab must beat re-switching to its workspace.
        let mut model = FrameModel::default();
        model.sidebar_rows.push(SidebarRow {
            worktree_path: Some("/wt/a".into()),
            tab_target: Some(RowTarget::Tab(2, 1)),
            ..SidebarRow::base(RowKind::Worktree, 1, "a", "app")
        });
        model.sidebar_db_worktrees.push(db_row("/wt/a"));
        assert_eq!(
            pipeline_target(&jump("/wt/a"), &model),
            Some(RowTarget::Tab(2, 1))
        );
    }

    #[test]
    fn both_tiers_missing_resolves_to_nothing() {
        let mut model = FrameModel::default();
        // Right path, but no target to land on (a collapsed-parent placeholder)
        // and no DB row either.
        model.sidebar_rows.push(SidebarRow {
            worktree_path: Some("/wt/a".into()),
            tab_target: None,
            ..SidebarRow::base(RowKind::Worktree, 1, "a", "app")
        });
        // Right target, wrong kind — a workspace row must not answer for a
        // worktree jump.
        model.sidebar_rows.push(SidebarRow {
            worktree_path: Some("/wt/b".into()),
            tab_target: Some(RowTarget::Tab(0, 0)),
            ..SidebarRow::base(RowKind::Workspace, 0, "b", "app")
        });
        assert_eq!(pipeline_target(&jump("/wt/a"), &model), None);
        assert_eq!(pipeline_target(&jump("/wt/b"), &model), None);
        assert_eq!(pipeline_target(&jump("/wt/zz"), &model), None);
    }
}
