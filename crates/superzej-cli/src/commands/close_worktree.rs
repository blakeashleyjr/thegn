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
use crate::config::Config;
use crate::db::Db;
use crate::remote::GitLoc;
use crate::{msg, repo, sandbox, worktree, zellij};
use anyhow::Result;
use std::path::{Path, PathBuf};

/// Close the focused pane (a plain panel). Never removes a worktree.
pub fn close_panel() -> Result<()> {
    if zellij::in_zellij() {
        zellij::close_pane();
    }
    Ok(())
}

pub fn run(cfg: &Config, delete_branch: bool, force: bool) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let wt_s: Option<String> = std::env::var("SUPERZEJ_WORKTREE")
        .ok()
        .or_else(|| repo::toplevel(&cwd).map(|p| p.to_string_lossy().into_owned()));

    let wt_s = match wt_s {
        Some(s) => s,
        None => {
            if zellij::in_zellij() {
                zellij::close_pane();
            }
            return Ok(());
        }
    };
    let loc = GitLoc::for_worktree(Path::new(&wt_s));

    // A local worktree must exist on disk; a remote one is reachable over ssh.
    if !loc.is_remote() && !Path::new(&wt_s).is_dir() {
        if zellij::in_zellij() {
            zellij::close_pane();
        }
        return Ok(());
    }

    // The repo root: from the DB for a remote worktree, else climbed locally.
    let root = if loc.is_remote() {
        Db::open()
            .ok()
            .and_then(|db| db.repo_root_for(&wt_s).ok().flatten())
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(&wt_s))
    } else {
        repo::main_worktree(Path::new(&wt_s)).unwrap_or_else(|| PathBuf::from(&wt_s))
    };
    // A worktree's main pane is the repo root itself — refuse to "remove" it
    // (local only; a remote worktree path is never the local root).
    if !loc.is_remote() && root == Path::new(&wt_s) {
        msg::warn("focused pane is the repo's main worktree; not removing it");
        if zellij::in_zellij() {
            zellij::close_pane();
        }
        return Ok(());
    }

    let branch = loc
        .git_out(&["symbolic-ref", "--quiet", "--short", "HEAD"])
        .unwrap_or_default();

    if !force && !confirm(&format!("Remove worktree '{branch}' at {wt_s}?")) {
        msg::info("cancelled");
        return Ok(());
    }
    if loc.is_remote() {
        // Remove the worktree on the remote over ssh.
        if !loc.git_ok(&["worktree", "remove", "--force", &wt_s]) {
            msg::warn(&format!("could not remove remote worktree at {wt_s}"));
        }
        if delete_branch && !branch.is_empty() && !loc.git_ok(&["branch", "-D", &branch]) {
            msg::warn(&format!("could not delete remote branch {branch}"));
        }
    } else {
        worktree::remove(&root, Path::new(&wt_s), &branch, delete_branch);
    }
    if let Ok(db) = Db::open() {
        let _ = db.del_worktree(&wt_s);
    }
    // Reap the worktree's sandbox container (no-op for host/bwrap; over ssh for
    // a remote worktree).
    let cname = sandbox::container_name(&wt_s);
    sandbox::teardown(&cfg.repo_sandbox(&root), &loc, &cname);
    msg::info(&format!("removed worktree {branch}"));

    if zellij::in_zellij() {
        // The worktree is a tab; closing the tab removes it (and the reaper).
        zellij::close_tab();
    }
    Ok(())
}
