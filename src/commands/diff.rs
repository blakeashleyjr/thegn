//! `superzej diff` — emit a syntax-highlighted diff of a worktree against
//! its branch point, using `syntect` (pure Rust, no external binary).
//! Compatible with both the right-panel plugin (`run_command` capture) and
//! the interactive CLI.
//!
//! Range: everything since the merge-base with the resolved base branch, so it
//! shows "what this branch changes" — including uncommitted work (`git diff
//! <merge-base>` diffs the working tree against that commit).

use crate::commands::resolve_worktree;
use crate::{diff_highlight, repo, util, worktree};
use anyhow::Result;
use std::collections::HashMap;
use std::io::Write;
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

    // --files: TSV with status, path, added, deleted columns (no diff, no delta).
    if files {
        let names = util::git_out(&wt, &["diff", "--name-status", &target]).unwrap_or_default();
        let nums = util::git_out(&wt, &["diff", "--numstat", &target]).unwrap_or_default();

        let mut num_map: HashMap<&str, (u32, u32)> = HashMap::new();
        for line in nums.lines() {
            let parts: Vec<&str> = line.splitn(3, '\t').collect();
            if parts.len() < 3 {
                continue;
            }
            let adds: u32 = parts[0].parse().unwrap_or(0);
            let dels: u32 = parts[1].parse().unwrap_or(0);
            num_map.insert(parts[2].trim(), (adds, dels));
        }

        for line in names.lines() {
            let (status, path) = match line.split_once('\t') {
                Some((s, p)) => (s, p.trim()),
                None => continue,
            };
            let (adds, dels) = num_map.get(path).copied().unwrap_or((0, 0));
            println!("{status}\t{path}\t{adds}\t{dels}");
        }
        return Ok(());
    }

    // Capture a git diff (without colour) and emit a syntax-highlighted
    // version to stdout using syntect.
    let emit_highlighted = |git_args: &[&str], file_path: Option<&str>| {
        if let Ok(output) = Command::new("git")
            .arg("-C")
            .arg(&wt)
            .args(git_args)
            .output()
        {
            let raw = String::from_utf8_lossy(&output.stdout);
            let highlighted = diff_highlight::highlight_diff(&raw, file_path.unwrap_or(""));
            let _ = std::io::stdout().write_all(highlighted.as_bytes());
        }
    };

    // --file <path>: full diff of a single file.
    if let Some(fp) = file_path {
        emit_highlighted(&["diff", "--no-color", &target, "--", &fp], Some(&fp));
        return Ok(());
    }

    if !stat {
        emit_highlighted(&["diff", "--no-color", &target], None);
        return Ok(());
    }

    let mut args = vec!["-c", "color.ui=always", "diff"];
    args.push("--stat");
    args.push(&target);
    run_git(&wt, &args);
    Ok(())
}

/// Run `git -C <dir> <args>` inheriting stdout (streams colors / large diffs).
fn run_git(dir: &Path, args: &[&str]) {
    let _ = Command::new("git").arg("-C").arg(dir).args(args).status();
}
