//! Shared data types.

use serde::Serialize;

/// A superzej-managed worktree as recorded in the DB. Some fields are carried
/// for future use (e.g. the Phase-2 sidebar) even if `list` doesn't read them.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct WorktreeRow {
    pub worktree: String,
    pub branch: String,
    pub agent: String,
    pub created_at: i64,
    pub repo_root: String,
    pub tab_name: String,
}

/// A worktree enriched with live git status, for `list` / `dashboard` output.
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
