//! `superzej restore-session` — bring back the previous session's worktree tabs
//! on a cold start. zellij's native session serialization is deliberately off
//! (it restores a stale layout and ignores `--layout`), so we reconstruct from
//! the DB instead: every managed worktree is a row in `worktrees` (path, tab
//! name, branch, agent). For each one still on disk we open a tab with the
//! `worktree-tab-restore` layout, whose pane runs `pick-agent --resume` to
//! relaunch the recorded agent without prompting.
//!
//! Called once from the home tab's `status` at cold start. The guard is simple
//! and self-correcting: only run when this is the only tab (a fresh session) —
//! so reopening a home view later never re-restores.

use crate::db::{self, Db};
use crate::zellij;
use anyhow::Result;
use std::path::Path;

pub fn run() -> Result<()> {
    if !zellij::in_zellij() {
        return Ok(());
    }
    // Fresh session == exactly one tab (the home tab we're running in). If the
    // user already has other tabs open, this is not a cold start — do nothing.
    if zellij::tab_names().len() > 1 {
        return Ok(());
    }

    let session = db::session();
    let Ok(db) = Db::open() else {
        return Ok(());
    };
    let Ok(rows) = db.worktrees() else {
        return Ok(());
    };

    // Worktrees recorded for this session, excluding home checkouts, that still
    // exist on disk (git is the source of truth — skip stale rows). Oldest
    // first so tabs come back in roughly their original order.
    let mut rows: Vec<_> = rows
        .into_iter()
        .filter(|w| w.session_name == session)
        .filter(|w| !w.tab_name.ends_with("/home"))
        .filter(|w| !w.worktree.is_empty() && Path::new(&w.worktree).is_dir())
        .collect();
    rows.sort_by_key(|w| w.created_at);

    for w in &rows {
        if !zellij::new_tab(
            &w.tab_name,
            Path::new(&w.worktree),
            Some("worktree-tab-restore"),
        ) {
            zellij::new_tab(&w.tab_name, Path::new(&w.worktree), None);
        }
    }

    // Opening tabs steals focus; return to the home tab.
    if let Some(home) = zellij::tab_names().into_iter().find(|t| t.ends_with("/home")) {
        zellij::go_to_tab_name(&home);
    }
    Ok(())
}
