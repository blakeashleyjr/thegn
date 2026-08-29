//! `thegn diff` — syntax-highlighted (syntect) diff of a worktree against its
//! branch point. Range = everything since the merge-base with the resolved base
//! branch, so it shows "what this branch changes," including uncommitted work.

use anyhow::{Context, Result, bail};
use std::io::Write;
use thegn_core::config::{Config, StructuralDiff};
use thegn_core::diff_highlight;
use thegn_core::remote::GitLoc;

use crate::cmd::resolve_worktree;

pub fn run(
    cfg: &Config,
    worktree: Option<String>,
    base: Option<String>,
    stat: bool,
    file_path: Option<String>,
    structural: bool,
) -> Result<()> {
    let wt = resolve_worktree(worktree);
    // Route git through the worktree's location — local, or over ssh for a
    // remote worktree.
    let loc = GitLoc::for_worktree(&wt);

    let base = base.unwrap_or_else(|| default_branch(&loc));
    // Validate an explicitly-resolvable base ref up front. A typo'd `--base`
    // must NOT silently diff against HEAD: exit non-zero so scripts/CI notice
    // the mistake instead of trusting an empty/HEAD-only diff.
    if !loc.git_ok(&[
        "rev-parse",
        "--verify",
        "--quiet",
        &format!("{base}^{{commit}}"),
    ]) {
        bail!("unknown base ref '{base}'");
    }
    // Diff against the merge-base so we capture the branch's full delta; fall
    // back to HEAD (uncommitted-only) if no merge-base exists (unrelated
    // histories — the base is valid, there's just no common ancestor).
    let target = loc
        .git_out(&["merge-base", &base, "HEAD"])
        .unwrap_or_else(|| "HEAD".to_string());

    // Structural (difftastic) read-only view. The `--structural` flag forces it;
    // otherwise `[git] structural_diff` may select it. Never for `--stat` (a
    // summary), and never fed to `git apply` — this surface is read-only.
    if !stat {
        let mode = if structural {
            StructuralDiff::Difft
        } else {
            cfg.repo_git(&wt).structural_diff
        };
        if mode != StructuralDiff::Off {
            if let Some(difft) = crate::structural_diff::choose(cfg, mode) {
                return crate::structural_diff::run_cli(
                    &loc,
                    &target,
                    file_path.as_deref(),
                    &difft,
                );
            }
            // Explicit intent (flag or `= "difft"`) but nothing resolved: say so,
            // then fall through to the internal highlighter. `auto` is silent.
            if structural || mode == StructuralDiff::Difft {
                thegn_core::msg::warn(
                    "difft unavailable — falling back to the internal diff. \
                     Install difftastic or set [managed_tools.difft] path.",
                );
            }
        }
    }

    let emit_highlighted = |git_args: &[&str], file_path: Option<&str>| -> Result<()> {
        // CLI path: `thegn diff` runs synchronously, no event loop.
        #[expect(clippy::disallowed_methods)]
        let output = loc
            .git_command(git_args)
            .output()
            .context("failed to run git diff")?;
        // Surface git failures instead of swallowing them; the exit code must be
        // non-zero so scripts/CI don't mistake a git error for an empty diff.
        if !output.status.success() {
            let err = String::from_utf8_lossy(&output.stderr);
            bail!("git diff failed: {}", err.trim());
        }
        let raw = String::from_utf8_lossy(&output.stdout);
        let highlighted = diff_highlight::highlight_diff(&raw, file_path.unwrap_or(""));
        let _ = std::io::stdout().write_all(highlighted.as_bytes()); // best-effort: stdout write: EPIPE on a closed |head pipe is normal
        Ok(())
    };

    if let Some(fp) = file_path {
        return emit_highlighted(&["diff", "--no-color", &target, "--", &fp], Some(&fp));
    }

    if !stat {
        return emit_highlighted(&["diff", "--no-color", &target], None);
    }

    // --stat: stream straight through (colors / large diffs).
    // CLI path: `thegn diff` runs synchronously, no event loop.
    #[expect(clippy::disallowed_methods)]
    let status = loc
        .git_command(&["-c", "color.ui=always", "diff", "--stat", &target])
        .status()
        .context("failed to run git diff --stat")?;
    // Propagate git's failure: exit non-zero so scripts/CI notice.
    if !status.success() {
        bail!("git diff --stat failed");
    }
    Ok(())
}

/// The repo's default branch (origin/HEAD, else main/master, else HEAD), probed
/// through the location so it works for remote worktrees too. Shared with the
/// in-app diff viewer (`diff_view`).
pub(crate) fn default_branch(loc: &GitLoc) -> String {
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
