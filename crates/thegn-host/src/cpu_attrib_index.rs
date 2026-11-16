//! Pane → tab → worktree → workspace index for the CPU/memory attribution
//! popup (THE-2025). The session has no single index from a pane id to where
//! it lives, so this walks the active session and every parked workspace once:
//! O(panes), no I/O, no locks. Built only while the popup is open.
use thegn_core::cpu_attrib::PaneOwner;

use crate::panes::Panes;
use crate::session::{GroupKind, Session, WorktreeGroup};
use crate::workspace_pool::WorkspacePool;

/// Workspace label for terminal groups: they are a global region rather than
/// part of any repo, so they get their own bucket.
const TERMINALS: &str = "terminals";

/// The short workspace name for a repo key (`Session::id`, a repo path).
fn workspace_label(repo: &str) -> String {
    std::path::Path::new(repo)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| repo.to_string())
}

fn push_group(out: &mut Vec<PaneOwner>, workspace: &str, group: &WorktreeGroup, panes: &Panes) {
    let workspace = if group.kind == GroupKind::Terminal {
        TERMINALS.to_string()
    } else {
        workspace.to_string()
    };
    for tab in &group.tabs {
        for pane_id in tab.center.pane_ids() {
            let program = panes
                .table
                .get(&pane_id)
                .map(|pane| pane.program().to_string())
                .filter(|program| !program.is_empty());
            out.push(PaneOwner {
                pane_id,
                workspace: workspace.clone(),
                worktree: group.name.clone(),
                worktree_path: (!group.path.is_empty()).then(|| group.path.clone()),
                tab: tab.title.clone(),
                program,
            });
        }
    }
}

/// Every pane thegn holds — the active session's and every parked
/// workspace's — with the workspace, worktree, tab and program it belongs to.
pub(crate) fn pane_owners(
    session: &Session,
    pool: &WorkspacePool,
    panes: &Panes,
) -> Vec<PaneOwner> {
    let mut out = Vec::new();
    let active = workspace_label(&session.id);
    for group in &session.worktrees {
        push_group(&mut out, &active, group, panes);
    }
    for (repo, group) in pool.parked_groups() {
        push_group(&mut out, &workspace_label(repo), group, panes);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{TERMINALS, pane_owners, workspace_label};
    use crate::center::CenterTree;
    use crate::panes::Panes;
    use crate::session::{GroupKind, Session, Tab, WorktreeGroup};
    use crate::workspace_pool::{ResidentWorkspace, WorkspacePool};

    fn panes() -> Panes {
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        Panes::new(tx)
    }

    fn group(name: &str, kind: GroupKind, ids: &[u32]) -> WorktreeGroup {
        let mut group = WorktreeGroup::new(name, kind, "/repo");
        group.tabs = ids
            .iter()
            .enumerate()
            .map(|(i, id)| {
                let mut tab = Tab::new((i + 1).to_string());
                tab.center = CenterTree::Leaf(*id);
                tab.focused_pane = *id;
                tab
            })
            .collect();
        group
    }

    #[test]
    fn indexes_active_worktrees_and_parked_workspaces() {
        let mut p = panes();
        let session = Session {
            id: "/home/u/code/alpha".into(),
            worktrees: vec![
                group("alpha", GroupKind::Home, &[1, 2]),
                group("alpha/feat", GroupKind::Branch, &[3]),
                group("prod", GroupKind::Terminal, &[9]),
            ],
            active: 0,
        };
        let mut pool = WorkspacePool::default();
        pool.stash(
            "/home/u/code/beta".into(),
            ResidentWorkspace {
                worktrees: vec![group("beta", GroupKind::Home, &[7])],
                active: 0,
            },
            &mut p,
        );
        let owners = pane_owners(&session, &pool, &p);
        let rows: Vec<(u32, &str, &str, &str)> = owners
            .iter()
            .map(|o| {
                (
                    o.pane_id,
                    o.workspace.as_str(),
                    o.worktree.as_str(),
                    o.tab.as_str(),
                )
            })
            .collect();
        assert_eq!(
            rows,
            vec![
                (1, "alpha", "alpha", "1"),
                (2, "alpha", "alpha", "2"),
                (3, "alpha", "alpha/feat", "1"),
                (9, TERMINALS, "prod", "1"),
                (7, "beta", "beta", "1"),
            ]
        );
        // No live PtyPane in the table: the program is unknown, not "".
        assert!(owners.iter().all(|o| o.program.is_none()));
    }

    #[test]
    fn workspace_label_is_the_repo_basename() {
        assert_eq!(workspace_label("/home/u/code/alpha"), "alpha");
        assert_eq!(workspace_label("/home/u/code/alpha/"), "alpha");
        assert_eq!(workspace_label("/"), "/");
        assert_eq!(workspace_label(""), "");
    }
}
