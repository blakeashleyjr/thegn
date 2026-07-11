//! `thegn diff` — syntax-highlighted (syntect) diff of a worktree against its
//! branch point. Range = everything since the merge-base with the resolved base
//! branch, so it shows "what this branch changes," including uncommitted work.

use anyhow::Result;
use std::io::Write;
use thegn_core::diff_highlight;
use thegn_core::remote::GitLoc;

use crate::cmd::resolve_worktree;

pub fn run(
    worktree: Option<String>,
    base: Option<String>,
    stat: bool,
    file_path: Option<String>,
) -> Result<()> {
    let wt = resolve_worktree(worktree);
    // Route git through the worktree's location — local, or over ssh for a
    // remote worktree.
    let loc = GitLoc::for_worktree(&wt);

    let base = base.unwrap_or_else(|| default_branch(&loc));
    // Diff against the merge-base so we capture the branch's full delta; fall
    // back to HEAD (uncommitted-only) if no merge-base exists.
    let target = loc
        .git_out(&["merge-base", &base, "HEAD"])
        .unwrap_or_else(|| "HEAD".to_string());

    let emit_highlighted = |git_args: &[&str], file_path: Option<&str>| {
        // CLI path: `thegn diff` runs synchronously, no event loop.
        #[expect(clippy::disallowed_methods)]
        if let Ok(output) = loc.git_command(git_args).output() {
            let raw = String::from_utf8_lossy(&output.stdout);
            let highlighted = diff_highlight::highlight_diff(&raw, file_path.unwrap_or(""));
            let _ = std::io::stdout().write_all(highlighted.as_bytes());
        }
    };

    if let Some(fp) = file_path {
        emit_highlighted(&["diff", "--no-color", &target, "--", &fp], Some(&fp));
        return Ok(());
    }

    if !stat {
        emit_highlighted(&["diff", "--no-color", &target], None);
        return Ok(());
    }

    // --stat: stream straight through (colors / large diffs).
    // CLI path: `thegn diff` runs synchronously, no event loop.
    #[expect(clippy::disallowed_methods)]
    let _ = loc
        .git_command(&["-c", "color.ui=always", "diff", "--stat", &target])
        .status();
    Ok(())
}

/// The repo's default branch (origin/HEAD, else main/master, else HEAD), probed
/// through the location so it works for remote worktrees too.
fn default_branch(loc: &GitLoc) -> String {
    if let Some(r) = loc.git_out(&[
        "symbolic-ref",
        "--quiet",
        "--short",
        "refs/remotes/origin/HEAD",
    ]) {
        return r.strip_prefix("origin/").unwrap_or(&r).to_string();
    }
    for b in ["main", "master"] {
        if loc.git_ok(&[
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{b}"),
        ]) {
            return b.to_string();
        }
    }
    "HEAD".to_string()
}
