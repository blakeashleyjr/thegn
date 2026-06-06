//! `superzej diff` — emit a syntax-highlighted diff of a worktree against
//! its branch point, using `syntect` (pure Rust, no external binary).
//! Compatible with both the right-panel plugin (`run_command` capture) and
//! the interactive CLI.
//!
//! Range: everything since the merge-base with the resolved base branch, so it
//! shows "what this branch changes" — including uncommitted work (`git diff
//! <merge-base>` diffs the working tree against that commit).

use crate::commands::resolve_worktree;
use crate::db::Db;
use crate::diff_highlight;
use crate::remote::GitLoc;
use anyhow::Result;
use std::collections::HashMap;
use std::io::Write;
use std::path::Path;

pub fn run(
    worktree: Option<String>,
    base: Option<String>,
    stat: bool,
    files: bool,
    file_path: Option<String>,
) -> Result<()> {
    let wt = resolve_worktree(worktree);
    // Route git through the worktree's location — local, or over ssh for a remote
    // worktree (so the panel reads remote state exactly like a local one).
    let loc = GitLoc::for_worktree(&wt);

    // Base = explicit, else the repo's default branch (main/master), not the
    // worktree's own branch.
    let base = base.unwrap_or_else(|| default_branch(&loc));

    // Diff against the merge-base so we capture the branch's full delta; fall
    // back to HEAD (uncommitted-only) if no merge-base exists.
    let target = loc
        .git_out(&["merge-base", &base, "HEAD"])
        .unwrap_or_else(|| "HEAD".to_string());

    // --files: TSV with status, path, added, deleted columns (no diff, no delta).
    if files {
        let tsv = files_tsv(&loc, &target);
        // Warm the diff cache so the next `panel-snapshot` paints instantly.
        if let Ok(db) = Db::open() {
            let _ = db.put_diff_cache(&wt.to_string_lossy(), &tsv);
        }
        crate::out!("{tsv}");
        return Ok(());
    }

    // Capture a git diff (without colour) and emit a syntax-highlighted
    // version to stdout using syntect.
    let emit_highlighted = |git_args: &[&str], file_path: Option<&str>| {
        if let Ok(output) = loc.git_command(git_args).output() {
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

    // --stat: stream straight through (colors / large diffs).
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

/// The merge-base target a worktree diffs against (its branch's full delta vs
/// the repo's default branch), falling back to HEAD when there's no merge-base.
fn default_target(loc: &GitLoc) -> String {
    let base = default_branch(loc);
    loc.git_out(&["merge-base", &base, "HEAD"])
        .unwrap_or_else(|| "HEAD".to_string())
}

/// Build the file-list TSV (`status\tpath\tadded\tdeleted` per line) for a diff
/// against `target`. Shared by the `--files` CLI path and the watch daemon, and
/// routed through `GitLoc` so it works for remote worktrees too.
fn files_tsv(loc: &GitLoc, target: &str) -> String {
    let names = loc
        .git_out(&["diff", "--name-status", target])
        .unwrap_or_default();
    let nums = loc
        .git_out(&["diff", "--numstat", target])
        .unwrap_or_default();

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

    let mut out = String::new();
    for line in names.lines() {
        let (status, path) = match line.split_once('\t') {
            Some((s, p)) => (s, p.trim()),
            None => continue,
        };
        let (adds, dels) = num_map.get(path).copied().unwrap_or((0, 0));
        out.push_str(&format!("{status}\t{path}\t{adds}\t{dels}\n"));
    }
    out
}

/// Compute the file-list TSV for a worktree against its default-branch
/// merge-base — used by the watch daemon to push live diffs to the panel.
pub fn files_for(wt: &Path) -> String {
    let loc = GitLoc::for_worktree(wt);
    files_tsv(&loc, &default_target(&loc))
}
