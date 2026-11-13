//! `thegn wt prune --merged [--apply]` — native replacement for the
//! `wt_cleanup.py` script: remove merged, clean, idle lane worktrees and their
//! branches. The decision is `thegn_core::wt_prune`; this module gathers the
//! facts (in the CLI process only) and applies the verdicts through the same
//! teardown `wt rm --delete-branch --force` uses.
//!
//! Limitations: the live-process check scans `/proc` (Linux only; elsewhere it
//! sees nothing), and a worktree open in a running instance is not detected —
//! there is no cheap CLI-side answer for it, so the live-process scan and the
//! dirty check are the guards. Merge-queue rows are not consulted either. Facts
//! are re-checked per worktree immediately before its removal, and a branch
//! whose tip moved since planning is left alone.

use anyhow::{Context, Result, anyhow};
use std::collections::HashMap;
use std::path::Path;
use thegn_core::config::Config;
use thegn_core::git_run::{GitOutcome, GitTextError};
use thegn_core::remote::GitLoc;
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
    /// what would be removed and why each other worktree is kept. Live
    /// sessions and merge-queue-held rows are NOT checked; only dirty trees
    /// and processes running inside a worktree are.
    #[arg(long)]
    pub apply: bool,
    /// Emit one JSON object instead of the human summary.
    #[arg(long)]
    pub json: bool,
}

/// Commit a local branch points at (`None` when the branch ref is absent).
fn branch_tip(dir: &Path, branch: &str) -> Result<Option<String>> {
    if branch.is_empty() {
        return Ok(None);
    }
    let reference = format!("refs/heads/{branch}");
    match GitLoc::Local(dir.to_path_buf()).git_text(&["rev-parse", "--verify", "-q", &reference]) {
        Ok(tip) => Ok((!tip.is_empty()).then_some(tip)),
        Err(GitTextError::Failed(GitOutcome::Exit {
            code: Some(1),
            stderr_tail,
        })) if stderr_tail.trim().is_empty() => Ok(None),
        Err(error) => Err(anyhow!("git rev-parse {reference}: {error}")),
    }
}

