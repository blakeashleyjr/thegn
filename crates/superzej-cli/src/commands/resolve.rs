//! `superzej resolve-worktree --session <s> --tab <t>` — internal helper for the
//! panel plugin, which knows the focused (session, tab) but not its path
//! (zellij's PaneInfo carries no cwd). Prints the worktree path, or nothing.

use crate::db::{self, Db};
use anyhow::Result;

pub fn run(session: Option<String>, tab: Option<String>) -> Result<()> {
    let session = session.unwrap_or_else(db::session);
    let Some(tab) = tab else {
        return Ok(());
    };
    if let Some(path) = resolve_tab_worktree(&session, &tab) {
        crate::outln!("{path}");
    }
    Ok(())
}

/// The worktree path for a (session, tab) pair. Extra same-worktree tabs
/// ("{base} ·N", from `superzej new-tab`) resolve to their base tab's worktree.
/// Shared by `resolve-worktree` and `panel-snapshot`.
pub fn resolve_tab_worktree(session: &str, tab: &str) -> Option<String> {
    let db = Db::open().ok()?;
    if let Ok(Some(path)) = db.worktree_for_tab(session, tab) {
        return Some(path);
    }
    let base = crate::commands::new_tab::strip_page_suffix(tab);
    if base != tab {
        if let Ok(Some(path)) = db.worktree_for_tab(session, base) {
            return Some(path);
        }
    }
    None
}
