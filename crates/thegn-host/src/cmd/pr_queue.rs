//! `thegn pr queue` — the PR queue namespace.
//!
//! Queue pull requests (`add`) and shepherd them (`drain`): each queued PR is
//! refreshed from the forge, classified, and — where configured — handed to an
//! agent in its own worktree. A PR that satisfies every gate is merged by the
//! forge's own auto-merge (or directly, if you asked for that).
//!
//! The team-mode counterpart to `thegn merge`, and deliberately the same shape.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use thegn_core::config::Config;
use thegn_core::db::{Db, PrQueueRow};
use thegn_core::outln;
use thegn_core::store::WorktreeAuxStore;

use crate::integrate;
use crate::pr_driver::{self, PrItem, PrStep};

#[derive(clap::Subcommand, Clone)]
pub enum Action {
    /// Show the PR queue for this repo.
    List {
        /// Emit one JSON array instead of the human table.
        #[arg(long)]
        json: bool,
    },
    /// Queue a pull request.
    Add {
        /// PR number. Omit to use the pull request for the current worktree's
        /// branch — the common case from inside a checkout.
        #[arg(long)]
        pr: Option<u64>,
        /// The worktree the PR belongs to (default: the current one). A queued
        /// PR without a worktree is watched but cannot be agent-fixed.
        #[arg(long)]
        worktree: Option<String>,
    },
    /// Remove a pull request from the queue.
    Rm {
        /// PR number.
        number: u64,
    },
    /// Empty the queue for this repo.
    Clear,
    /// One refresh pass: classify every queued PR and act on it.
    Drain {
        /// Emit a JSON summary instead of the human log.
        #[arg(long)]
        json: bool,
    },
    /// Print the queue's current state as a one-line summary.
    Status {
        #[arg(long)]
        json: bool,
    },
}

pub fn run(cfg: &Config, action: Action) -> Result<()> {
    // `enabled` is checked against the REPO-resolved table, so
    // `[workspace.<slug>] pr_queue.enabled` can turn the queue on for one repo
    // and leave it off everywhere else. Without a repo root, fall back to the
    // global table rather than refusing outright.
    let enabled = match repo_root() {
        Ok(root) => cfg.repo_pr_queue(&root).enabled,
        Err(_) => cfg.pr_queue.enabled,
    };
    if !enabled {
        anyhow::bail!(
            "PR queue is disabled — set [pr_queue] enabled = true (or \
             [workspace.<slug>.pr_queue] enabled = true for just this repo)"
        );
    }
    match action {
        Action::List { json } => list(json),
        Action::Add { pr, worktree } => add(cfg, pr, worktree),
        Action::Rm { number } => rm(number),
        Action::Clear => clear(),
        Action::Drain { json } => drain(cfg, json),
        Action::Status { json } => status(json),
    }
}

/// The repo root (main checkout) reachable from the cwd.
fn repo_root() -> Result<PathBuf> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    integrate::main_checkout(&cwd).context("not inside a git repository")
}

fn rows(root: &Path) -> Result<Vec<PrQueueRow>> {
    let db = Db::open()?;
    Ok(pr_driver::rows_for_repo(&db, root))
}

fn list(json: bool) -> Result<()> {
    let root = repo_root()?;
    let rows = rows(&root)?;
    if json {
        return super::emit_json(&rows);
    }
    if rows.is_empty() {
        outln!("PR queue is empty.");
        return Ok(());
    }
    for r in &rows {
        let detail = r.detail.as_deref().unwrap_or("");
        let wt = match r.worktree.as_deref() {
            Some(w) if !w.is_empty() => format!(" ({w})"),
            _ => " (no worktree)".to_string(),
        };
        outln!(
            "#{:<6} {:<18} {}{}{}",
            r.number,
            r.status,
            r.branch,
            wt,
            if detail.is_empty() {
                String::new()
            } else {
                format!(" — {detail}")
            }
        );
    }
    Ok(())
}

fn add(cfg: &Config, pr: Option<u64>, worktree: Option<String>) -> Result<()> {
    let root = repo_root()?;
    let wt = super::resolve_worktree(worktree)
        .to_string_lossy()
        .into_owned();
    let loc = thegn_core::remote::GitLoc::from_db(&wt, None);

    // Resolve the PR: an explicit number, else whatever is open for this
    // worktree's branch. `pr_status_for` and `pr_status` share error mapping, so
    // both paths report a missing PR identically.
    let panel = match pr {
        Some(n) => thegn_core::github::pr_status_for(&loc, n),
        None => thegn_core::github::pr_status(&loc),
    };
    let status = match panel.state {
        thegn_core::github::PanelState::Pr(p) => *p,
        other => anyhow::bail!("could not resolve a pull request: {}", state_word(&other)),
    };

    let db = Db::open()?;
    db.enqueue_pr(
        &root.to_string_lossy(),
        status.number,
        Some(&wt),
        &status.head_ref_name,
        &status.base_ref_name,
        "github",
    )?;
    let _ = cfg;
    outln!(
        "Queued PR #{} ({} → {}).",
        status.number,
        status.head_ref_name,
        status.base_ref_name
    );
    Ok(())
}

