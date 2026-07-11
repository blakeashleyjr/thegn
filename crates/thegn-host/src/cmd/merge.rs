//! `thegn merge` — the agent-driven merge-queue namespace.
//!
//! Assign worktree branches to the queue (`add`) and drain them one at a time
//! (`drain`): each branch is folded onto the repo's target in the object DB, and
//! one that conflicts or fails the gate is handed to the configured headless CLI
//! agent (in the branch's own worktree) to rebase/resolve/fix, then re-attempted.
//! The batch, fold-everything-at-once path is still `thegn integrate`.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use thegn_core::config::Config;
use thegn_core::db::Db;
use thegn_core::outln;
use thegn_core::store::WorktreeAuxStore;

use thegn_core::merge_lifecycle::LifecycleEvent;

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

/// Queue rows belonging to the current repo (the membership rule lives in
/// `merge_driver::rows_for_repo`, shared with the host's in-app drain).
fn rows_for_repo(root: &Path) -> Result<Vec<thegn_core::db::MergeQueueRow>> {
    let db = Db::open()?;
    Ok(merge_driver::rows_for_repo(&db, root))
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
            crate::merge_lifecycle::apply(mq, &db, &root, wt, branch, LifecycleEvent::Enqueued);
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
        let msg = crate::merge_ops::enqueue_worktree(mq, &db, &wt)?;
        let mark = if msg.starts_with("skipped") {
            "•"
        } else {
            "+"
        };
        outln!("  {mark} {msg}");
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
    let n = crate::merge_ops::clear_repo(&db, &root)?;
    let _ = cfg;
    outln!("Queue cleared ({n} removed).");
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
    let wt = super::resolve_worktree(worktree);
    let wt_s = wt.to_string_lossy().to_string();
    // Share the fold/gate/CAS core with `thegn land`; this queue-aware path
    // additionally records the outcome on the worktree's merge-queue row.
    let (branch, _target, outcome) = super::land::land_branch(cfg, &wt)?;
    let db = Db::open()?;
    // Apply the sidebar-folder lifecycle for this worktree once we know its fate.
    let lifecycle = |event: LifecycleEvent| {
        if let Some(root) = integrate::main_checkout(&wt) {
            crate::merge_lifecycle::apply(&cfg.merge_queue, &db, &root, &wt_s, &branch, event);
        }
    };
    match outcome {
        AttemptOutcome::Landed { commit } => {
            let _ = db.update_merge_status(&wt_s, "landed", Some(&commit), None, None);
            lifecycle(LifecycleEvent::Landed);
            outln!("✓ landed {branch} → {}", &commit[..commit.len().min(12)]);
        }
        AttemptOutcome::UpToDate => {
            let _ = db.update_merge_status(&wt_s, "landed", None, Some("already merged"), None);
            lifecycle(LifecycleEvent::Landed);
            outln!("{branch} already merged.");
        }
        AttemptOutcome::Conflict { paths } => {
            lifecycle(LifecycleEvent::Failed);
            outln!("✗ {branch} conflicts: {}", paths.join(", "));
        }
        AttemptOutcome::GateFailed { .. } => {
            lifecycle(LifecycleEvent::Failed);
            outln!("✗ {branch} breaks the build (gate red).");
        }
        AttemptOutcome::Ready { .. } => {
            // Unreachable with auto_land forced on, but handle for completeness.
            outln!("{branch} is ready but was not landed.");
        }
    }
    Ok(())
}