/// Gather the facts for one worktree. Git read failures (status, target ref,
/// branch tip) are returned as errors; only an absent branch ref reads as no
/// tip. `git merge-base --is-ancestor` failures still count against removal.
fn facts_for(path: &str, branch: &str, target: &str, root: &Path) -> Result<PruneFacts> {
    let dir = Path::new(path);
    let branch_opt = (!branch.is_empty()).then(|| branch.to_string());
    let ancestor_of_target = branch_opt
        .as_deref()
        .is_some_and(|b| util::git_is_ancestor(dir, b, target) == Some(true));
    let tree_clean = GitLoc::Local(dir.to_path_buf())
        .git_text(&["status", "--porcelain"])
        .map(|status| status.is_empty())
        .map_err(|error| anyhow!("git status --porcelain: {error}"))?;
    let target_tip = GitLoc::Local(dir.to_path_buf())
        .git_text(&["rev-parse", "--verify", "-q", target])
        .map_err(|error| anyhow!("git rev-parse --verify -q {target}: {error}"))
        .with_context(|| format!("reading target ref {target}"))?;
    let tip_equals_target = match branch_opt.as_deref() {
        Some(branch) => branch_tip(dir, branch)?.is_some_and(|tip| tip == target_tip),
        None => false,
    };
    Ok(PruneFacts {
        path: path.to_string(),
        is_primary: Path::new(path) == root,
        is_target_branch: branch == target,
        branch: branch_opt,
        ancestor_of_target,
        tip_equals_target,
        tree_clean,
        live_cwd_pids: crate::platform::proc::pids_with_cwd_under(dir),
        held_by_session: false,
    })
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
    let mut branches: Vec<(String, String, Option<String>)> = Vec::new();
    let facts: Vec<PruneFacts> = super::list::collect(cfg)?
        .into_iter()
        .filter(|w| w.repo == root_s && w.exists)
        .map(|w| {
            let tip = branch_tip(Path::new(&w.path), &w.branch)?;
            branches.push((w.path.clone(), w.branch.clone(), tip));
            facts_for(&w.path, &w.branch, &target, &root)
        })
        .collect::<Result<Vec<_>>>()?;
    let plan = wt_prune::plan(facts);
    let (removable, kept) = wt_prune::counts(&plan);

    let mut failed: Vec<(String, String)> = Vec::new();
    let mut removed = 0usize;
    let mut skipped: Vec<(String, String)> = Vec::new();
    if args.apply {
        let tips: HashMap<String, Option<String>> = branches
            .iter()
            .map(|(p, _, t)| (p.clone(), t.clone()))
            .collect();
        let branch_of: HashMap<&str, &str> = branches
            .iter()
            .map(|(p, b, _)| (p.as_str(), b.as_str()))
            .collect();
        let out = apply_plan(
            &plan,
            &tips,
            &branch_of,
            &target,
            &root,
            &mut |path, del| super::wt::rm_with(cfg, path, del, true, !args.json),
        );
        removed = out.removed;
        skipped = out.skipped;
        failed = out.failed;
    }
    let kept = kept + skipped.len();

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
            "skipped": skipped.iter().map(|(p, e)| serde_json::json!({"path": p, "reason": e})).collect::<Vec<_>>(),
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
        for (path, why) in &skipped {
            thegn_core::msg::warn(&format!("kept {path}: {why}"));
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
    finish(&failed)
}

fn finish(failed: &[(String, String)]) -> Result<()> {
    if failed.is_empty() {
        Ok(())
    } else {
        anyhow::bail!("{} worktree(s) could not be removed", failed.len())
    }
}

/// Outcome of applying a plan.
#[derive(Default)]
struct Applied {
    removed: usize,
    /// Planned for removal but no longer safe at apply time.
    skipped: Vec<(String, String)>,
    failed: Vec<(String, String)>,
}

/// Remove every `Remove` entry. Each worktree's facts are recomputed right
/// before its removal and the removal is skipped unless the decision still
/// holds and the branch tip is unchanged since planning, so work written in
/// between is never destroyed. A failing removal does not stop the rest.
fn apply_plan(
    plan: &[(String, PruneDecision)],
    tips: &HashMap<String, Option<String>>,
    branch_of: &HashMap<&str, &str>,
    target: &str,
    root: &Path,
    remove: &mut dyn FnMut(&str, bool) -> Result<()>,
) -> Applied {
    let mut out = Applied::default();
    for (path, decision) in plan {
        if !matches!(decision, PruneDecision::Remove { .. }) {
            continue;
        }
        let branch = branch_of.get(path.as_str()).copied().unwrap_or("");
        let fresh = match facts_for(path, branch, target, root) {
            Ok(fresh) => fresh,
            Err(error) => {
                out.skipped.push((
                    path.clone(),
                    format!("could not recheck Git facts: {error:#}"),
                ));
                continue;
            }
        };
        let PruneDecision::Remove { delete_branch } = wt_prune::decide(&fresh) else {
            let why = match wt_prune::decide(&fresh) {
                PruneDecision::Keep(r) => r.describe(),
                PruneDecision::Remove { .. } => "changed",
            };
            out.skipped
                .push((path.clone(), format!("changed since planning: {why}")));
            continue;
        };
        let planned_tip = tips.get(path).cloned().flatten();
        let current_tip = match branch_tip(Path::new(path), branch) {
            Ok(tip) => tip,
            Err(error) => {
                out.skipped.push((
                    path.clone(),
                    format!("could not recheck branch tip: {error:#}"),
                ));
                continue;
            }
        };
        if planned_tip.is_none() || current_tip != planned_tip {
            out.skipped
                .push((path.clone(), "branch moved since planning".to_string()));
            continue;
        }
        match remove(path, delete_branch) {
            Ok(()) => out.removed += 1,
            Err(why) => out.failed.push((path.clone(), format!("{why:#}"))),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) {
        assert!(
            util::git_ok(
                dir,
                ["-c", "commit.gpgsign=false"]
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
        git(&wt, &["commit", "--allow-empty", "-q", "-m", "work"]);
        git(
            main.path(),
            &["merge", "--no-ff", "-q", "-m", "merge", "tg/merged"],
        );
        let f = facts_for(wt.to_str().unwrap(), "tg/merged", "main", main.path()).unwrap();
        assert!(f.ancestor_of_target && f.tree_clean && !f.is_primary && !f.is_target_branch);
        assert!(matches!(
            wt_prune::decide(&f),
            PruneDecision::Remove {
                delete_branch: true
            }
        ));

        git(&wt, &["commit", "--allow-empty", "-q", "-m", "ahead"]);
        let f = facts_for(wt.to_str().unwrap(), "tg/merged", "main", main.path()).unwrap();
        assert!(!f.ancestor_of_target);
    }

    #[test]
    fn untracked_file_marks_the_tree_dirty_and_primary_is_flagged() {
        let main = repo();
        std::fs::write(main.path().join("stray"), "x").unwrap();
        let f = facts_for(main.path().to_str().unwrap(), "main", "main", main.path()).unwrap();
        assert!(!f.tree_clean && f.is_primary && f.is_target_branch);
    }

    #[test]
    fn a_process_cwd_inside_the_worktree_is_reported() {
        // This test process's own cwd is inside the tree being probed.
        let cwd = std::env::current_dir().unwrap();
        let f = facts_for(cwd.to_str().unwrap(), "", "main", Path::new("/nonexistent")).unwrap();
        if cfg!(target_os = "linux") {
            assert!(f.live_cwd_pids.contains(&std::process::id()));
        }
        assert!(f.branch.is_none());
    }

    #[test]
    fn unreadable_worktree_git_facts_return_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let branch_error = branch_tip(dir.path(), "main").unwrap_err();
        assert!(
            branch_error.to_string().contains("git rev-parse"),
            "{branch_error}"
        );
        let error = facts_for(
            dir.path().to_str().unwrap(),
            "main",
            "main",
            Path::new("/nonexistent"),
        )
        .unwrap_err();
        assert!(error.to_string().contains("git"), "{error}");
    }

    #[test]
    fn missing_target_ref_is_not_treated_as_a_matching_tip() {
        let main = repo();
        assert_eq!(branch_tip(main.path(), "no-such-branch").unwrap(), None);
        let error = facts_for(
            main.path().to_str().unwrap(),
            "main",
            "no-such-target",
            main.path(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("no-such-target"), "{error}");
    }

    /// Merge `branch` into main with a merge commit so the lane tip differs
    /// from the target tip (an ff-merged tip would count as "no work").
    fn merged_lane(main: &Path, name: &str) -> std::path::PathBuf {
        let wt = main.join(format!("wt-{name}"));
        git(
            main,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                &format!("tg/{name}"),
                wt.to_str().unwrap(),
            ],
        );
        git(&wt, &["commit", "--allow-empty", "-q", "-m", "work"]);
        git(
            main,
            &[
                "merge",
                "--no-ff",
                "-q",
                "-m",
                "merge",
                &format!("tg/{name}"),
            ],
        );
        wt
    }

    type OneResult = (
        Vec<(String, PruneDecision)>,
        HashMap<String, Option<String>>,
        String,
    );

    fn one(main: &Path, wt: &Path, name: &str) -> OneResult {
        let path = wt.to_str().unwrap().to_string();
        let branch = format!("tg/{name}");
        let f = facts_for(&path, &branch, "main", main).unwrap();
        let tips = HashMap::from([(path.clone(), branch_tip(wt, &branch).unwrap())]);
        (wt_prune::plan(vec![f]), tips, branch)
    }

    #[test]
    fn fresh_branch_at_target_tip_is_kept() {
        let main = repo();
        let wt = main.path().join("wt-fresh");
        git(
            main.path(),
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "tg/fresh",
                wt.to_str().unwrap(),
            ],
        );
        let f = facts_for(wt.to_str().unwrap(), "tg/fresh", "main", main.path()).unwrap();
        assert!(f.tip_equals_target && f.ancestor_of_target);
        assert!(matches!(
            wt_prune::decide(&f),
            PruneDecision::Keep(wt_prune::KeepReason::NoWork)
        ));
    }

    #[test]
    fn apply_removes_merged_lane_and_passes_delete_branch() {
        let main = repo();
        let wt = merged_lane(main.path(), "ok");
        let (plan, tips, branch) = one(main.path(), &wt, "ok");
        let branch_of = HashMap::from([(wt.to_str().unwrap(), branch.as_str())]);
        let mut seen = Vec::new();
        let out = apply_plan(
            &plan,
            &tips,
            &branch_of,
            "main",
            main.path(),
            &mut |p, d| {
                seen.push((p.to_string(), d));
                Ok(())
            },
        );
        assert_eq!(out.removed, 1);
        assert_eq!(seen, vec![(wt.to_str().unwrap().to_string(), true)]);
    }

    #[test]
    fn lane_dirtied_between_plan_and_apply_is_kept() {
        let main = repo();
        let wt = merged_lane(main.path(), "dirty");
        let (plan, tips, branch) = one(main.path(), &wt, "dirty");
        assert!(matches!(plan[0].1, PruneDecision::Remove { .. }));
        std::fs::write(wt.join("late"), "work").unwrap();
        let branch_of = HashMap::from([(wt.to_str().unwrap(), branch.as_str())]);
        let mut called = false;
        let out = apply_plan(
            &plan,
            &tips,
            &branch_of,
            "main",
            main.path(),
            &mut |_, _| {
                called = true;
                Ok(())
            },
        );
        assert!(!called && out.removed == 0 && out.skipped.len() == 1);
    }

    #[test]
    fn lane_gaining_a_commit_between_plan_and_apply_is_kept() {
        let main = repo();
        let wt = merged_lane(main.path(), "moved");
        let (plan, tips, branch) = one(main.path(), &wt, "moved");
        git(&wt, &["commit", "--allow-empty", "-q", "-m", "late"]);
        let branch_of = HashMap::from([(wt.to_str().unwrap(), branch.as_str())]);
        let mut called = false;
        let out = apply_plan(
            &plan,
            &tips,
            &branch_of,
            "main",
            main.path(),
            &mut |_, _| {
                called = true;
                Ok(())
            },
        );
        assert!(!called && out.skipped.len() == 1);
    }

    #[test]
    fn a_failed_removal_does_not_stop_the_rest_and_exits_nonzero() {
        let main = repo();
        let a = merged_lane(main.path(), "a");
        let b = merged_lane(main.path(), "b");
        let mut plan = Vec::new();
        let mut tips = HashMap::new();
        let mut names = Vec::new();
        for (wt, n) in [(&a, "a"), (&b, "b")] {
            let (p, t, br) = one(main.path(), wt, n);
            plan.extend(p);
            tips.extend(t);
            names.push((wt.to_str().unwrap().to_string(), br));
        }
        let branch_of: HashMap<&str, &str> = names
            .iter()
            .map(|(p, b)| (p.as_str(), b.as_str()))
            .collect();
        let first = a.to_str().unwrap().to_string();
        let out = apply_plan(
            &plan,
            &tips,
            &branch_of,
            "main",
            main.path(),
            &mut |p, _| {
                if p == first {
                    anyhow::bail!("boom")
                }
                Ok(())
            },
        );
        assert_eq!((out.removed, out.failed.len()), (1, 1));
        assert!(finish(&out.failed).is_err());
        assert!(finish(&[]).is_ok());
    }

    #[test]
    fn bare_prune_without_merged_is_refused() {
        let args = PruneArgs {
            merged: false,
            target: None,
            apply: false,
            json: false,
        };
        let err = run(&Config::default(), args).unwrap_err().to_string();
        assert!(err.contains("--merged"), "{err}");
    }
}