fn state_word(s: &thegn_core::github::PanelState) -> String {
    use thegn_core::github::PanelState as P;
    match s {
        P::NoGh => "gh is not installed".into(),
        P::NotAuthenticated => "gh is not authenticated (run: gh auth login)".into(),
        P::NoPr => "no open pull request for this branch".into(),
        P::RateLimited => "the forge rate-limited us".into(),
        P::Offline => "the forge is unreachable".into(),
        P::Error { message } => message.clone(),
        P::Pr(_) => "ok".into(),
    }
}

fn rm(number: u64) -> Result<()> {
    let root = repo_root()?;
    let key = PrQueueRow::make_key(&root.to_string_lossy(), number);
    let db = Db::open()?;
    db.remove_pr_entry(&key)?;
    outln!("Removed PR #{number} from the queue.");
    Ok(())
}

fn clear() -> Result<()> {
    let root = repo_root()?;
    let db = Db::open()?;
    let n = db.clear_pr_queue(&root.to_string_lossy())?;
    outln!("PR queue cleared ({n} removed).");
    Ok(())
}

fn status(json: bool) -> Result<()> {
    let root = repo_root()?;
    let rows = rows(&root)?;
    let count = |s: &str| rows.iter().filter(|r| r.status == s).count();
    let blocked = rows
        .iter()
        .filter(|r| r.status.starts_with("blocked_"))
        .count();
    if json {
        return super::emit_json(&serde_json::json!({
            "total": rows.len(),
            "watching": count("watching"),
            "blocked": blocked,
            "agent_running": count("agent_running"),
            "ready": count("ready"),
            "needs_human": count("needs_human"),
        }));
    }
    outln!(
        "{} queued: {} watching, {} blocked, {} with an agent, {} ready, {} need a human.",
        rows.len(),
        count("watching"),
        blocked,
        count("agent_running"),
        count("ready"),
        count("needs_human")
    );
    Ok(())
}

fn drain(cfg: &Config, json: bool) -> Result<()> {
    let root = repo_root()?;
    let pq = cfg.repo_pr_queue(&root);
    let db = Db::open()?;

    let items: Vec<PrItem> = pr_driver::rows_for_repo(&db, &root)
        .iter()
        // Settled rows are done with — re-queue one explicitly to look again.
        .filter(|r| !matches!(r.status.as_str(), "merged" | "closed" | "needs_human"))
        .map(PrItem::from)
        .collect();

    if items.is_empty() {
        if json {
            return super::emit_json(&drain_json(&pr_driver::PrOutcome::default()));
        }
        outln!("Nothing to do — the PR queue has no active entries.");
        return Ok(());
    }

    if !json {
        outln!(
            "Refreshing {} pull request(s){}…",
            items.len(),
            match pq.merge_mode {
                thegn_core::config::PrMergeMode::AutoMerge => " (green ⇒ the forge's auto-merge)",
                thegn_core::config::PrMergeMode::Thegn => " (green ⇒ thegn merges)",
                thegn_core::config::PrMergeMode::Ready => " (green ⇒ held at ready)",
            }
        );
    }

    // Rows carry their own forge id, so a repo whose PRs live on more than one
    // forge still resolves correctly once a second provider exists.
    let forge_id = items
        .first()
        .map(|i| i.forge.clone())
        .unwrap_or_else(|| "github".to_string());
    let forge = thegn_svc::prq::for_id(&forge_id);
    let out = pr_driver::drive_queue(
        &pq,
        cfg,
        forge.as_ref(),
        &root,
        &db,
        items,
        |step: &PrStep| {
            if json {
                return;
            }
            // Only settled transitions are worth a line; `merging`/`agent_running`
            // are transient and would just be noise before the outcome.
            match step.status {
                "merged" | "ready" | "needs_human" | "closed" => {
                    let d = if step.detail.is_empty() {
                        String::new()
                    } else {
                        format!(" — {}", step.detail)
                    };
                    outln!("  #{} {}{}", step.number, step.status, d);
                }
                _ => {}
            }
        },
    );

    if json {
        return super::emit_json(&drain_json(&out));
    }
    for w in &out.warnings {
        outln!("Warning: {w}");
    }
    outln!(
        "Done: {} merged/armed, {} ready, {} still blocked, {} need a human, {} left the queue.",
        out.merged.len(),
        out.ready.len(),
        out.blocked.len(),
        out.needs_human.len(),
        out.dropped.len()
    );
    Ok(())
}

/// The `drain --json` document. One shape for every exit path, including the
/// empty queue — scripts must not have to special-case the common no-op.
fn drain_json(out: &pr_driver::PrOutcome) -> serde_json::Value {
    serde_json::json!({
        "merged": out.merged,
        "ready": out.ready,
        "blocked": out.blocked,
        "needs_human": out.needs_human,
        "dropped": out.dropped,
        // Non-fatal problems (an unreachable forge, an unresolvable agent), so a
        // scripted drain can tell "nothing to do" from "couldn't look".
        "warnings": out.warnings,
    })
}
