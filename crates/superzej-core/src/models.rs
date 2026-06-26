//! Shared data types.

use serde::Serialize;

/// A sandbox audit event from the `container_events` table.
#[derive(Debug, Clone, PartialEq)]
pub struct ContainerEvent {
    pub id: i64,
    pub worktree: String,
    pub ts: i64,
    pub kind: String,
    pub detail: Option<String>,
    pub exit_code: Option<i64>,
}

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
    pub sandbox_backend: Option<String>,
    pub folder_id: Option<i64>,
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
    /// Per-leaf working directories: a JSON map of `pane id → cwd` (opaque to
    /// core). Empty string when unset (pre-v14 rows / no captured cwds).
    pub pane_cwds: String,
    /// Per-leaf last foreground command: a JSON map of `pane id → {argv, cwd}`
    /// (opaque to core). Empty string when unset (pre-v15 rows / idle shell, no
    /// non-shell program was running).
    pub pane_cmds: String,
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

/// A persistent folder in the sidebar.
#[derive(Debug, Clone)]
pub struct FolderRow {
    pub folder_id: i64,
    pub repo_path: String,
    pub name: String,
    pub position: i64,
    pub created_at: i64,
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
            sandbox_backend: None,
            folder_id: None,
        };
        let _ = format!("{:?}", row.clone());
    }
}
