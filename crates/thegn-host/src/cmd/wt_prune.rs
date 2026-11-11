//! `thegn wt prune --merged [--apply]` — native replacement for the
//! `wt_cleanup.py` script: remove merged, clean, idle lane worktrees and their
//! branches. The decision is `thegn_core::wt_prune`; this module gathers the
//! facts (in the CLI process only) and applies the verdicts through the same
//! teardown `wt rm --delete-branch --force` uses.
//!
//! Limitations: the live-process check scans `/proc` (Linux only; elsewhere it
//! sees nothing), and a worktree open in a running instance is not detected —
//! there is no cheap CLI-side answer for it, so the live-process scan and the
//! dirty check are the guards.

use anyhow::Result;
use std::path::Path;
use thegn_core::config::Config;
use thegn_core::wt_prune::{self, PruneDecision, PruneFacts};
use thegn_core::{outln, util};

/// Args for `wt prune`.
#[derive(clap::Args, Clone)]
pub struct PruneArgs {
    /// Prune worktrees whose branch is already merged into the target
    /// (required in this release; other modes are reserved).
    #[arg(long)]
    pub merged: bool,
    /// Branch the worktrees must be merged into (default: the repo's merge
    /// target, `[merge_queue] target_branch`, else its default branch).
    #[arg(long)]
    pub target: Option<String>,
    /// Actually remove them. Without this the command is a dry run that lists
    /// what would be removed and why each other worktree is kept.
    #[arg(long)]
    pub apply: bool,
    /// Emit one JSON object instead of the human summary.
    #[arg(long)]
    pub json: bool,
}

/// Gather the facts for one worktree. Anything Git cannot answer counts
/// against removal (not merged / not clean).
fn facts_for(path: &str, branch: &str, target: &str, root: &Path) -> PruneFacts {
    let dir = Path::new(path);
    let branch_opt = (!branch.is_empty()).then(|| branch.to_string());
    let ancestor_of_target = branch_opt
        .as_deref()
        .is_some_and(|b| util::git_is_ancestor(dir, b, target) == Some(true));
    let tree_clean = util::git_out_allow_empty(dir, &["status", "--porcelain"])
        .is_some_and(|s| s.trim().is_empty());
    PruneFacts {
        path: path.to_string(),
        is_primary: Path::new(path) == root,
        is_target_branch: branch == target,
        branch: branch_opt,
        ancestor_of_target,
        tree_clean,
        live_cwd_pids: crate::platform::proc::pids_with_cwd_under(dir),
        held_by_session: false,
    }
}

pub fn run(cfg: &Config, args: PruneArgs) -> Result<()> {
    if !args.merged {
        anyhow::bail!(
            "wt prune needs a mode: pass --merged (remove worktrees already merged into the target)"
        );
    }
    let start = super::resolve_worktree(None);
    let Some(root) = thegn_core::repo::main_worktree(&start) else {
        anyhow::bail!("not a git repo: {}", start.display());
    };
    let target = match args.target.clone() {
        Some(t) => t,
        None => crate::integrate::resolve_target(&cfg.repo_merge_queue(&root), &root),
    };
    let root_s = root.to_string_lossy().into_owned();
    let facts: Vec<PruneFacts> = super::list::collect(cfg)?
        .into_iter()
        .filter(|w| w.repo == root_s && w.exists)
        .map(|w| facts_for(&w.path, &w.branch, &target, &root))
        .collect();
    let plan = wt_prune::plan(facts);
    let (removable, kept) = wt_prune::counts(&plan);

    let mut failed: Vec<(String, String)> = Vec::new();
    let mut removed = 0usize;
    if args.apply {
        for (path, decision) in &plan {
            if matches!(decision, PruneDecision::Remove { .. }) {
                match super::wt::rm(cfg, path, true, true) {
                    Ok(()) => removed += 1,
                    Err(why) => failed.push((path.clone(), format!("{why:#}"))),
                }
            }
        }
    }

    if args.json {
        let items: Vec<serde_json::Value> = plan
            .iter()
            .map(|(path, d)| match d {
                PruneDecision::Remove { delete_branch } => serde_json::json!({
                    "path": path, "action": "remove", "delete_branch": delete_branch,
                }),
                PruneDecision::Keep(r) => serde_json::json!({
                    "path": path, "action": "keep", "reason": r.code(),
                }),
            })
            .collect();
        let out = serde_json::json!({
            "target": target,
            "applied": args.apply,
            "removed": removed,
            "kept": kept,
            "failed": failed.iter().map(|(p, e)| serde_json::json!({"path": p, "error": e})).collect::<Vec<_>>(),
            "worktrees": items,
        });
        outln!("{out}");
    } else {
        if !args.apply {
            for (path, d) in &plan {
                outln!("{}", wt_prune::render(path, d));
            }
        }
        for (path, why) in &failed {
            thegn_core::msg::warn(&format!("could not remove {path}: {why}"));
        }
        if args.apply {
            outln!("Pruned {removed}, kept {kept}");
        } else {
            outln!("Would prune {removable}, kept {kept} (dry run; pass --apply)");
        }
    }
    if failed.is_empty() {
        Ok(())
    } else {
        anyhow::bail!("{} worktree(s) could not be removed", failed.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) {
        assert!(
            util::git_ok(
                dir,
                &["-c", "commit.gpgsign=false"]
                    .iter()
                    .chain(args)
                    .copied()
                    .collect::<Vec<_>>()
                    .as_slice()
            ),
            "git {args:?}"
        );
    }

    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir_in("/tmp").unwrap();
        let p = dir.path();
        git(p, &["init", "-q", "-b", "main"]);
        git(p, &["config", "user.email", "t@example.invalid"]);
        git(p, &["config", "user.name", "t"]);
        git(p, &["commit", "--allow-empty", "-q", "-m", "init"]);
        dir
    }

    #[test]
    fn merged_clean_worktree_is_removable_and_unmerged_is_not() {
        let main = repo();
        let wt = main.path().join("wt-merged");
        git(
            main.path(),
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "tg/merged",
                wt.to_str().unwrap(),
            ],
        );
        let f = facts_for(wt.to_str().unwrap(), "tg/merged", "main", main.path());
        assert!(f.ancestor_of_target && f.tree_clean && !f.is_primary && !f.is_target_branch);
        assert!(matches!(
            wt_prune::decide(&f),
            PruneDecision::Remove {
                delete_branch: true
            }
        ));

        git(&wt, &["commit", "--allow-empty", "-q", "-m", "ahead"]);
        let f = facts_for(wt.to_str().unwrap(), "tg/merged", "main", main.path());
        assert!(!f.ancestor_of_target);
    }

    #[test]
    fn untracked_file_marks_the_tree_dirty_and_primary_is_flagged() {
        let main = repo();
        std::fs::write(main.path().join("stray"), "x").unwrap();
        let f = facts_for(main.path().to_str().unwrap(), "main", "main", main.path());
        assert!(!f.tree_clean && f.is_primary && f.is_target_branch);
    }

    #[test]
    fn a_process_cwd_inside_the_worktree_is_reported() {
        // This test process's own cwd is inside the tree being probed.
        let cwd = std::env::current_dir().unwrap();
        let f = facts_for(cwd.to_str().unwrap(), "", "main", Path::new("/nonexistent"));
        if cfg!(target_os = "linux") {
            assert!(f.live_cwd_pids.contains(&std::process::id()));
        }
        assert!(f.branch.is_none());
    }
}
