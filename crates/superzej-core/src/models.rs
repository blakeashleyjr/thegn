//! Shared data types.

use serde::Serialize;

/// A registered workspace, as recorded in the DB. Identified by its path — a
/// git repo's main worktree, or a plain directory for a non-repo workspace.
#[derive(Debug, Clone, Serialize)]
#[allow(dead_code)]
pub struct WorkspaceRow {
    pub repo_path: String,
    pub name: String,
    pub created_at: i64,
    pub last_active: i64,
    /// `"repo"` (a git repo) or `"dir"` (a plain non-git directory). Git-only
    /// actions no-op on `dir` workspaces.
    pub kind: String,
}

/// A superzej-managed worktree (one per tab) as recorded in the DB. Some fields
/// are carried for the sidebar/panel even if `list` ignores them.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct WorktreeRow {
    pub worktree: String,
    pub branch: String,
    pub agent: String,
    pub created_at: i64,
    pub repo_root: String,
    pub tab_name: String,
    pub session_name: String,
    /// Remote-location descriptor (JSON) for a remote worktree; empty = local.
    pub location: String,
    /// Persistent sort key for the sidebar (creation order by default,
    /// user-reorderable via Shift+Alt+↑/↓). Lower sorts first.
    pub position: i64,
}

/// A persisted worktree group (native host, schema v6): one worktree shown in
/// the sidebar, owning an ordered set of tabs (`GroupTabRow`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabGroupRow {
    /// Display name, e.g. "app/feat" — unique within a session.
    pub name: String,
    /// "home" (the main checkout) or "branch".
    pub kind: String,
    /// Worktree dir on disk (empty only for legacy rows with no path).
    pub worktree: String,
    pub ordinal: i64,
    /// Index of the group's active tab (restored when switching back).
    pub active_tab: i64,
}

/// A persisted tab inside a worktree group (schema v6). The `pane_tree` is the
/// serialized `CenterTree` (host-owned); core treats it as an opaque blob so the
/// layout model can evolve without touching the schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupTabRow {
    pub group_name: String,
    pub ordinal: i64,
    /// Short display title for the tab chip ("1", "zsh", …).
    pub title: String,
    /// Serialized pane tree (opaque JSON to core).
    pub pane_tree: String,
    pub focused_pane: i64,
}

/// A worktree enriched with live git status, for `list` / `dashboard` output.
/// `workspace` holds the owning session name (the workspace) in the v2 model.
#[derive(Debug, Clone, Serialize)]
pub struct WorktreeView {
    pub workspace: String,
    pub repo: String,
    pub path: String,
    pub branch: String,
    pub agent: String,
    pub dirty: i64,
    pub ahead: i64,
    pub behind: i64,
    pub created_at: i64,
    pub exists: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_construct_and_serialize() {
        let ws = WorkspaceRow {
            repo_path: "/r".into(),
            name: "r".into(),
            created_at: 1,
            last_active: 2,
            kind: "repo".into(),
        };
        assert!(
            serde_json::to_string(&ws)
                .unwrap()
                .contains("\"repo_path\":\"/r\"")
        );

        let v = WorktreeView {
            workspace: "w".into(),
            repo: "/r".into(),
            path: "/wt".into(),
            branch: "sz/x".into(),
            agent: "claude".into(),
            dirty: 1,
            ahead: 2,
            behind: 0,
            created_at: 3,
            exists: true,
        };
        let j = serde_json::to_string(&v).unwrap();
        assert!(j.contains("\"branch\":\"sz/x\"") && j.contains("\"exists\":true"));

        // WorktreeRow has no Serialize; just exercise construction + Clone/Debug.
        let row = WorktreeRow {
            worktree: "/wt".into(),
            branch: "sz/x".into(),
            agent: String::new(),
            created_at: 0,
            repo_root: "/r".into(),
            tab_name: "r/x".into(),
            session_name: "default".into(),
            location: String::new(),
            position: 0,
        };
        let _ = format!("{:?}", row.clone());
    }
}
