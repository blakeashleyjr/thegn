//! Build the worktree list the activity FSM tracks (see `superzej_core::activity`).

use superzej_core::activity::ManagedWorktree;
use superzej_core::store::WorkspaceStore;

/// Every registered worktree the activity FSM should track, keyed by path so a
/// fresh session tab overlays its persisted DB row. `has_agent` (the DB `agent`
/// column, non-empty only where an agent attached) gates the FSM: a non-agent
/// worktree passes through but is held at `none`, so the sidebar dot is an
/// agent-attention signal, not a generic "CPU used here" light — a plain shell's
/// incidental CPU can no longer flap it between `active`/`waiting`/`read`.
pub(crate) fn managed_worktrees(
    session: &crate::session::Session,
    db: &superzej_core::db::Db,
) -> Vec<ManagedWorktree> {
    let mut map = std::collections::BTreeMap::new();
    if let Ok(rows) = db.worktrees() {
        for wt in rows {
            if !wt.worktree.is_empty() {
                map.insert(
                    wt.worktree.clone(),
                    ManagedWorktree {
                        has_agent: !wt.agent.is_empty(),
                        worktree: wt.worktree.clone(),
                        tab: wt.tab_name.clone(),
                    },
                );
            }
        }
    }
    // Session may carry unpersisted fresh tabs; its tab name wins, but keep the
    // DB agent flag on any row it overwrites (a fresh tab has no agent yet).
    for g in &session.worktrees {
        if !g.path.is_empty() {
            let has_agent = map.get(&g.path).is_some_and(|m| m.has_agent);
            map.insert(
                g.path.clone(),
                ManagedWorktree {
                    has_agent,
                    worktree: g.path.clone(),
                    tab: g.name.clone(),
                },
            );
        }
    }
    map.into_values().collect()
}
