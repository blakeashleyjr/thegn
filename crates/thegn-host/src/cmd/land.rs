//! `thegn land` — land the current worktree's branch onto the repo's target
//! branch (`main`) through the fold-actor, without the merge-queue machinery.
//!
//! This is the blessed one-shot alternative to `git checkout main && git merge`
//! or a hand-rolled `git update-ref`: the fold runs in the object DB (no target
//! checkout) and advances the target ref by compare-and-swap, so it lands even
//! when the main checkout's working tree is read-only to the caller (a sandboxed
//! agent). On a successful advance it fast-forwards every worktree that has the
//! target checked out — main or linked — and prints the exact resync command for
//! any it had to leave alone (see `thegn_core::util::resync_branch_checkouts`).
//! A running instance also self-heals on the ref move (see
//! [`crate::git_watch::spawn_main_checkout_heal`]), but that is a belt-and-braces
//! second path: the CLI must not depend on an instance being up.
//!
//! Unlike `thegn merge land`, this neither requires `[merge_queue] enabled`
//! nor touches the queue's DB rows; it shares only the fold/gate/CAS core
//! ([`crate::integrate::attempt_land`]).

use anyhow::{Context, Result};
use std::path::Path;
use thegn_core::config::Config;
use thegn_core::db::Db;
use thegn_core::merge_lifecycle::LifecycleEvent;
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
    let mut mq = cfg.repo_merge_queue(&root);
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
    // On a successful land, file the worktree into the Merged folder — the same
    // destination a queue land reaches under `move`/`expire`. `thegn land` shares
    // the fold/gate/CAS core with the queue but deliberately leaves the worktree
    // in place (no worktree/branch removal, no queue-row bookkeeping), so
    // `LandedInPlace` (file, never remove) is the deliberate event: it degrades
    // the destructive `remove`/`detach` arms to a plain filing because a scripted
    // `thegn land` is typically run from *inside* the worktree being landed and
    // must not delete the caller's cwd. Under `on_landed = "off"` it instead
    // clears any stale "Merging"/"Needs attention" membership its enqueue left, so
    // a fold-actor land never strands the worktree — the sidebar/queue de-sync.
    // Best-effort and guarded host-side to lifecycle folders, so a user-filed
    // folder is left alone and a DB hiccup never fails the land. It writes no queue
    // row, so the worktree it files is never an expiry-sweep candidate.
    let file_landed = |branch: &str| {
        if let Ok(db) = Db::open()
            && let Some(root) = integrate::main_checkout(&wt)
        {
            crate::merge_lifecycle::apply(
                // Repo-resolved, so a `[workspace.<slug>]` folder setting is
                // honored here as well as on the land itself.
                &cfg.repo_merge_queue(&root),
                &db,
                &root,
                &wt.to_string_lossy(),
                branch,
                LifecycleEvent::LandedInPlace,
            );
        }
    };
    match outcome {
        AttemptOutcome::Landed { commit, resyncs } => {
            file_landed(&branch);
            outln!(
                "✓ landed {branch} → {target} @ {}",
                &commit[..commit.len().min(12)]
            );
            crate::integrate::report_resyncs(&target, &resyncs);
        }
        AttemptOutcome::UpToDate => {
            file_landed(&branch);
            outln!("{branch} already in {target}.");
        }
        // A failed land must exit non-zero: `thegn land` is scripted (CI, the
        // fold-actor, git aliases), so an exit-0 conflict/gate-red would look
        // like a success. The message rides the returned error (anyhow prints it).
        AttemptOutcome::Conflict {
            paths,
            submodule_conflicts,
        } => {
            let detail =
                crate::integrate::conflict_details(&paths, &submodule_conflicts).join(", ");
            anyhow::bail!("{branch} conflicts with {target}: {detail}");
        }
        AttemptOutcome::GateFailed { .. } => {
            anyhow::bail!("{branch} breaks the build (gate red); not landed.");
        }
        AttemptOutcome::GateError { reason, .. } => {
            // The gate never ran, so this says nothing about the branch. Naming
            // it "breaks the build" would be a false accusation.
            anyhow::bail!(
                "{branch} was NOT gated — {reason}. The branch was not judged; \
                 fix the gate environment (see `[merge_queue] gate_setup_command`) \
                 and re-run."
            );
        }
        AttemptOutcome::Unreachable { detail } => {
            anyhow::bail!("{branch}: {detail}");
        }
        AttemptOutcome::Ready { .. } => {
            // Unreachable with auto_land forced on, but handle for completeness.
            anyhow::bail!("{branch} is ready but was not landed.");
        }
    }
    Ok(())
}
