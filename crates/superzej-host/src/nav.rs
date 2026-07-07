//! Connect-to-root — the sesh-`root` jump: resolve the focused pane's cwd to
//! the git worktree that owns it and reveal that worktree's tab. The git
//! resolution lives in core (`repo::worktree_root_for_cwd`); this module is
//! the pure decision of *where that root lands* in the one-session model
//! (already-open tab, cold worktree, workspace, or nothing registered), so the
//! `run.rs` dispatch arm stays a thin act-on-target match.

use std::path::Path;

/// Where a resolved worktree root lands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ConnectTarget {
    /// An open worktree group in the current session — switch to that tab.
    CurrentTab(usize),
    /// A registered (but not open) worktree or workspace — switch workspaces,
    /// then land on `tab` when the root was a specific worktree.
    Workspace {
        repo_path: String,
        tab: Option<String>,
    },
    /// Not registered anywhere: offer to add `path` as a workspace instead of
    /// failing silently (spec: connect-to-root outside any workspace).
    OfferAdd(String),
}

/// Decide the connect-to-root target for a resolved `root` (None = the cwd is
/// not inside any git worktree; the offer then falls back to the raw `cwd`).
/// Returns `None` only when there is no cwd to work from at all.
pub(crate) fn connect_target(
    root: Option<&Path>,
    cwd: Option<&Path>,
    open_group_paths: &[String],
    db_worktrees: &[(String, String, String)], // (path, repo_path, tab_name)
    workspaces: &[String],
) -> Option<ConnectTarget> {
    let cwd = cwd?;
    let Some(root) = root else {
        return Some(ConnectTarget::OfferAdd(cwd.display().to_string()));
    };
    if let Some(i) = open_group_paths.iter().position(|p| Path::new(p) == root) {
        return Some(ConnectTarget::CurrentTab(i));
    }
    if let Some((_, repo_path, tab_name)) = db_worktrees
        .iter()
        .find(|(path, _, _)| Path::new(path) == root)
    {
        return Some(ConnectTarget::Workspace {
            repo_path: repo_path.clone(),
            tab: Some(tab_name.clone()),
        });
    }
    if let Some(ws) = workspaces.iter().find(|w| Path::new(w) == root) {
        return Some(ConnectTarget::Workspace {
            repo_path: ws.clone(),
            tab: None,
        });
    }
    Some(ConnectTarget::OfferAdd(root.display().to_string()))
}

/// The worktree label parts for the nav row: `(workspace, leaf)`. The
/// workspace prefix renders uppercased (display form of the canonical slug);
/// single-segment names render as the leaf alone. (Extracted from the pinned
/// `chrome.rs`.)
pub(crate) fn worktree_parts(model: &crate::chrome::FrameModel) -> Option<(String, String)> {
    if model.worktree.is_empty() {
        return None;
    }
    match model.worktree.split_once('/') {
        Some((ws, leaf)) => Some((ws.to_uppercase(), leaf.to_string())),
        None => Some((String::new(), model.worktree.clone())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// (open group paths, db worktree (path, repo, tab) rows, workspace roots).
    type Fixtures = (Vec<String>, Vec<(String, String, String)>, Vec<String>);

    fn fixtures() -> Fixtures {
        (
            vec!["/ws/repo-a".into(), "/wt/repo-a/feat".into()],
            vec![(
                "/wt/repo-b/fix".into(),
                "/ws/repo-b".into(),
                "repo-b/fix".into(),
            )],
            vec!["/ws/repo-a".into(), "/ws/repo-b".into()],
        )
    }

    #[test]
    fn open_group_wins_as_current_tab() {
        let (groups, wts, wss) = fixtures();
        let t = connect_target(
            Some(Path::new("/wt/repo-a/feat")),
            Some(Path::new("/wt/repo-a/feat/src")),
            &groups,
            &wts,
            &wss,
        );
        assert_eq!(t, Some(ConnectTarget::CurrentTab(1)));
    }

    #[test]
    fn registered_worktree_switches_workspace_with_tab() {
        let (groups, wts, wss) = fixtures();
        let t = connect_target(
            Some(Path::new("/wt/repo-b/fix")),
            Some(Path::new("/wt/repo-b/fix/deep")),
            &groups,
            &wts,
            &wss,
        );
        assert_eq!(
            t,
            Some(ConnectTarget::Workspace {
                repo_path: "/ws/repo-b".into(),
                tab: Some("repo-b/fix".into()),
            })
        );
    }

    #[test]
    fn workspace_root_switches_without_a_tab() {
        let (groups, wts, wss) = fixtures();
        let t = connect_target(
            Some(Path::new("/ws/repo-b")),
            Some(Path::new("/ws/repo-b/src")),
            &groups,
            &wts,
            &wss,
        );
        assert_eq!(
            t,
            Some(ConnectTarget::Workspace {
                repo_path: "/ws/repo-b".into(),
                tab: None,
            })
        );
    }

    #[test]
    fn unregistered_root_offers_to_add() {
        let (groups, wts, wss) = fixtures();
        let t = connect_target(
            Some(Path::new("/elsewhere/proj")),
            Some(Path::new("/elsewhere/proj/sub")),
            &groups,
            &wts,
            &wss,
        );
        assert_eq!(t, Some(ConnectTarget::OfferAdd("/elsewhere/proj".into())));
        // Not in a git repo at all: the offer falls back to the cwd itself.
        let t = connect_target(None, Some(Path::new("/plain/dir")), &groups, &wts, &wss);
        assert_eq!(t, Some(ConnectTarget::OfferAdd("/plain/dir".into())));
        // No cwd at all: nothing to do.
        assert_eq!(connect_target(None, None, &groups, &wts, &wss), None);
    }

    #[test]
    fn connect_switch_is_a_chrome_repaint() {
        // Acting on a ConnectTarget marks the `chrome` damage channel (the
        // loop's `dirty`), never a per-pane recompose — the render plan for
        // that damage shape is a full chrome frame, and pane-only damage
        // stays on the bounded incremental path (the render invariants).
        use crate::render_plan::{Damage, Overlays, RenderPlan, plan};
        let chrome = Damage {
            chrome: true,
            ..Damage::default()
        };
        assert_eq!(plan(&chrome, &Overlays::default()), RenderPlan::Full);
        let mut pane_only = Damage::default();
        pane_only.panes.insert(7);
        assert_eq!(
            plan(&pane_only, &Overlays::default()),
            RenderPlan::Incremental {
                panes: vec![7],
                bars: false,
                sidebar: false,
            }
        );
    }
}
