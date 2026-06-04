//! Repo / main-worktree resolution from a directory's git context, and repo
//! discovery for the workspace picker. Self-healing across session
//! resurrection (git is the source of truth; the DB is only a cache).

use crate::config::Config;
use crate::util;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

/// Toplevel of the working tree containing `dir`.
pub fn toplevel(dir: &Path) -> Option<PathBuf> {
    util::git_out(dir, &["rev-parse", "--show-toplevel"]).map(PathBuf::from)
}

/// The MAIN worktree root for `dir`'s repo — climb out of any linked worktree
/// so we never create worktrees-of-worktrees. None if `dir` isn't in a repo.
pub fn main_worktree(dir: &Path) -> Option<PathBuf> {
    let common = util::git_out(
        dir,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    let common = PathBuf::from(common);
    // Normal repo: ".../.git" -> parent is the main worktree.
    // Bare repo: the common dir is the repo itself.
    if common.file_name().map(|n| n == ".git").unwrap_or(false) {
        common.parent().map(|p| p.to_path_buf())
    } else {
        Some(common)
    }
}

pub fn is_bare(dir: &Path) -> bool {
    util::git_out(dir, &["rev-parse", "--is-bare-repository"]).as_deref() == Some("true")
}

/// Short repo name for tab names and worktree grouping.
pub fn repo_name(root: &Path) -> String {
    let top = toplevel(root).unwrap_or_else(|| root.to_path_buf());
    let base = top
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    base.strip_suffix(".git")
        .map(|s| s.to_string())
        .unwrap_or(base)
}

/// Discover git repos under the configured roots (parent dirs of a `.git`
/// directory). Skips worktrees (whose `.git` is a file). Sorted + deduped.
pub fn discover_repos(cfg: &Config) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for root in &cfg.repo_roots {
        let root = Path::new(root);
        if !root.is_dir() {
            continue;
        }
        for entry in WalkDir::new(root)
            .max_depth(cfg.repo_scan_depth)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if entry.file_type().is_dir() && entry.file_name() == ".git" {
                if let Some(parent) = entry.path().parent() {
                    found.push(parent.to_string_lossy().into_owned());
                }
            }
        }
    }
    found.sort();
    found.dedup();
    found
}
