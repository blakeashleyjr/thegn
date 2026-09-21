//! `thegn integrate` — batch-fold queued branches (the fold-actor).
//!
//! Folds branches into the repo's target branch in the object database, landing
//! the clean ones and deferring only the genuine conflicts. One command instead
//! of checking out main and merging each branch by hand.
//!
//! Which branches: those explicitly enqueued (`thegn merge add`), unless
//! `[merge_queue] require_enqueue = false` or `--all` widens it to every eligible
//! worktree branch. "Eligible" means clean and not on the target — a test that
//! cannot tell finished work from a branch you are still building, which is why
//! it is no longer the default.

use crate::integrate::{self, GateOutcome};
use anyhow::{Context, Result};
use std::path::PathBuf;
use thegn_core::config::Config;
use thegn_core::db::Db;
use thegn_core::outln;

fn short(oid: &str) -> &str {
    &oid[..oid.len().min(12)]
}

/// Flags for `thegn integrate`.
#[derive(clap::Args, Clone, Default)]
pub struct IntegrateArgs {
    /// Print the branches that would be folded and exit, changing nothing.
    #[arg(long)]
    pub dry_run: bool,
    /// Fold every eligible worktree branch, not just the queued ones.
    #[arg(long)]
    pub all: bool,
    /// Skip the confirmation prompt (required to fold non-interactively).
    #[arg(long, short = 'y')]
    pub yes: bool,
}

