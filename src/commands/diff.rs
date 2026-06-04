//! `superzej diff` — emit a colorized, non-paged diff of a worktree against its
//! branch point, for the right panel (and as a quick CLI view).
//!
//! Range: everything since the merge-base with the resolved base branch, so it
//! shows "what this branch changes" — including uncommitted work (`git diff
//! <merge-base>` diffs the working tree against that commit). Colors are forced
//! on (output is captured by the panel, not a tty) and any pager is disabled.

use crate::commands::resolve_worktree;
use crate::{repo, util, worktree};
use anyhow::Result;
use std::path::Path;
use std::process::Command;

pub fn run(
    worktree: Option<String>,
    base: Option<String>,
    stat: bool,
    files: bool,
    file_path: Option<String>,
) -> Result<()> {
    let wt = resolve_worktree(worktree);

    // Base = explicit, else the repo's default branch (main/master), not the
    // worktree's own branch.
    let base = base.unwrap_or_else(|| {
        let root = repo::main_worktree(&wt).unwrap_or_else(|| wt.clone());
        worktree::default_branch(&root)
    });

    // Diff against the merge-base so we capture the branch's full delta; fall
    // back to HEAD (uncommitted-only) if no merge-base exists.
    let target =
        util::git_out(&wt, &["merge-base", &base, "HEAD"]).unwrap_or_else(|| "HEAD".to_string());

    // --files: TSV of modified files (status\tpath per line).
    if files {
        let output = util::git_out(&wt, &["diff", "--name-status", &target]).unwrap_or_default();
        println!("{output}");
        return Ok(());
    }

    // --file <path>: full diff of a single file.
    if let Some(fp) = file_path {
        if util::have("delta") {
            let cmd = format!(
                "git -c color.ui=always diff {} -- {} | delta --paging=never --color-only",
                target,
                shell_quote(&fp),
            );
            let _ = Command::new("sh")
                .arg("-c")
                .arg(&cmd)
                .current_dir(&wt)
                .status();
        } else {
            let args = vec!["-c", "color.ui=always", "diff", &target, "--", &fp];
            run_git(&wt, &args);
        }
        return Ok(());
    }

    if !stat && util::have("delta") {
        // Pipe through delta with paging disabled (never blocks the panel).
        let cmd =
            format!("git -c color.ui=always diff {target} | delta --paging=never --color-only");
        let _ = Command::new("sh")
            .arg("-c")
            .arg(&cmd)
            .current_dir(&wt)
            .status();
        return Ok(());
    }

    let mut args = vec!["-c", "color.ui=always", "diff"];
    if stat {
        args.push("--stat");
    }
    args.push(&target);
    run_git(&wt, &args);
    Ok(())
}

/// Simple shell-quoting: wrap in single quotes, escaping internal single quotes.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Run `git -C <dir> <args>` inheriting stdout (streams colors / large diffs).
fn run_git(dir: &Path, args: &[&str]) {
    let _ = Command::new("git").arg("-C").arg(dir).args(args).status();
}
