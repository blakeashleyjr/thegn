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
    if let Ok(db) = Db::open() {
        if let Ok(Some(path)) = db.worktree_for_tab(&session, &tab) {
            println!("{path}");
            return Ok(());
        }
        // Extra same-worktree tabs ("{base} ·N", superzej new-tab) resolve to
        // their base tab's worktree.
        let base = crate::commands::new_tab::strip_page_suffix(&tab);
        if base != tab {
            if let Ok(Some(path)) = db.worktree_for_tab(&session, base) {
                println!("{path}");
            }
        }
    }
    Ok(())
}
