//! Resolve the connecting shim's [`WorktreeContext`] from its cwd.
//!
//! An agent launches `thegn mcp proxy` from inside its pane, so the cwd is the
//! worktree. We read the identity from git (bounded subprocess calls — this is
//! a CLI command, never the event loop). Any field that cannot be resolved is
//! left `None`; a scoped upstream then withholds itself (partition leakage is a
//! correctness bug, handled in core).

use std::path::Path;
use std::process::Command;

use thegn_core::mcp::proxy::partition::WorktreeContext;

/// Resolve from the current directory.
pub fn resolve_from_cwd() -> WorktreeContext {
    let cwd = std::env::current_dir().unwrap_or_else(|_| Path::new(".").to_path_buf());
    resolve(&cwd)
}

/// Resolve the worktree identity anchored at `cwd`.
pub fn resolve(cwd: &Path) -> WorktreeContext {
    let toplevel = git(cwd, &["rev-parse", "--show-toplevel"]);
    let branch = git(cwd, &["rev-parse", "--abbrev-ref", "HEAD"]).filter(|b| b != "HEAD");
    // The shared object store's parent is the primary checkout → the "repo".
    let common = git(
        cwd,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    );
    let repo_root = common
        .as_deref()
        .map(Path::new)
        .and_then(Path::parent)
        .map(|p| p.to_string_lossy().into_owned());

    // workspace = the repo's name; worktree = this worktree's own dir name.
    let workspace = repo_root
        .as_deref()
        .map(Path::new)
        .or(toplevel.as_deref().map(Path::new))
        .and_then(basename);
    let worktree = toplevel.as_deref().map(Path::new).and_then(basename);

    WorktreeContext {
        workspace,
        worktree,
        repo_root: repo_root.or(toplevel),
        branch,
    }
}

fn basename(p: &Path) -> Option<String> {
    p.file_name().map(|n| n.to_string_lossy().into_owned())
}

/// Run a git command anchored at `cwd`, returning trimmed stdout on success.
fn git(cwd: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}
