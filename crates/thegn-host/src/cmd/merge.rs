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
        #[command(flatten)]
        target: super::target::WorktreeTarget,
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
        #[command(flatten)]
        target: super::target::WorktreeTarget,
    },
    /// Re-arm a blocked branch for the next drain: back to `queued`, agent
    /// budget reset, prior failure detail cleared. The "I fixed it, try again"
    /// gesture (the panel's `r` key).
    Retry {
        #[command(flatten)]
        target: super::target::WorktreeTarget,
    },
}

pub fn run(cfg: &Config, action: Action) -> Result<()> {
    if !cfg.merge_queue.enabled {
        // Refusal, not success: bail so the process exits non-zero — scripts/CI
        // must be able to tell "did nothing because disabled" from "did the work".
        anyhow::bail!(
            "Merge queue disabled. Set `[merge_queue]` `enabled = true` in your config to use it."
        );
    }
    match action {
        Action::List { json } => list(json),
        Action::Add { worktrees, all } => add(cfg, worktrees, all),
        Action::Rm { target } => rm(cfg, target.get()),
        Action::Clear => clear(cfg),
        Action::Drain { all, json } => drain(cfg, all, json),
        Action::Land { target } => land(cfg, target.get()),
        Action::Retry { target } => retry(target.get()),
    }
}