pub fn run(cfg: &Config, args: &IntegrateArgs) -> Result<()> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let repo_root = integrate::main_checkout(&cwd).context("not inside a git repository")?;
    // THE-515: an ambiguous trusted overlay is refused — never fold under the
    // weaker global gate in its place.
    if let Some(refusal) = cfg.workspace_overlay_refusal(&repo_root) {
        anyhow::bail!("{}: {refusal}", repo_root.display());
    }
    // Resolve the repo's effective `[merge_queue]` FIRST, so `enabled` (and
    // everything below) honors a `[workspace.<slug>]` refinement rather than the
    // bare global table.
    let resolved = cfg.repo_merge_queue(&repo_root);
    if !resolved.enabled {
        outln!(
            "Merge queue disabled. Set `[merge_queue]` `enabled = true` in your config to use it."
        );
        return Ok(());
    }
    // `push` mode lands the sprite's OWN clone and pushes to origin, so it skips
    // the remote-target guard (which otherwise redirects an off-host target to
    // its host). In `route_to_host` mode the fold still runs in the target repo's
    // object store, so a remote target must be folded on its own host.
    let push_mode = resolved.remote_mode == thegn_core::config::MergeRemoteMode::Push;
    if !push_mode {
        let db = Db::open().context("remote-target guard database unavailable")?;
        if let Some(msg) = crate::merge_ops::remote_target_guard(&db, &repo_root)? {
            outln!("{msg}");
            return Ok(());
        }
    }
    let mq = &resolved;
    let target = integrate::resolve_target(mq, &repo_root);

    let override_gpg = cfg.repo_git(&repo_root).override_gpg;
    let mut cands = integrate::candidate_branches(mq, &repo_root, &target)?;
    for s in &cands.skipped_dirty {
        outln!("  • skipped {s} (dirty — set [merge_queue] snapshot_dirty = true to fold it)");
    }

    // `--all` is the explicit, per-run opt out of the queue: fold every eligible
    // branch, the pre-`require_enqueue` behavior, without editing config.
    let require_enqueue = mq.require_enqueue && !args.all;
    if require_enqueue {
        let enqueued = Db::open()
            .map(|db| integrate::enqueued_worktrees(&db, &target))
            .unwrap_or_default();
        let held = integrate::hold_unenqueued(&mut cands, &enqueued);
        if !held.is_empty() {
            outln!(
                "  • not queued, left alone: {} ({} branch(es))",
                held.join(", "),
                held.len()
            );
        }
    }

    if cands.branches.is_empty() {
        outln!("Nothing to integrate into {target}.");
        if require_enqueue {
            outln!(
                "  Queue a branch with `thegn merge add` (or `--all`), or fold every\n  \
                 eligible branch this once with `thegn integrate --all`."
            );
        }
        return Ok(());
    }

    // The plan is printed BEFORE anything is folded, always — a dry run just
    // stops here. Naming each branch is the point: the failure this guards is
    // "I did not know that worktree counted as eligible".
    outln!(
        "{} {} branch(es) into {target}{}:",
        if args.dry_run {
            "Would fold"
        } else {
            "Folding"
        },
        cands.branches.len(),
        if mq.gate_on && !mq.gate_command.is_empty() {
            format!(" (gate: {})", mq.gate_command)
        } else {
            String::new()
        }
    );
    for b in &cands.branches {
        outln!(
            "    {}{}",
            b.name,
            if cands.pending_snapshots.contains(&b.name) {
                " (dirty; snapshot only after confirmation)"
            } else {
                ""
            }
        );
    }
    if args.dry_run {
        outln!(
            "Dry run — no candidate snapshots, folds or queue changes. State initialization may occur."
        );
        return Ok(());
    }
    // Advancing the target and committing selected dirty snapshots are writes
    // that require confirmation. Cleanup is a separate explicit action; this
    // command does not expire historical worktrees or delete their branches.
    // Non-interactive callers must say `--yes` rather than silently confirming.
    if !args.yes {
        use std::io::IsTerminal;
        if !std::io::stdin().is_terminal() {
            anyhow::bail!(
                "Refusing to fold non-interactively without `--yes`; use `--dry-run` to preview or `--yes` to proceed."
            );
        }
        if !super::confirm(&format!(
            "Fold {} branch(es) into {target}?",
            cands.branches.len()
        )) {
            outln!("Aborted — nothing folded.");
            return Ok(());
        }
    }

    let report = integrate::run_selected_fold(mq, &repo_root, &cands, override_gpg)?;
    // Explicit integration authorizes the listed candidates, not expiry of
    // unrelated historical worktrees. Cleanup remains an explicit sweep action.

    for l in &report.landed {
        outln!("  ✓ landed {} → {}", l.branch, short(&l.commit));
    }
    for prepared in &report.prepared {
        outln!("  • held {} (prepared only; not landed)", prepared.branch);
    }
    for branch in &report.unprepared {
        outln!("  • held {branch} (fold preparation unavailable; not landed)");
    }
    for d in &report.deferred {
        if d.gate_failed {
            outln!(
                "  ✗ {} held back — breaks the build (gate offender)",
                d.branch
            );
        } else {
            outln!(
                "  ✗ {} deferred — conflicts: {}",
                d.branch,
                d.paths.join(", ")
            );
        }
    }
    match &report.gate {
        GateOutcome::Passed => outln!("Gate passed."),
        GateOutcome::Failed { offender } => match offender {
            Some(b) => outln!("Gate failed — isolated {b}; {target} not advanced by this fold."),
            None => outln!("Gate failed — {target} not advanced by this fold."),
        },
        GateOutcome::Errored { reason } => {
            // Not a verdict about any branch — say so, and say what to fix.
            outln!("Gate could NOT RUN — {reason}; {target} not advanced by this fold.");
            outln!("  No branch was blamed. Check `[merge_queue] gate_command`");
            outln!("  and `gate_setup_command` — the gate worktree is a bare");
            outln!("  checkout with no dependencies installed.");
        }
        GateOutcome::Skipped => {}
    }
    if !report.diagnostics.is_empty() {
        outln!("{}", report.diagnostics);
    }
    if let Some(error) = &report.bookkeeping_error {
        outln!("Queue bookkeeping failed (Git outcome unchanged): {error}");
    }
    if report.advanced {
        let retried = if report.cas_attempts > 1 {
            format!(
                " ({} CAS attempts — {target} moved under the fold)",
                report.cas_attempts
            )
        } else {
            String::new()
        };
        outln!(
            "{target} advanced {} → {}{retried}.",
            short(&report.original),
            short(&report.final_tip)
        );
        crate::integrate::report_resyncs(&target, &report.resyncs);
    } else {
        outln!(
            "{target} not advanced by this fold (starting tip {}).",
            short(&report.original)
        );
    }
    // push mode: converge by pushing the advanced target to origin.
    if push_mode && report.advanced {
        match crate::merge_ops::push_target(&repo_root, &target) {
            Ok(()) => outln!("Pushed {target} to origin."),
            Err(e) => {
                outln!("Push failed — {target} advanced locally but NOT on origin: {e}");
                return Err(e);
            }
        }
    }
    report.request_result()
}
