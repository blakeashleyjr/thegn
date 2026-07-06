//! `superzej merge` — the agent-driven merge-queue namespace.
//!
//! Assign worktree branches to the queue (`add`) and drain them one at a time
//! (`drain`): each branch is folded onto the repo's target in the object DB, and
//! one that conflicts or fails the gate is handed to the configured headless CLI
//! agent (in the branch's own worktree) to rebase/resolve/fix, then re-attempted.
//! The batch, fold-everything-at-once path is still `superzej integrate`.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use superzej_core::config::Config;
use superzej_core::db::Db;
use superzej_core::store::WorktreeAuxStore;
use superzej_core::{outln, util};

use crate::integrate::{self, AttemptOutcome};
use crate::merge_driver::{self, DriveStep, QueueItem};

#[derive(clap::Subcommand, Clone)]
pub enum Action {
    /// Show the merge queue.
    List {
        /// Emit one JSON array instead of the human table.
        #[arg(long)]
        json: bool,
    },
    /// Assign worktree branch(es) to the queue.
    Add {
        /// Worktree paths to enqueue (default: the current worktree).
        worktrees: Vec<String>,
        /// Enqueue every eligible worktree branch in this repo.
        #[arg(long)]
        all: bool,
    },
    /// Remove a worktree from the queue.
    Rm {
        /// Worktree path (default: the current worktree).
        worktree: Option<String>,
    },
    /// Empty the queue for this repo.
    Clear,
    /// Process the queue one branch at a time (the agent autopilot).
    Drain {
        /// Enqueue every eligible branch first, then drain.
        #[arg(long)]
        all: bool,
        /// Emit a JSON summary instead of the human log.
        #[arg(long)]
        json: bool,
    },
    /// Land a branch that is `ready` (gated green, held by `auto_land = false`).
    Land {
        /// Worktree path (default: the current worktree).
        worktree: Option<String>,
    },
}

pub fn run(cfg: &Config, action: Action) -> Result<()> {
    if !cfg.merge_queue.enabled {
        outln!(
            "Merge queue disabled. Set `[merge_queue]` `enabled = true` in your config to use it."
        );
        return Ok(());
    }
    match action {
        Action::List { json } => list(json),
        Action::Add { worktrees, all } => add(cfg, worktrees, all),
        Action::Rm { worktree } => rm(worktree),
        Action::Clear => clear(cfg),
        Action::Drain { all, json } => drain(cfg, all, json),
        Action::Land { worktree } => land(cfg, worktree),
    }
}

/// The repo root (main checkout) reachable from the cwd.
fn repo_root() -> Result<PathBuf> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    integrate::main_checkout(&cwd).context("not inside a git repository")
}

