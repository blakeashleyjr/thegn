//! Worktree / panel teardown.
//!
//! `close-worktree` removes the focused worktree and closes its tab (a worktree
//! *is* a tab in the v2 model). It runs either in-pane ($SUPERZEJ_WORKTREE set)
//! or as a floating reaper spawned by the Alt-X keybind — in both cases the
//! reaper/pane shares the worktree's tab, so `close-tab` tears the whole tab
//! down (including the reaper) after the git worktree is removed.
//!
//! `close-panel` just closes the focused pane — for ordinary splits, never
//! touching worktrees.

use crate::commands::confirm;
use crate::db::Db;
use crate::{msg, repo, util, worktree, zellij};
use anyhow::Result;
use std::path::{Path, PathBuf};

/// Close the focused pane (a plain panel). Never removes a worktree.
pub fn close_panel() -> Result<()> {
    if zellij::in_zellij() {
        zellij::close_pane();
    }
    Ok(())
}

pub fn run(delete_branch: bool, force: bool) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let worktree_path: Option<PathBuf> = match std::env::var("SUPERZEJ_WORKTREE") {
        Ok(w) => Some(PathBuf::from(w)),
        Err(_) => repo::toplevel(&cwd),
    };

    let worktree_path = match worktree_path {
        Some(p) if p.is_dir() => p,
        _ => {
            // Not a worktree — just close the focused pane.
            if zellij::in_zellij() {
                zellij::close_pane();
            }
            return Ok(());
        }
    };

    let root = repo::main_worktree(&worktree_path).unwrap_or_else(|| worktree_path.clone());
    // A worktree's main pane is the repo root itself — refuse to "remove" it.
    if root == worktree_path {
        msg::warn("focused pane is the repo's main worktree; not removing it");
        if zellij::in_zellij() {
            zellij::close_pane();
        }
        return Ok(());
    }

    let branch = util::git_out(
        &worktree_path,
        &["symbolic-ref", "--quiet", "--short", "HEAD"],
    )
    .unwrap_or_default();
    let wt_s = worktree_path.to_string_lossy().into_owned();

    if !force && !confirm(&format!("Remove worktree '{branch}' at {wt_s}?")) {
        msg::info("cancelled");
        return Ok(());
    }
    worktree::remove(&root, Path::new(&wt_s), &branch, delete_branch);
    if let Ok(db) = Db::open() {
        let _ = db.del_worktree(&wt_s);
    }
    msg::info(&format!("removed worktree {branch}"));

    if zellij::in_zellij() {
        // The worktree is a tab; closing the tab removes it (and the reaper).
        zellij::close_tab();
    }
    Ok(())
}
