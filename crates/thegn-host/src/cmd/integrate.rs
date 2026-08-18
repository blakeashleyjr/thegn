//! `thegn integrate` — drain the local merge queue (the fold-actor).
//!
//! Folds every eligible worktree branch into the repo's target branch in the
//! object database, landing the clean ones automatically and deferring only the
//! genuine conflicts. One command instead of checking out main and merging each
//! branch by hand.

use crate::integrate::{self, GateOutcome};
use anyhow::{Context, Result};
use std::path::PathBuf;
use thegn_core::config::Config;
use thegn_core::db::Db;
use thegn_core::outln;

fn short(oid: &str) -> &str {
    &oid[..oid.len().min(12)]
}

pub fn run(cfg: &Config) -> Result<()> {
    if !cfg.merge_queue.enabled {
        outln!(
            "Merge queue disabled. Set `[merge_queue]` `enabled = true` in your config to use it."
        );
        return Ok(());
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let repo_root = integrate::main_checkout(&cwd).context("not inside a git repository")?;
    // `push` mode lands the sprite's OWN clone and pushes to origin, so it skips
    // the remote-target guard (which otherwise redirects an off-host target to
    // its host). In `route_to_host` mode the fold still runs in the target repo's
    // object store, so a remote target must be folded on its own host.
    let push_mode = cfg.merge_queue.remote_mode == thegn_core::config::MergeRemoteMode::Push;
    if !push_mode
        && let Ok(db) = Db::open()
        && let Some(msg) = crate::merge_ops::remote_target_guard(&db, &repo_root)
    {
        outln!("{msg}");
        return Ok(());
    }
    let mq = &cfg.merge_queue;
    let target = integrate::resolve_target(mq, &repo_root);

    let cands = integrate::candidate_branches(mq, &repo_root, &target)?;
    for s in &cands.skipped_dirty {
        outln!("  • skipped {s} (dirty — set [merge_queue] snapshot_dirty = true to fold it)");
    }
    if cands.branches.is_empty() {
        outln!("Nothing to integrate into {target}.");
        return Ok(());
    }
    outln!(
        "Folding {} branch(es) into {target}{}…",
        cands.branches.len(),
        if mq.gate_on && !mq.gate_command.is_empty() {
            format!(" (gate: {})", mq.gate_command)
        } else {
            String::new()
        }
    );

    let report = integrate::run_fold(mq, &repo_root, cands.branches.clone())?;
    if let Ok(db) = Db::open() {
        let _ = integrate::persist(mq, &repo_root, &db, &cands, &report);
    }

    for l in &report.landed {
        outln!("  ✓ landed {} → {}", l.branch, short(&l.commit));
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
            Some(b) => outln!("Gate failed — isolated {b}; main not advanced."),
            None => outln!("Gate failed — main not advanced."),
        },
        GateOutcome::Errored { reason } => {
            // Not a verdict about any branch — say so, and say what to fix.
            outln!("Gate could NOT RUN — {reason}; {target} not advanced.");
            outln!("  No branch was blamed. Check `[merge_queue] gate_command`");
            outln!("  and `gate_setup_command` — the gate worktree is a bare");
            outln!("  checkout with no dependencies installed.");
        }
        GateOutcome::Skipped => {}
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
        outln!("{target} unchanged ({}).", short(&report.original));
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
    Ok(())
}
