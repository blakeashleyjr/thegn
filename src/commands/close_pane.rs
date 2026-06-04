//! `superzej close-pane [--remove-worktree]` — close a worktree pane, optionally
//! removing its worktree.
//!
//! in-pane mode ($SUPERZEJ_WORKTREE set): closes this pane via close-pane.
//! keybind mode (floating reaper): captures the focused worktree via cwd, then
//! focuses the previous (worktree) pane and closes it; the reaper self-closes.

use crate::commands::confirm;
use crate::config::Config;
use crate::db::Db;
use crate::{msg, repo, util, worktree, zellij};
use anyhow::Result;
use std::path::{Path, PathBuf};

pub fn run(cfg: &Config, remove: bool, delete_branch: bool, force: bool) -> Result<()> {
    let remove = remove || cfg.auto_remove_worktree;

    let cwd = std::env::current_dir()?;
    let (in_pane, worktree_path): (bool, Option<PathBuf>) = match std::env::var("SUPERZEJ_WORKTREE")
    {
        Ok(w) => (true, Some(PathBuf::from(w))),
        Err(_) => (false, repo::toplevel(&cwd)),
    };

    let worktree_path = match worktree_path {
        Some(p) if p.is_dir() => p,
        _ => {
            // Not a worktree — just close the focused pane (in-pane mode).
            if in_pane && zellij::in_zellij() {
                zellij::close_pane();
            }
            return Ok(());
        }
    };

    let root = repo::main_worktree(&worktree_path).unwrap_or_else(|| worktree_path.clone());
    let branch = util::git_out(
        &worktree_path,
        &["symbolic-ref", "--quiet", "--short", "HEAD"],
    )
    .unwrap_or_default();
    let wt_s = worktree_path.to_string_lossy().into_owned();

    if remove {
        if !force && !confirm(&format!("Remove worktree '{branch}' at {wt_s}?")) {
            msg::info("cancelled");
            return Ok(());
        }
        worktree::remove(&root, Path::new(&wt_s), &branch, delete_branch);
        if let Ok(db) = Db::open() {
            let _ = db.del_worktree(&wt_s);
        }
        msg::info(&format!("removed worktree {branch}"));
    }

    if zellij::in_zellij() {
        if in_pane {
            zellij::close_pane(); // closes self (the worktree pane)
        } else {
            zellij::focus_previous_pane();
            zellij::close_pane(); // closes the worktree pane; reaper self-closes
        }
    }
    Ok(())
}
