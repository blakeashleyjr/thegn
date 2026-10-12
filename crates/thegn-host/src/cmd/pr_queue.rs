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
    let root = repo_root().ok();
    // THE-515: report a refused trusted overlay as itself; read/cleanup verbs
    // keep working, the forge-driving ones stop.
    if let Some(root) = &root
        && let Some(refusal) = cfg.workspace_overlay_refusal(root)
    {
        if matches!(
            action,
            Action::List { .. } | Action::Rm { .. } | Action::Clear | Action::Status { .. }
        ) {
            thegn_core::msg::warn(&format!("{}: {refusal}", root.display()));
            if !cfg.pr_queue.enabled {
                anyhow::bail!("PR queue disabled — set [pr_queue] enabled = true");
            }
        } else {
            anyhow::bail!("{}: {refusal}", root.display());
        }
    } else if !root
        .as_ref()
        .map(|root| cfg.repo_pr_queue(root).enabled)
        .unwrap_or(cfg.pr_queue.enabled)
    {
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
    let root = repo_root()?
        .canonicalize()
        .context("canonicalize repo root")?;
    let explicit = worktree.is_some();
    let wt_path = super::resolve_worktree(worktree)
        .canonicalize()
        .context("canonicalize PR worktree")?;
    let wt = wt_path.to_string_lossy().into_owned();
    let loc = thegn_core::remote::GitLoc::from_db(&wt, None);

    // Resolve the PR: an explicit number, else whatever is open for this
    // worktree's branch.
    let forges = crate::forge_handle::get();
    let forge = forges.for_loc(&loc);
    let status = match forge.pr_status(&loc, thegn_core::forge::PrRef::from(pr)) {
        Ok(p) => p,
        Err(e) => anyhow::bail!("could not resolve a pull request: {}", e.describe()),
    };

    let wt_main = integrate::main_checkout(&wt_path);
    let wt_branch = loc
        .git_out(&["rev-parse", "--abbrev-ref", "HEAD"])
        .filter(|branch| !branch.is_empty());
    let bound_worktree = bind_checkout(
        &root,
        &wt_path,
        explicit,
        wt_main.as_deref(),
        wt_branch.as_deref(),
        &status.head_ref_name,
    )?;

    let db = Db::open()?;
    let bound_worktree = bound_worktree
        .as_ref()
        .map(|path| path.to_string_lossy().into_owned());
    db.enqueue_pr(
        &root.to_string_lossy(),
        status.number,
        bound_worktree.as_deref(),
        &status.head_ref_name,
        &status.base_ref_name,
        forge.id(),
    )?;
    let _ = cfg;
    outln!(
        "Queued PR #{} ({} → {}){}.",
        status.number,
        status.head_ref_name,
        status.base_ref_name,
        if bound_worktree.is_some() {
            String::new()
        } else {
            " (number-only; no matching checkout bound)".to_string()
        }
    );
    Ok(())
}

fn bind_checkout(
    root: &Path,
    wt: &Path,
    explicit: bool,
    wt_main: Option<&Path>,
    wt_branch: Option<&str>,
    pr_head: &str,
) -> Result<Option<PathBuf>> {
    let canonical_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let canonical_wt_main =
        wt_main.map(|path| path.canonicalize().unwrap_or_else(|_| path.to_path_buf()));
    if canonical_wt_main.as_deref() != Some(canonical_root.as_path()) {
        anyhow::bail!(
            "worktree {} belongs to another repository than {}",
            wt.display(),
            root.display()
        );
    }
    if wt_branch == Some(pr_head) {
        return Ok(Some(wt.to_path_buf()));
    }
    if explicit {
        anyhow::bail!(
            "worktree {} is on branch {}, but PR head is {}",
            wt.display(),
            wt_branch.unwrap_or("unknown"),
            pr_head
        );
    }
    Ok(None)
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

    // The repo's forge, by its origin host (rows also carry their forge id).
    let forges = crate::forge_handle::get();
    let loc = thegn_core::remote::GitLoc::from_db(&root.to_string_lossy(), None);
    let forge = forges.for_loc(&loc);
    let out = pr_driver::drive_queue(&pq, cfg, forge, &root, &db, items, |step: &PrStep| {
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
    });

    if json {
        return super::emit_json(&drain_json(&out));
    }
    for w in &out.warnings {
        outln!("Warning: {w}");
    }
    outln!(
        "Done: {} merged, {} auto-merge armed, {} ready, {} still blocked, {} need a human, {} left the queue.",
        out.merged.len(),
        out.auto_merge_armed.len(),
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
        "auto_merge_armed": out.auto_merge_armed,
        "ready": out.ready,
        "blocked": out.blocked,
        "needs_human": out.needs_human,
        "dropped": out.dropped,
        // Non-fatal problems (an unreachable forge, an unresolvable agent), so a
        // scripted drain can tell "nothing to do" from "couldn't look".
        "warnings": out.warnings,
    })
}

