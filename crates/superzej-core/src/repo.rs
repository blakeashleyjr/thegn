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

/// Short repo name derived purely from a repo *root* path — no git, no DB.
/// Use this for bulk listings (e.g. the sidebar inventory) where the paths are
/// already repo roots and spawning `git` per repo would be the bottleneck.
pub fn repo_name_from_path(root: &Path) -> String {
    let base = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    base.strip_suffix(".git")
        .map(|s| s.to_string())
        .unwrap_or(base)
}

/// A stable, globally-unique slug for a repo — the prefix of every tab that
/// belongs to it (`"{slug}/…"`). All repos live in one zellij session now, so
/// tabs are scoped by this prefix rather than by a per-repo session.
///
/// The slug is assigned once and persisted (DB), with `-2`/`-3` suffixing when
/// two repos share a basename (e.g. two different `WASHU` checkouts), so their
/// tabs never collide. Same repo path always yields the same slug, so every call
/// site (tab creation, rename, resolve, sidebar grouping) stays consistent.
pub fn repo_slug(root: &Path) -> String {
    let base = {
        let s = util::slugify(&repo_name(root));
        if s.is_empty() {
            "repo".to_string()
        } else {
            s
        }
    };
    crate::db::Db::open()
        .ok()
        .and_then(|db| db.slug_for_repo(&root.to_string_lossy(), &base).ok())
        .unwrap_or(base)
}

/// Tab name for a repo's main checkout (its "home" tab).
pub fn home_tab(slug: &str) -> String {
    format!("{slug}/home")
}

/// Tab name for a worktree of `slug` on `branch` (`"{slug}/{branch-slug}"`).
/// Globally unique, so it doubles as the key the panel/`resolve-worktree` use.
pub fn branch_tab(slug: &str, branch: &str) -> String {
    format!("{slug}/{}", util::slugify(branch))
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

/// Discover git repos under a single `root` (e.g. `$HOME`), depth-limited and
/// pruning hidden + heavy dirs so a deep home directory stays fast. A repo is a
/// dir containing a `.git` (dir or file); we don't descend into `.git`. Used by
/// the sidebar's "+ new workspace" fzf picker. Sorted + deduped.
pub fn discover_repos_in(root: &Path, max_depth: usize) -> Vec<String> {
    fn prune(e: &walkdir::DirEntry) -> bool {
        if e.depth() == 0 {
            return false;
        }
        let n = e.file_name().to_string_lossy();
        // Don't descend into .git, hidden dirs, or notorious build/cache dirs.
        n == ".git" || n.starts_with('.') || matches!(n.as_ref(), "node_modules" | "target")
    }
    let mut found: Vec<String> = Vec::new();
    if !root.is_dir() {
        return found;
    }
    for entry in WalkDir::new(root)
        .max_depth(max_depth)
        .into_iter()
        .filter_entry(|e| !prune(e))
        .filter_map(|e| e.ok())
    {
        if entry.file_type().is_dir() && entry.path().join(".git").exists() {
            found.push(entry.path().to_string_lossy().into_owned());
        }
    }
    found.sort();
    found.dedup();
    found
}