/// `merge retry [worktree]` — re-arm a blocked row.
///
/// A plain drain already re-attempts every non-settled row, but the agent budget
/// is now persisted per branch, so an exhausted `needs_human` row would keep
/// deferring without ever dispatching again. This resets it — and gives the
/// gesture a name in `merge --help`, which it never had on the CLI even though
/// the panel has bound `r` to it all along.
fn retry(worktree: Option<String>) -> Result<()> {
    let wt = crate::merge_ops::canonical_worktree(&super::resolve_worktree(worktree));
    let wt_s = wt.to_string_lossy().to_string();
    let db = Db::open()?;
    if !db.retry_merge_entry(&wt_s)? {
        // Not queued is a distinct, non-zero outcome — scripting must be able to
        // tell "re-armed" from "there was nothing to re-arm".
        anyhow::bail!("{wt_s} is not in the merge queue.");
    }
    outln!("Re-queued for the next drain.");
    Ok(())
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
    // Scope to the current repo (same membership rule as `drain`) so a shared
    // state DB holding several repos' queues doesn't leak other repos' rows into
    // this repo's listing — `merge` is per-repo, and `list` must match `drain`.
    let root = repo_root()?;
    let rows = rows_for_repo(&root)?;
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

/// The host control endpoint + merge-scoped token injected into a sprite at
/// provision time (the `route_to_host` remote_mode). `None` on a normal on-host
/// worktree, where enqueue stays local.
fn control_endpoint_from_env() -> Option<(String, String)> {
    let url = std::env::var("THEGN_CONTROL_URL")
        .ok()
        .filter(|s| !s.is_empty())?;
    let token = std::env::var("THEGN_CONTROL_TOKEN")
        .ok()
        .filter(|s| !s.is_empty())?;
    Some((url, token))
}

/// Enqueue on the host's daemon over the control plane. Sends the **host-canonical**
/// worktree path (`$THEGN_WORKTREE`) so the host resolves the branch in its own
/// store; the host's queue then owns the row and its drain bundle-fetches the
/// sprite's tip. A failed call surfaces the reason and does not fall back to a
/// local row.
fn add_via_host(url: &str, token: &str, worktrees: Vec<String>) -> Result<()> {
    use thegn_svc::control::client::{ControlAddr, ControlClient};
    let targets: Vec<String> = if worktrees.is_empty() {
        vec![
            std::env::var("THEGN_WORKTREE")
                .ok()
                .filter(|s| !s.is_empty())
                .context(
                    "route_to_host: $THEGN_WORKTREE is unset, so this worktree can't be \
                     identified to the host",
                )?,
        ]
    } else {
        worktrees
    };
    let client = ControlClient::new(ControlAddr::Tcp {
        addr: url.to_string(),
        token: token.to_string(),
    });
    let rt = tokio::runtime::Runtime::new().context("tokio runtime for route-to-host enqueue")?;
    for wt in &targets {
        match rt.block_on(client.merge_add(wt)) {
            Ok(_) => outln!("  + queued {wt} on host"),
            Err(e) => {
                outln!("  ✗ route-to-host enqueue failed for {wt}: {e}");
                return Err(e);
            }
        }
    }
    Ok(())
}

fn add(cfg: &Config, worktrees: Vec<String>, all: bool) -> Result<()> {
    add_quiet(cfg, worktrees, all, false)
}

/// `add`, with its human output suppressible so `drain --all --json` can enqueue
/// without printing prose ahead of the single JSON document.
fn add_quiet(cfg: &Config, worktrees: Vec<String>, all: bool, quiet: bool) -> Result<()> {
    // route_to_host: a provisioned sprite (host control endpoint + token in its
    // env) sends the enqueue to the host's daemon so the host's queue owns the
    // row. `--all` enumerates local branches, so it stays on the local path.
    if !all
        && cfg.merge_queue.remote_mode == thegn_core::config::MergeRemoteMode::RouteToHost
        && let Some((url, token)) = control_endpoint_from_env()
    {
        return add_via_host(&url, &token, worktrees);
    }
    let mq = &cfg.merge_queue;
    let db = Db::open()?;

    if all {
        // `--all` enumerates the CWD's repo, so it needs a repo root. An
        // explicit path argument does not — it resolves its own root — and
        // demanding one here made `merge add <path>` from outside a repo fail
        // with "not inside a git repository", pointing at the wrong thing.
        let root = repo_root()?;
        let target = integrate::resolve_target(mq, &root);
        let cands = integrate::candidate_branches(mq, &root, &target)?;
        for s in &cands.skipped_dirty {
            if !quiet {
                outln!(
                    "  • skipped {s} (dirty — set [merge_queue] snapshot_dirty = true to queue it)"
                );
            }
        }
        for (branch, wt) in &cands.worktrees {
            db.enqueue_merge(wt, branch, &target)?;
            crate::merge_lifecycle::apply(mq, &db, &root, wt, branch, LifecycleEvent::Enqueued);
            if !quiet {
                outln!("  + queued {branch}");
            }
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
        if !quiet {
            outln!("  {mark} {msg}");
        }
    }
    Ok(())
}

fn rm(cfg: &Config, worktree: Option<String>) -> Result<()> {
    // Same normalization the enqueue used, or the row won't be found.
    let wt = crate::merge_ops::canonical_worktree(&super::resolve_worktree(worktree));
    let wt_s = wt.to_string_lossy().to_string();
    let db = Db::open()?;
    // Check membership before deleting so "not queued" is a distinct, non-zero
    // outcome — otherwise `rm` reports success (exit 0) even when it removed
    // nothing, which scripting/CI can't distinguish from a real removal.
    let was_queued = db.list_merge_queue()?.iter().any(|r| r.worktree == wt_s);
    // Dequeue AND un-file from the lifecycle folder, so `rm` doesn't strand the
    // worktree in "Merging"/"Needs attention" (the sidebar/queue de-sync).
    crate::merge_ops::dequeue_worktree(&cfg.merge_queue, &db, &wt)?;
    if !was_queued {
        anyhow::bail!("{wt_s} was not in the queue.");
    }
    outln!("Removed from queue.");
    Ok(())
}

fn clear(cfg: &Config) -> Result<()> {
    let root = repo_root()?;
    let db = Db::open()?;
    let n = crate::merge_ops::clear_repo(&cfg.merge_queue, &db, &root)?;
    outln!("Queue cleared ({n} removed).");
    Ok(())
}

fn drain(cfg: &Config, all: bool, json: bool) -> Result<()> {
    let root = repo_root()?;
    let mq = &cfg.merge_queue;
    // `push` mode drains this clone locally and pushes to origin, so it skips the
    // remote-target guard; `route_to_host` keeps the fold on the target host.
    let push_mode = mq.remote_mode == thegn_core::config::MergeRemoteMode::Push;
    if !push_mode
        && let Ok(db) = Db::open()
        && let Some(msg) = crate::merge_ops::remote_target_guard(&db, &root)
    {
        // Guard refusal: bail so the exit code is non-zero for scripting/CI.
        anyhow::bail!("{msg}");
    }
    // `--json` means EXACTLY one document on stdout (see `cmd::emit_json`), so
    // every human line below is suppressed under it — including the enqueue
    // chatter from `--all`, the banner, per-branch progress, and the push
    // footer. Previously all of those printed regardless, so `--json` emitted a
    // stream of prose with one JSON object somewhere in the middle.
    if all {
        add_quiet(cfg, Vec::new(), true, json)?;
    }
    let target = integrate::resolve_target(mq, &root);
    let items: Vec<QueueItem> = rows_for_repo(&root)?
        .into_iter()
        // Only settled-good rows are excluded. `gate_failed`/`gate_error`/
        // `deferred`/`needs_human` are all retried by a plain drain — an
        // environment failure especially, since it may simply be fixed by now.
        .filter(|r| r.status != "landed" && r.status != "ready")
        .map(|r| QueueItem {
            worktree: r.worktree,
            branch: r.branch,
            location: r.location,
            agent_attempts: r.agent_attempts,
        })
        .collect();
    if items.is_empty() {
        // The empty path is the one a cron/CI loop hits most often, so it must
        // honour `--json` like every other path rather than printing prose.
        if json {
            return super::emit_json(&drain_json(&target, &merge_driver::DriveOutcome::default()));
        }
        outln!("Nothing to drain.");
        return Ok(());
    }
    if !json {
        outln!(
            "Draining {} branch(es) into {target}{}…",
            items.len(),
            match (mq.gate_on, mq.gate_command.is_empty()) {
                (true, false) => format!(" (gate: {})", mq.gate_command),
                // Say "ungated" out loud: an unintentionally ungated drain used
                // to look identical to a gated one (the suffix was just absent).
                _ => " (UNGATED — no gate_command)".to_string(),
            }
        );
    }

    let db = Db::open()?;
    // The run's effective target may differ from the one frozen on each row at
    // enqueue time (e.g. under `--set merge_queue.target_branch=…`), which is
    // why `merge list` used to keep showing the stale value. Re-stamp it.
    for it in &items {
        let _ = db.set_merge_target(&it.worktree, &target);
    }
    let out = merge_driver::drive_queue(mq, &root, &db, items, |step: &DriveStep| {
        if json {
            return;
        }
        // Only the settled transitions are worth a CLI line; folding/agent_running
        // are transient and would just be noise before the outcome.
        match step.status {
            "landed" => outln!("  ✓ landed {} ({})", step.branch, step.detail),
            "ready" => outln!("  ◆ ready  {} ({})", step.branch, step.detail),
            "deferred" | "gate_failed" => {
                outln!("  ✗ {} deferred — {}", step.branch, first_line(step.detail))
            }
            // Not a verdict about the branch — worded so it can't be misread.
            "gate_error" => outln!(
                "  ! {} was NOT gated — {}",
                step.branch,
                first_line(step.detail)
            ),
            "needs_human" => outln!(
                "  ⚑ {} needs a human — {}",
                step.branch,
                first_line(step.detail)
            ),
            "agent_running" => outln!("  … {} — {}", step.branch, step.detail),
            _ => {}
        }
    });

    // push mode: converge by pushing the advanced target to origin. Done before
    // emitting JSON so its result can ride inside the single document.
    let mut push_err: Option<anyhow::Error> = None;
    let mut pushed = false;
    if push_mode && !out.landed.is_empty() {
        match crate::merge_ops::push_target(&root, &target) {
            Ok(()) => {
                pushed = true;
                if !json {
                    outln!("Pushed {target} to origin.");
                }
            }
            Err(e) => {
                if !json {
                    outln!("Push failed — {target} advanced locally but NOT on origin: {e}");
                }
                push_err = Some(e);
            }
        }
    }

    if json {
        let mut doc = drain_json(&target, &out);
        if push_mode {
            doc["pushed"] = serde_json::json!(pushed);
        }
        super::emit_json(&doc)?;
    } else {
        outln!(
            "Done: {} landed, {} ready, {} deferred, {} ungated, {} need a human.",
            out.landed.len(),
            out.ready.len(),
            out.deferred.len(),
            out.gate_error.len(),
            out.needs_human.len()
        );
        integrate::report_resyncs(&target, &out.resyncs);
        if !out.gate_error.is_empty() {
            outln!(
                "Note: {} branch(es) were never judged — the gate could not run. \
                 That is an environment failure, not a verdict about the code.",
                out.gate_error.len()
            );
        }
    }
    if let Some(e) = push_err {
        return Err(e);
    }
    Ok(())
}

/// The `drain --json` document. One shape for every exit path, including the
/// empty queue — scripts must not have to special-case the common no-op.
fn drain_json(target: &str, out: &merge_driver::DriveOutcome) -> serde_json::Value {
    serde_json::json!({
        "target": target,
        "landed": out.landed,
        "ready": out.ready,
        "deferred": out.deferred,
        // The gate could not RUN for these — reported apart from `deferred` so a
        // script never reads "the branch is bad" out of "the gate is missing".
        "gate_error": out.gate_error,
        "needs_human": out.needs_human,
        // Live checkouts of the target the fold could not fast-forward. A script
        // that lands into a checked-out branch needs to know its working tree is
        // now stale — silence here is what made this dangerous.
        "stale_checkouts": out
            .resyncs
            .iter()
            .filter(|r| !matches!(r.outcome, thegn_core::util::ResyncOutcome::Healed))
            .map(|r| {
                serde_json::json!({
                    "path": r.path.to_string_lossy(),
                    "fix": r.manual_fix(),
                })
            })
            .collect::<Vec<_>>(),
        "counts": {
            "landed": out.landed.len(),
            "ready": out.ready.len(),
            "deferred": out.deferred.len(),
            "gate_error": out.gate_error.len(),
            "needs_human": out.needs_human.len(),
        },
    })
}

/// First line of a multi-line status detail (the rest is the retained gate log).
fn first_line(detail: &str) -> &str {
    detail.lines().next().unwrap_or(detail)
}

fn land(cfg: &Config, worktree: Option<String>) -> Result<()> {
    let wt = crate::merge_ops::canonical_worktree(&super::resolve_worktree(worktree));
    let wt_s = wt.to_string_lossy().to_string();
    if let Ok(db) = Db::open()
        && let Some(root) = integrate::main_checkout(&wt)
        && let Some(msg) = crate::merge_ops::remote_target_guard(&db, &root)
    {
        // Guard refusal: bail so the exit code is non-zero for scripting/CI.
        anyhow::bail!("{msg}");
    }
    // Share the fold/gate/CAS core with `thegn land`; this queue-aware path
    // additionally records the outcome on the worktree's merge-queue row.
    let (branch, target, outcome) = super::land::land_branch(cfg, &wt)?;
    let db = Db::open()?;
    // Apply the sidebar-folder lifecycle for this worktree once we know its fate.
    let lifecycle = |event: LifecycleEvent| {
        if let Some(root) = integrate::main_checkout(&wt) {
            crate::merge_lifecycle::apply(&cfg.merge_queue, &db, &root, &wt_s, &branch, event);
        }
    };
    // A failed land still records its fate (DB + lifecycle) below, but must exit
    // non-zero afterward so scripting/CI sees the failure rather than a clean 0.
    let mut failure: Option<String> = None;
    match outcome {
        AttemptOutcome::Landed { commit, resyncs } => {
            let _ = db.update_merge_status(&wt_s, "landed", Some(&commit), None, None);
            lifecycle(LifecycleEvent::Landed);
            outln!("✓ landed {branch} → {}", &commit[..commit.len().min(12)]);
            integrate::report_resyncs(&target, &resyncs);
        }
        AttemptOutcome::UpToDate => {
            let _ = db.update_merge_status(&wt_s, "landed", None, Some("already merged"), None);
            lifecycle(LifecycleEvent::Landed);
            outln!("{branch} already merged.");
        }
        AttemptOutcome::Conflict { paths } => {
            lifecycle(LifecycleEvent::Failed);
            outln!("✗ {branch} conflicts: {}", paths.join(", "));
            failure = Some(format!("land failed: {branch} conflicts"));
        }
        AttemptOutcome::GateFailed { .. } => {
            lifecycle(LifecycleEvent::Failed);
            outln!("✗ {branch} breaks the build (gate red).");
            failure = Some(format!("land failed: {branch} gate red"));
        }
        AttemptOutcome::GateError { reason, log } => {
            // The gate never ran: record it as an environment failure, not as a
            // verdict about the branch, and keep the log so the row can say why.
            let detail = if log.trim().is_empty() {
                reason.clone()
            } else {
                format!("{reason}\n{}", log.trim())
            };
            let _ = db.update_merge_status(&wt_s, "gate_error", None, None, Some(&detail));
            lifecycle(LifecycleEvent::Failed);
            outln!("✗ {branch} was NOT gated — {reason}.");
            outln!("  The branch was not judged; fix the gate environment and re-run.");
            failure = Some(format!("land failed: {branch} gate could not run"));
        }
        AttemptOutcome::Unreachable { detail } => {
            let _ = db.update_merge_status(&wt_s, "deferred", None, Some(&detail), None);
            lifecycle(LifecycleEvent::Failed);
            outln!("✗ {branch}: {detail}");
            failure = Some(format!("land failed: {branch} unreachable"));
        }
        AttemptOutcome::Ready { .. } => {
            // Unreachable with auto_land forced on, but handle for completeness.
            outln!("{branch} is ready but was not landed.");
            failure = Some(format!("{branch} is ready but was not landed"));
        }
    }
    if let Some(msg) = failure {
        // Detail already printed above (✗ line); bail with a terse, distinct
        // reason only to exit non-zero for scripting/CI — no double-print.
        anyhow::bail!("{msg}");
    }
    Ok(())
}