#[cfg(test)]
mod bind_checkout_tests {
    use super::*;

    #[test]
    fn binding_requires_repo_ownership_and_matches_branch_or_is_number_only() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        init_repo(&repo);
        let root = integrate::main_checkout(&repo).unwrap();
        let wt = repo.canonicalize().unwrap();
        let wt_main = integrate::main_checkout(&wt).unwrap();
        let cases = [
            ("matching", Some(root.as_path()), Some("feat"), false, true),
            (
                "explicit mismatch",
                Some(wt_main.as_path()),
                Some("other"),
                true,
                false,
            ),
            (
                "implicit mismatch",
                Some(wt_main.as_path()),
                Some("other"),
                false,
                true,
            ),
            (
                "different repository",
                Some(Path::new("/other")),
                Some("feat"),
                false,
                false,
            ),
        ];
        for (name, wt_main, branch, explicit, succeeds) in cases {
            let result = bind_checkout(&root, &wt, explicit, wt_main, branch, "feat");
            if succeeds && branch == Some("feat") {
                assert_eq!(result.unwrap().as_deref(), Some(wt.as_path()), "{name}");
            } else if succeeds {
                assert_eq!(
                    result.unwrap(),
                    None,
                    "{name}: implicit mismatch is number-only"
                );
            } else {
                let error = result.unwrap_err().to_string();
                assert!(error.contains(&wt.display().to_string()), "{name}: {error}");
                match name {
                    "explicit mismatch" => {
                        assert!(error.contains("other") && error.contains("feat"), "{error}");
                    }
                    "different repository" => assert!(error.contains("/other"), "{error}"),
                    _ => unreachable!("unexpected refusing case: {name}"),
                }
            }
        }
    }

    #[expect(clippy::disallowed_methods)]
    fn init_repo(path: &Path) {
        thegn_core::fsperm::restrict_dir_to_owner(path).unwrap();
        let status = thegn_core::util::git_cmd(path)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .args(["init"])
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn worktree_from_another_repository_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let repo_a = dir.path().join("a");
        let repo_b = dir.path().join("b");
        std::fs::create_dir(&repo_a).unwrap();
        std::fs::create_dir(&repo_b).unwrap();
        init_repo(&repo_a);
        init_repo(&repo_b);
        let root = integrate::main_checkout(&repo_a).unwrap();
        let wt = repo_b.canonicalize().unwrap();
        let wt_main = integrate::main_checkout(&wt).unwrap();
        assert!(
            bind_checkout(&root, &wt, true, Some(&wt_main), Some("feat"), "feat").is_err(),
            "a worktree from repo B cannot be stored under repo A's queue"
        );
    }

    #[test]
    fn equivalent_path_spellings_bind_to_the_same_repository() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        init_repo(&repo);
        let root = integrate::main_checkout(&repo).unwrap();
        let wt = repo.canonicalize().unwrap();
        let wt_main = integrate::main_checkout(&wt).unwrap();
        let alias_dir = root.parent().unwrap().join("alias");
        std::fs::create_dir(&alias_dir).unwrap();
        let alternate_root = alias_dir.join("..").join(root.file_name().unwrap());
        assert_ne!(root, alternate_root, "fixture uses distinct path spellings");
        assert_eq!(
            alternate_root.canonicalize().unwrap(),
            wt_main.canonicalize().unwrap()
        );

        assert_eq!(
            bind_checkout(
                &alternate_root,
                &wt,
                false,
                Some(&wt_main),
                Some("feat"),
                "feat"
            )
            .unwrap()
            .as_deref(),
            Some(wt.as_path()),
            "equivalent repository path spellings must remain bound"
        );
    }
}

#[cfg(test)]
mod drain_json_tests {
    use super::drain_json;
    use crate::pr_driver::PrOutcome;

    #[test]
    fn auto_merge_armed_is_additive_and_not_reported_as_merged() {
        let out = PrOutcome {
            auto_merge_armed: vec![42],
            ..PrOutcome::default()
        };
        let json = drain_json(&out);
        assert_eq!(json["merged"], serde_json::json!([]));
        assert_eq!(json["auto_merge_armed"], serde_json::json!([42]));
    }
}