/// The branch a worktree is currently on.
fn branch_of(worktree: &Path) -> Option<String> {
    util::git_out(worktree, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Queue rows belonging to the current repo (the queue is global; a drain is
/// per-repo because the target ref is).
fn rows_for_repo(root: &Path) -> Result<Vec<superzej_core::db::MergeQueueRow>> {
    let db = Db::open()?;
    Ok(db
        .list_merge_queue()?
        .into_iter()
        .filter(|r| integrate::main_checkout(Path::new(&r.worktree)).as_deref() == Some(root))
        .collect())
}

fn list(json: bool) -> Result<()> {
    let db = Db::open()?;
    let rows = db.list_merge_queue()?;
    if json {
        return super::emit_json(&rows);
    }
    if rows.is_empty() {
        outln!("Merge queue empty.");
        return Ok(());
    }
    for r in &rows {
        let detail = r
            .conflict_paths
            .as_deref()
            .or(r.error_detail.as_deref())
            .map(|d| format!("  — {}", d.replace('\n', ", ")))
            .unwrap_or_default();
        outln!("  {} {} → {}{detail}", r.status, r.branch, r.target_branch);
    }
    Ok(())
}

fn add(cfg: &Config, worktrees: Vec<String>, all: bool) -> Result<()> {
    let root = repo_root()?;
    let mq = &cfg.merge_queue;
    let target = integrate::resolve_target(mq, &root);
    let db = Db::open()?;

    if all {
        let cands = integrate::candidate_branches(mq, &root, &target)?;
        for s in &cands.skipped_dirty {
            outln!("  • skipped {s} (dirty — set [merge_queue] snapshot_dirty = true to queue it)");
        }
        for (branch, wt) in &cands.worktrees {
            db.enqueue_merge(wt, branch, &target)?;
            outln!("  + queued {branch}");
        }
        return Ok(());
    }

    let paths = if worktrees.is_empty() {
        vec![super::resolve_worktree(None)]
    } else {
        worktrees.iter().map(PathBuf::from).collect()
    };
    for wt in paths {
        let wt_s = wt.to_string_lossy().to_string();
        let branch =
            branch_of(&wt).with_context(|| format!("{wt_s}: not on a branch (detached HEAD?)"))?;
        if branch == target {
            outln!("  • skipped {branch} (that's the target branch)");
            continue;
        }
        db.enqueue_merge(&wt_s, &branch, &target)?;
        outln!("  + queued {branch}");
    }
    Ok(())
}

fn rm(worktree: Option<String>) -> Result<()> {
    let wt = super::resolve_worktree(worktree);
    let db = Db::open()?;
    db.remove_merge_entry(&wt.to_string_lossy())?;
    outln!("Removed from queue.");
    Ok(())
}

fn clear(cfg: &Config) -> Result<()> {
    let root = repo_root()?;
    let db = Db::open()?;
    for r in rows_for_repo(&root)? {
        db.remove_merge_entry(&r.worktree)?;
    }
    let _ = cfg;
    outln!("Queue cleared.");
    Ok(())
}

fn drain(cfg: &Config, all: bool, json: bool) -> Result<()> {
    let root = repo_root()?;
    let mq = &cfg.merge_queue;
    if all {
        add(cfg, Vec::new(), true)?;
    }
    let items: Vec<QueueItem> = rows_for_repo(&root)?
        .into_iter()
        .filter(|r| r.status != "landed" && r.status != "ready")
        .map(|r| QueueItem {
            worktree: r.worktree,
            branch: r.branch,
        })
        .collect();
    if items.is_empty() {
        outln!("Nothing to drain.");
        return Ok(());
    }
    let target = integrate::resolve_target(mq, &root);
    outln!(
        "Draining {} branch(es) into {target}{}…",
        items.len(),
        if mq.gate_on && !mq.gate_command.is_empty() {
            format!(" (gate: {})", mq.gate_command)
        } else {
            String::new()
        }
    );

    let db = Db::open()?;
    let out = merge_driver::drive_queue(mq, &root, &db, items, |step: &DriveStep| {
        // Only the settled transitions are worth a CLI line; folding/agent_running
        // are transient and would just be noise before the outcome.
        match step.status {
            "landed" => outln!("  ✓ landed {} ({})", step.branch, step.detail),
            "ready" => outln!("  ◆ ready  {} ({})", step.branch, step.detail),
            "deferred" | "gate_failed" => {
                outln!("  ✗ {} deferred — {}", step.branch, step.detail)
            }
            "needs_human" => outln!("  ⚑ {} needs a human — {}", step.branch, step.detail),
            "agent_running" => outln!("  … {} — {}", step.branch, step.detail),
            _ => {}
        }
    });

    if json {
        super::emit_json(&serde_json::json!({
            "landed": out.landed,
            "ready": out.ready,
            "deferred": out.deferred,
            "needs_human": out.needs_human,
        }))?;
    } else {
        outln!(
            "Done: {} landed, {} ready, {} deferred, {} need a human.",
            out.landed.len(),
            out.ready.len(),
            out.deferred.len(),
            out.needs_human.len()
        );
    }
    Ok(())
}

fn land(cfg: &Config, worktree: Option<String>) -> Result<()> {
    let root = repo_root()?;
    let wt = super::resolve_worktree(worktree);
    let wt_s = wt.to_string_lossy().to_string();
    let branch = branch_of(&wt).with_context(|| format!("{wt_s}: not on a branch"))?;
    // Force the land regardless of the configured auto_land (this IS the manual land).
    let mut mq = cfg.merge_queue.clone();
    mq.auto_land = true;
    let db = Db::open()?;
    match integrate::attempt_land(&mq, &root, &branch)? {
        AttemptOutcome::Landed { commit } => {
            let _ = db.update_merge_status(&wt_s, "landed", Some(&commit), None, None);
            outln!("✓ landed {branch} → {}", &commit[..commit.len().min(12)]);
        }
        AttemptOutcome::UpToDate => {
            let _ = db.update_merge_status(&wt_s, "landed", None, Some("already merged"), None);
            outln!("{branch} already merged.");
        }
        AttemptOutcome::Conflict { paths } => {
            outln!("✗ {branch} conflicts: {}", paths.join(", "));
        }
        AttemptOutcome::GateFailed { .. } => {
            outln!("✗ {branch} breaks the build (gate red).");
        }
        AttemptOutcome::Ready { .. } => {
            // Unreachable with auto_land forced on, but handle for completeness.
            outln!("{branch} is ready but was not landed.");
        }
    }
    Ok(())
}
