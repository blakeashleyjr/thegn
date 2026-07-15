//! `thegn land` — land the current worktree's branch onto the repo's target
//! branch (`main`) through the fold-actor, without the merge-queue machinery.
//!
//! This is the blessed one-shot alternative to `git checkout main && git merge`
//! or a hand-rolled `git update-ref`: the fold runs in the object DB (no target
//! checkout) and advances the target ref by compare-and-swap, so it lands even
//! when the main checkout's working tree is read-only to the caller (a sandboxed
//! agent). The working-tree sync then defers to the running instance's own
//! self-heal — a clean checkout on the target fast-forwards itself once it sees
//! the ref move (see [`crate::git_watch::spawn_main_checkout_heal`]).
//!
//! Unlike `thegn merge land`, this neither requires `[merge_queue] enabled`
//! nor touches the queue's DB rows; it shares only the fold/gate/CAS core
//! ([`crate::integrate::attempt_land`]).

use anyhow::{Context, Result};
use std::path::Path;
use thegn_core::config::Config;
use thegn_core::db::Db;
use thegn_core::{outln, util};

use crate::integrate::{self, AttemptOutcome};

/// Fold `worktree`'s current branch onto the repo target via the fold-actor's
/// CAS land, forcing the land regardless of the configured `auto_land`. Returns
/// `(branch, target, outcome)`. No DB / queue side effects — callers that want
/// queue bookkeeping (`merge land`) record it from the returned outcome.
pub(crate) fn land_branch(
    cfg: &Config,
    worktree: &Path,
) -> Result<(String, String, AttemptOutcome)> {
    let root = integrate::main_checkout(worktree).context("not inside a git repository")?;
    let branch = util::git_out(worktree, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .with_context(|| format!("{}: not on a branch (detached HEAD?)", worktree.display()))?;
    // This IS the manual land, so force it on regardless of queue policy.
    let mut mq = cfg.merge_queue.clone();
    mq.auto_land = true;
    let target = integrate::resolve_target(&mq, &root);
    // `thegn land` lands the branch checked out in `worktree`; its loc tells
    // attempt_land whether that worktree is on this host (no ingest) or remote.
    let branch_loc = thegn_core::remote::GitLoc::for_worktree(worktree);
    let outcome = integrate::attempt_land(&mq, &root, &branch, &branch_loc)?;
    Ok((branch, target, outcome))
}

pub fn run(cfg: &Config, worktree: Option<String>) -> Result<()> {
    let wt = super::resolve_worktree(worktree);
    if let Ok(db) = Db::open()
        && let Some(root) = integrate::main_checkout(&wt)
        && let Some(msg) = crate::merge_ops::remote_target_guard(&db, &root)
    {
        outln!("{msg}");
        return Ok(());
    }
    let (branch, target, outcome) = land_branch(cfg, &wt)?;
    match outcome {
        AttemptOutcome::Landed { commit } => {
            outln!(
                "✓ landed {branch} → {target} @ {}",
                &commit[..commit.len().min(12)]
            );
        }
        AttemptOutcome::UpToDate => outln!("{branch} already in {target}."),
        AttemptOutcome::Conflict { paths } => {
            outln!("✗ {branch} conflicts with {target}: {}", paths.join(", "));
        }
        AttemptOutcome::GateFailed { .. } => {
            outln!("✗ {branch} breaks the build (gate red); not landed.");
        }
        AttemptOutcome::Unreachable { detail } => {
            outln!("✗ {branch}: {detail}");
        }
        AttemptOutcome::Ready { .. } => {
            // Unreachable with auto_land forced on, but handle for completeness.
            outln!("{branch} is ready but was not landed.");
        }
    }
    Ok(())
}
