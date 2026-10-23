//! `thegn merge` — the agent-driven merge-queue namespace.
//!
//! Assign worktree branches to the queue (`add`) and drain them one at a time
//! (`drain`): each branch is folded onto the repo's target in the object DB, and
//! one that conflicts or fails the gate is handed to the configured headless CLI
//! agent (in the branch's own worktree) to rebase/resolve/fix, then re-attempted.
//! The batch, fold-everything-at-once path is still `thegn integrate`.

use anyhow::{Context, Result};
use rusqlite::ErrorCode;
use std::path::{Path, PathBuf};
use thegn_core::attention::MqStatus;
use thegn_core::config::Config;
use thegn_core::db::Db;
use thegn_core::outln;
use thegn_core::store::{MergeRetryOutcome, WorktreeAuxStore};

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
        /// Record an override for an unsatisfied pipeline gate.
        #[arg(long, requires = "why", conflicts_with = "all")]
        admin: bool,
        /// Reason recorded on the pipeline row for the override.
        #[arg(long, value_name = "REASON", requires = "admin")]
        why: Option<String>,
    },
    /// Remove a worktree from the queue.
    Rm {
        #[command(flatten)]
        target: super::target::WorktreeTarget,
    },
    /// Empty the queue for this repo.
    Clear,
    /// Remove merged worktrees whose grace period is up (`on_landed = "expire"`).
    ///
    /// Runs automatically at startup and after each land; this is the "do it now"
    /// gesture. A merged worktree you have gone back to and edited is never
    /// swept, with or without `--force`.
    Sweep {
        /// Collect every merged worktree now, ignoring the remaining grace period.
        #[arg(long)]
        force: bool,
        /// Emit a JSON report instead of the human log.
        #[arg(long)]
        json: bool,
    },
    /// Process the queue one branch at a time (the agent autopilot).
    Drain {
        /// Enqueue every eligible branch first, then drain.
        #[arg(long)]
        all: bool,
        /// Emit a JSON summary instead of the human log.
        #[arg(long)]
        json: bool,
    },
    /// Land only a ready merge-queue row; use `thegn land` outside the queue.
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
    /// Classify conflicts and print a reconcile draft that must be completed.
    ///
    /// Splits hunks into concurrent additions (the base had nothing) and
    /// restructures (both sides changed existing code). Both require a decision;
    /// an empty base alone never proves that keeping both is valid. Run it in the worktree
    /// with the conflicted merge already in progress.
    Conflicts {
        /// Issue label for the skeleton heading, e.g. `THE-32`.
        #[arg(long, default_value = "LANE")]
        issue: String,
        /// Print the counts only, not the skeleton.
        #[arg(long)]
        summary: bool,
        /// Emit JSON instead of the human output.
        #[arg(long)]
        json: bool,
        /// Resolve one item: `path:line=text`, or `path=text` for an unclassified file.
        #[arg(
            long,
            value_name = "KEY=TEXT",
            conflicts_with_all = ["summary", "json"]
        )]
        decision: Vec<String>,
    },
}

pub fn run(cfg: &Config, action: Action) -> Result<()> {
    // `enabled` is checked against the REPO-resolved table, so a
    // `[workspace.<slug>] merge_queue.enabled = false` can turn the queue off for
    // one repo. Resolving needs a repo root; when there isn't one (not inside a
    // repo) fall back to the global table so the refusal message below still
    // beats a confusing "not inside a git repository" from the subcommand.
    let root = repo_root().ok();
    // THE-515: a refused (ambiguous) trusted overlay is reported as itself,
    // not as "disabled". Read/cleanup verbs keep working so the queue can
    // still be inspected and emptied; everything that gates, folds or lands
    // stops here.
    if let Some(root) = &root
        && let Some(refusal) = cfg.workspace_overlay_refusal(root)
    {
        if matches!(
            action,
            Action::List { .. } | Action::Rm { .. } | Action::Clear | Action::Conflicts { .. }
        ) {
            thegn_core::msg::warn(&format!("{}: {refusal}", root.display()));
            if !cfg.merge_queue.enabled {
                anyhow::bail!("Merge queue disabled — set [merge_queue] enabled = true");
            }
        } else {
            anyhow::bail!("{}: {refusal}", root.display());
        }
    } else if !root
        .as_ref()
        .map(|root| cfg.repo_merge_queue(root).enabled)
        .unwrap_or(cfg.merge_queue.enabled)
    {
        // Refusal, not success: bail so the process exits non-zero — scripts/CI
        // must be able to tell "did nothing because disabled" from "did the work".
        anyhow::bail!(
            "Merge queue disabled. Set `[merge_queue]` `enabled = true` in your config to use it."
        );
    }
    match action {
        Action::List { json } => list(json),
        Action::Add {
            worktrees,
            all,
            admin,
            why,
        } => {
            let admin_why = if admin { why.as_deref() } else { None };
            add_with(cfg, worktrees, all, false, admin_why)
        }
        Action::Rm { target } => rm(cfg, target.get()),
        Action::Clear => clear(cfg),
        Action::Sweep { force, json } => sweep(cfg, force, json),
        Action::Drain { all, json } => drain(cfg, all, json),
        Action::Land { target } => land(cfg, target.get()),
        Action::Retry { target } => retry(target.get()),
        Action::Conflicts {
            issue,
            summary,
            json,
            decision,
        } => conflicts(&issue, summary, json, &decision),
    }
}

/// `merge conflicts` — the mechanical half of writing a reconcile chunk.
///
/// A reconcile chunk written as generic prose is worse than none: the one used
/// for THE-32 advised "default to keeping both sides", which was right for 9 of
/// its 34 hunks and wrong for 25. Classification is computable; the decisions
/// are not, so this prints a draft, labels unsupported conflict shapes, and
/// makes its non-dispatchable state explicit.
fn conflicts(issue: &str, summary: bool, json: bool, decision_args: &[String]) -> Result<()> {
    // The CURRENT worktree, not `repo_root()`: that resolves the main checkout,
    // and a reconcile is always in flight in the lane's own linked worktree.
    let cwd = std::env::current_dir().context("`merge conflicts` needs a working directory")?;
    let root = thegn_core::util::git_out(&cwd, &["rev-parse", "--show-toplevel"])
        .map(|s| PathBuf::from(s.trim()))
        .context("`merge conflicts` must run inside a git worktree")?;
    let paths = conflicted_paths(&root)?;
    if paths.is_empty() {
        outln!("no conflicted files — is a merge actually in progress here?");
        return Ok(());
    }
    let mut files = Vec::with_capacity(paths.len());
    for path in paths {
        let display = crate::platform::display_git_path(&path);
        let (hunks, inspection_error) = match std::fs::read(root.join(&path)) {
            Err(error) => (
                vec![],
                Some(format!("could not read working-tree file: {error}")),
            ),
            Ok(bytes) => match std::str::from_utf8(&bytes) {
                Err(_) => (
                    vec![],
                    Some("file is not UTF-8 (binary or encoded conflict)".into()),
                ),
                Ok(body) => {
                    let hunks = thegn_core::merge_classify::classify_file(body);
                    let marker_count = body
                        .lines()
                        .filter(|line| line.starts_with("<<<<<<<"))
                        .count();
                    if marker_count != hunks.len() {
                        (
                            hunks,
                            Some(
                                "one or more textual conflict markers are malformed or incomplete"
                                    .into(),
                            ),
                        )
                    } else if hunks.is_empty() {
                        (
                            hunks,
                            Some(
                                "no textual conflict markers (binary, rename, delete/modify, or submodule conflict)"
                                    .into(),
                            ),
                        )
                    } else {
                        (hunks, None)
                    }
                }
            },
        };
        files.push(thegn_core::merge_classify::FileConflicts {
            path: display,
            hunks,
            inspection_error,
        });
    }
    let concurrent_add: usize = files
        .iter()
        .map(thegn_core::merge_classify::FileConflicts::concurrent_add)
        .sum();
    let restructure: usize = files
        .iter()
        .map(thegn_core::merge_classify::FileConflicts::restructure)
        .sum();
    let unclassified = files
        .iter()
        .filter(|file| file.inspection_error.is_some())
        .count();
    if json {
        let rows: Vec<_> = files
            .iter()
            .map(|f| {
                serde_json::json!({
                    "path": f.path,
                    "concurrent_add": f.concurrent_add(),
                    "restructure": f.restructure(),
                    "inspection_error": f.inspection_error,
                    "hunks": f.hunks.iter().map(|h| serde_json::json!({
                        "line": h.line,
                        "class": h.class.as_str(),
                        "ours": h.ours_hint,
                        "theirs": h.theirs_hint,
                    })).collect::<Vec<_>>(),
                })
            })
            .collect();
        return super::emit_json(&serde_json::json!({
            "files": rows,
            "concurrent_add": concurrent_add,
            "restructure": restructure,
            "unclassified": unclassified,
        }));
    }
    if summary {
        for f in &files {
            outln!(
                "  {:>3} concurrent-add  {:>3} rewrite  {:>3} unclassified   {:?}",
                f.concurrent_add(),
                f.restructure(),
                usize::from(f.inspection_error.is_some()),
                f.path
            );
        }
        outln!(
            "\n{concurrent_add} concurrent additions, {restructure} rewrites, {unclassified} unclassified; all require review"
        );
        return Ok(());
    }
    let mut decisions = std::collections::BTreeMap::new();
    for arg in decision_args {
        let (key, text) = arg
            .split_once('=')
            .filter(|(key, text)| !key.is_empty() && !text.trim().is_empty())
            .with_context(|| format!("invalid --decision {arg:?}; expected KEY=TEXT"))?;
        decisions.insert(key.to_string(), text.to_string());
    }
    outln!(
        "{}",
        thegn_core::merge_classify::render_chunk(issue, &files, &decisions)
    );
    Ok(())
}

/// Ask Git for raw, NUL-delimited paths. Newline parsing and Git's display
/// quoting both corrupt valid filenames, particularly on Unix where paths are
/// arbitrary byte strings. This is the blocking seam for the synchronous
/// `thegn merge conflicts` CLI action; it is never called by the compositor.
#[expect(
    clippy::disallowed_methods,
    reason = "CLI-only merge-conflict inspection blocks synchronously on its Git query"
)]
fn conflicted_paths(root: &Path) -> Result<Vec<PathBuf>> {
    let output = thegn_core::util::git_cmd(root)
        .args(["diff", "--name-only", "--diff-filter=U", "-z"])
        .output()
        .context("could not ask Git for conflicted files")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("could not list conflicted files: {}", stderr.trim());
    }
    parse_conflicted_paths(&output.stdout)
}

fn parse_conflicted_paths(stdout: &[u8]) -> Result<Vec<PathBuf>> {
    stdout
        .split(|byte| *byte == 0)
        .filter(|bytes| !bytes.is_empty())
        .map(crate::platform::os_path_from_git_bytes)
        .collect()
}

/// `merge retry [worktree]` — re-arm a held row.
///
/// Only held rows may be explicitly retried; other states are reported as
/// refusals so a completed or active row cannot be reset accidentally.
fn retry(worktree: Option<String>) -> Result<()> {
    let wt = crate::merge_ops::canonical_worktree(&super::resolve_worktree(worktree));
    let wt_s = wt.to_string_lossy().to_string();
    let db = Db::open()?;
    match retry_message(&wt_s, &db.retry_held_merge_entry(&wt_s)?) {
        Ok(message) => outln!("{message}"),
        Err(message) => anyhow::bail!("{message}"),
    }
    Ok(())
}

fn retry_message(
    wt: &str,
    outcome: &MergeRetryOutcome,
) -> std::result::Result<&'static str, String> {
    match outcome {
        MergeRetryOutcome::Rearmed => Ok("Re-queued for the next drain."),
        MergeRetryOutcome::NotQueued => Err(format!("{wt} is not in the merge queue.")),
        MergeRetryOutcome::NotRetryable { status } => Err(format!(
            "cannot retry a {status} row: retry re-arms deferred, gate_failed, gate_error, agent_blocked or needs_human rows"
        )),
        MergeRetryOutcome::Claimed => Err(format!(
            "{wt} is being worked by a drain; retry it after the drain finishes"
        )),
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
    merge_driver::rows_for_repo(&db, root)
}

pub(crate) fn queue_error_kind(err: &anyhow::Error) -> (&'static str, bool) {
    for cause in err.chain() {
        if let Some(rusqlite::Error::SqliteFailure(code, _)) =
            cause.downcast_ref::<rusqlite::Error>()
        {
            match code.code {
                ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked => {
                    return ("queue_busy", true);
                }
                ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase => {
                    return ("queue_corrupt", false);
                }
                _ => {}
            }
        }
        if cause.is::<thegn_core::db::MigrationAccessError>() {
            return ("queue_incompatible", false);
        }
    }
    ("queue_unavailable", false)
}

fn queue_error_hint(err: &anyhow::Error) -> &'static str {
    match queue_error_kind(err).0 {
        "queue_busy" => "database busy, retry",
        "queue_corrupt" => "database corrupt or not a SQLite database",
        "queue_incompatible" => "database incompatible: upgrade thegn",
        _ => "database unavailable",
    }
}

fn queue_error_context(err: anyhow::Error) -> anyhow::Error {
    let hint = queue_error_hint(&err);
    err.context(format!("merge queue unavailable ({hint})"))
}

fn queue_error_json(err: &anyhow::Error) -> serde_json::Value {
    let (kind, retryable) = queue_error_kind(err);
    serde_json::json!({
        "error": {
            "kind": kind,
            "message": err.to_string(),
            "retryable": retryable,
        }
    })
}

#[derive(serde::Serialize)]
struct ListedRow<'a> {
    #[serde(flatten)]
    row: &'a thegn_core::db::MergeQueueRow,
    claim: Option<ClaimJson>,
}

#[derive(serde::Serialize, Debug, Clone, PartialEq, Eq)]
struct ClaimJson {
    phase: String,
    generation: i64,
    lease_until_ms: Option<i64>,
    owner_hint: Option<String>,
    live: bool,
}

fn claim_json(view: Option<&thegn_core::store::MergeClaimView>, now_ms: i64) -> Option<ClaimJson> {
    let view = view.filter(|view| view.phase != "unclaimed")?;
    Some(ClaimJson {
        phase: view.phase.clone(),
        generation: view.generation,
        lease_until_ms: view.lease_until_ms,
        owner_hint: view.owner_hint.clone(),
        live: view
            .lease_until_ms
            .is_some_and(|lease_until_ms| lease_until_ms > now_ms),
    })
}

fn claim_suffix(claim: Option<&ClaimJson>, now_ms: i64) -> String {
    let Some(claim) = claim else {
        return String::new();
    };
    let owner = claim.owner_hint.as_deref().unwrap_or("unknown owner");
    if claim.live {
        let seconds = claim
            .lease_until_ms
            .map(|until| until.saturating_sub(now_ms) / 1000)
            .unwrap_or(0);
        format!(
            "  [claimed by {owner}, {}, lease {seconds}s left]",
            claim.phase
        )
    } else if let Some(until) = claim.lease_until_ms {
        let seconds = now_ms.saturating_sub(until) / 1000;
        format!("  [claim by {owner} expired {seconds}s ago, recoverable]")
    } else {
        format!("  [claim by {owner}, {}, lease unavailable]", claim.phase)
    }
}

fn list(json: bool) -> Result<()> {
    // Scope to the current repo (same membership rule as `drain`) so a shared
    // state DB holding several repos' queues doesn't leak other repos' rows into
    // this repo's listing — `merge` is per-repo, and `list` must match `drain`.
    let root = repo_root()?;
    let db = match Db::open() {
        Ok(db) => db,
        Err(error) if json => {
            super::emit_json(&queue_error_json(&error))?;
            return Err(error.context("merge queue unreadable"));
        }
        Err(error) => return Err(queue_error_context(error)),
    };
    let rows = match merge_driver::rows_for_repo(&db, &root) {
        Ok(rows) => rows,
        Err(error) if json => {
            super::emit_json(&queue_error_json(&error))?;
            return Err(error.context("merge queue unreadable"));
        }
        Err(error) => return Err(queue_error_context(error)),
    };
    let now_ms = thegn_core::util::now_ms();
    let mut listed = Vec::with_capacity(rows.len());
    for row in &rows {
        let view = match db.observe_merge_claim(&row.worktree) {
            Ok(view) => view,
            Err(error) if json => {
                super::emit_json(&queue_error_json(&error))?;
                return Err(error.context("merge queue unreadable"));
            }
            Err(error) => return Err(queue_error_context(error)),
        };
        listed.push(ListedRow {
            row,
            claim: claim_json(view.as_ref(), now_ms),
        });
    }
    if json {
        return super::emit_json(&listed);
    }
    if rows.is_empty() {
        outln!("Merge queue empty.");
        return Ok(());
    }
    for (r, listed_row) in rows.iter().zip(&listed) {
        let detail_text = if let Some(paths) = r.conflict_paths.as_deref() {
            Some(paths.to_string())
        } else {
            r.error_detail.as_deref().map(|detail| {
                thegn_core::merge_sweep::describe_reserved_detail(detail, thegn_core::util::now())
                    .unwrap_or_else(|| detail.to_string())
            })
        };
        let detail = detail_text
            .map(|d| format!("  — {}", d.replace('\n', ", ")))
            .unwrap_or_default();
        let claim = claim_suffix(listed_row.claim.as_ref(), now_ms);
        outln!(
            "  {} {} → {}{detail}{claim}",
            r.status,
            r.branch,
            r.target_branch
        );
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
    let host_worktree = std::env::var("THEGN_WORKTREE")
        .ok()
        .filter(|s| !s.is_empty())
        .context(
            "route_to_host: $THEGN_WORKTREE is unset, so this worktree can't be identified to the host",
        )?;
    anyhow::ensure!(
        worktrees.is_empty() || (worktrees.len() == 1 && worktrees.first() == Some(&host_worktree)),
        "route_to_host accepts only this provisioned worktree ({host_worktree}); omit the path argument"
    );
    let targets = [host_worktree];
    let client = ControlClient::new(ControlAddr::HttpOrigin {
        origin: url.to_string(),
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

/// Select the authority model from the repo-resolved config whenever the
/// in-sandbox clone can identify its repository. Provisioning uses that same
/// layer; consulting only the raw global table here would invert a trusted
/// `[workspace.<slug>.merge_queue] remote_mode` override. If the current
/// directory no longer resolves but a provisioned return credential exists,
/// the credential is the fail-closed evidence that provisioning selected the
/// host authority model.
fn effective_remote_mode(
    cfg: &Config,
    current_repo: Option<&Path>,
    has_return_credential: bool,
) -> thegn_core::config::MergeRemoteMode {
    current_repo.map_or_else(
        || {
            if has_return_credential {
                thegn_core::config::MergeRemoteMode::RouteToHost
            } else {
                cfg.merge_queue.remote_mode
            }
        },
        |root| cfg.repo_merge_queue(root).remote_mode,
    )
}

/// `add`, with its human output suppressible so `drain --all --json` can enqueue
/// without printing prose ahead of the single JSON document.
fn add_quiet(cfg: &Config, worktrees: Vec<String>, all: bool, quiet: bool) -> Result<()> {
    add_with(cfg, worktrees, all, quiet, None)
}

fn add_with(
    cfg: &Config,
    worktrees: Vec<String>,
    all: bool,
    quiet: bool,
    admin_why: Option<&str>,
) -> Result<()> {
    // route_to_host: a provisioned sprite (host control endpoint + token in its
    // env) sends the enqueue to the host's daemon so the host's queue owns the
    // row. `--all` enumerates local branches, so it stays on the local path.
    if !all {
        let endpoint = control_endpoint_from_env();
        let current_repo = repo_root().ok();
        let mode = effective_remote_mode(cfg, current_repo.as_deref(), endpoint.is_some());
        if mode == thegn_core::config::MergeRemoteMode::RouteToHost
            && let Some((url, token)) = endpoint
        {
            return add_via_host(&url, &token, worktrees);
        }
        if mode == thegn_core::config::MergeRemoteMode::RouteToHost
            && std::env::var("THEGN_REMOTE_ENV").as_deref() == Ok("1")
        {
            anyhow::bail!(
                "route_to_host credential is missing: reprovision this environment after starting the secure host control endpoint; no local queue row was created"
            );
        }
    }
    let db = Db::open()?;

    if all {
        // `--all` enumerates the CWD's repo, so it needs a repo root. An
        // explicit path argument does not — it resolves its own root — and
        // demanding one here made `merge add <path>` from outside a repo fail
        // with "not inside a git repository", pointing at the wrong thing.
        let root = repo_root()?;
        let mq = &cfg.repo_merge_queue(&root);
        let target = integrate::resolve_target(mq, &root);
        let cands = integrate::candidate_branches(mq, &root, &target)?;
        for s in &cands.skipped_dirty {
            if !quiet {
                outln!(
                    "  • skipped {s} (dirty — set [merge_queue] snapshot_dirty = true to queue it)"
                );
            }
        }
        let mut refused = 0usize;
        let mut refusal_details = Vec::new();
        for (branch, wt) in &cands.worktrees {
            match crate::merge_ops::enqueue_bound_local_with(&db, wt, branch, &target, admin_why) {
                Ok(()) => {
                    crate::merge_lifecycle::apply(
                        mq,
                        &db,
                        &root,
                        wt,
                        branch,
                        LifecycleEvent::Enqueued,
                    );
                    if !quiet {
                        outln!("  + queued {branch}");
                    }
                }
                Err(error)
                    if error
                        .downcast_ref::<thegn_core::pipeline_merge_admission::PipelineMergeRefused>(
                        )
                        .is_some() =>
                {
                    refused += 1;
                    refusal_details.push(format!("{branch}: {error}"));
                    if !quiet {
                        outln!("  ! refused {branch}: {error}");
                    }
                }
                Err(error) => return Err(error),
            }
        }
        if refused > 0 {
            return Err(crate::merge_ops::PipelineWorktreeRefusals {
                details: refusal_details,
            }
            .into());
        }
        return Ok(());
    }

    let paths = if worktrees.is_empty() {
        vec![super::resolve_worktree(None)]
    } else {
        worktrees.iter().map(PathBuf::from).collect()
    };
    let mut refusal_details = Vec::new();
    for wt in paths {
        match crate::merge_ops::enqueue_worktree_with(cfg, &db, &wt, admin_why) {
            Ok(msg) => {
                let mark = if msg.starts_with("skipped") {
                    "•"
                } else {
                    "+"
                };
                if !quiet {
                    outln!("  {mark} {msg}");
                }
            }
            Err(error)
                if error
                    .downcast_ref::<thegn_core::pipeline_merge_admission::PipelineMergeRefused>()
                    .is_some() =>
            {
                let wt_name = wt.to_string_lossy();
                refusal_details.push(format!("{wt_name}: {error}"));
                if !quiet {
                    outln!("  ! refused {wt_name}: {error}");
                }
            }
            Err(error) => return Err(error),
        }
    }
    if !refusal_details.is_empty() {
        return Err(crate::merge_ops::PipelineWorktreeRefusals {
            details: refusal_details,
        }
        .into());
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
    if let Err(error) = crate::merge_ops::dequeue_worktree(cfg, &db, &wt) {
        if error
            .chain()
            .any(|cause| cause.is::<thegn_core::store::MergeClaimActive>())
        {
            anyhow::bail!("{}", rm_claimed_message(&wt_s));
        }
        return Err(error);
    }
    if !was_queued {
        anyhow::bail!("{wt_s} was not in the queue.");
    }
    outln!("Removed from queue.");
    Ok(())
}

fn clear(cfg: &Config) -> Result<()> {
    let root = repo_root()?;
    let db = Db::open().map_err(queue_error_context)?;
    let n = crate::merge_ops::clear_repo(cfg, &db, &root).map_err(clear_error_context)?;
    outln!("Queue cleared ({n} removed).");
    Ok(())
}

fn clear_error_context(err: anyhow::Error) -> anyhow::Error {
    if err
        .downcast_ref::<crate::merge_ops::ClearKeptClaimed>()
        .is_some()
    {
        err
    } else {
        queue_error_context(err)
    }
}

fn rm_claimed_message(wt: &str) -> String {
    format!("{wt} is being worked by a drain; remove it after the drain finishes")
}

/// `merge sweep [--force] [--json]` — collect merged worktrees past their grace period.
fn sweep(cfg: &Config, force: bool, json: bool) -> Result<()> {
    let root = repo_root()?;
    let mq = cfg.repo_merge_queue(&root);
    // Say why nothing happened rather than printing a bare zero: under any other
    // `on_landed` there is no grace period for a sweep to end, and a silent no-op
    // reads as "there was nothing merged", which is a different fact.
    if mq.on_landed != thegn_core::config::OnLanded::Expire {
        if json {
            let mut doc =
                crate::merge_sweep::sweep_json(&crate::merge_sweep::SweepReport::default());
            doc["skipped"] = serde_json::json!({
                "reason": "on_landed",
                "on_landed": mq.on_landed.as_str(),
            });
            return super::emit_json(&doc);
        }
        outln!(
            "Nothing to sweep: [merge_queue] on_landed = \"{}\", so merged worktrees are not held for a grace period.",
            mq.on_landed.as_str()
        );
        return Ok(());
    }
    let report = crate::merge_sweep::sweep(cfg, &root, force);
    if json {
        super::emit_json(&crate::merge_sweep::sweep_json(&report))?;
    } else {
        use crate::merge_sweep::safe_display;
        for b in &report.collected {
            let discarded_build = report
                .discarded_build_state
                .iter()
                .any(|discarded| discarded == b);
            let discarded_tool = report
                .discarded_tool_state
                .iter()
                .any(|discarded| discarded == b);
            let reason = match (discarded_build, discarded_tool) {
                (true, true) => " (discarded build and seeded tool state)",
                (true, false) => " (discarded build state)",
                (false, true) => " (discarded seeded tool state)",
                (false, false) => "",
            };
            outln!("  ⌫ swept {}{}", safe_display(b), reason);
        }
        for (branch, errors) in &report.collected_partial {
            outln!(
                "  ⌫ swept {} (incomplete: {})",
                safe_display(branch),
                safe_display(&errors.join("; "))
            );
        }
        for branch in &report.held_branches {
            outln!(
                "  • worktree previously collected; branch {} and queue evidence retained for explicit cleanup (THE-596)",
                safe_display(branch)
            );
        }
        for (branch, reason) in &report.informational {
            outln!(
                "  • left {} untouched — {}",
                safe_display(branch),
                safe_display(reason)
            );
        }
        for b in &report.kept_dirty {
            outln!("  • kept {} — edited since landing", safe_display(b));
        }
        for b in &report.kept_changed {
            outln!("  • kept {} — changed during cleanup", safe_display(b));
        }
        for kept in &report.kept {
            outln!(
                "  • kept {} — {}",
                safe_display(&kept.branch),
                safe_display(&kept.reason)
            );
        }
        for error in &report.bookkeeping_errors {
            outln!("  ! cleanup bookkeeping: {}", safe_display(error));
        }
        if report.is_empty() {
            outln!("Nothing to sweep.");
        } else {
            outln!(
                "Swept {} merged worktree(s).",
                report.collected.len() + report.collected_partial.len()
            );
        }
    }
    if report.is_incomplete() {
        anyhow::bail!(
            "merge sweep incomplete: {} refused, {} changed during cleanup, {} partial, {} bookkeeping error(s)",
            report.kept.len(),
            report.kept_changed.len(),
            report.collected_partial.len(),
            report.bookkeeping_errors.len()
        );
    }
    Ok(())
}

fn drain(cfg: &Config, all: bool, json: bool) -> Result<()> {
    let root = repo_root()?;
    let mq = &cfg.repo_merge_queue(&root);
    // `push` mode drains this clone locally and pushes to origin, so it skips the
    // remote-target guard; `route_to_host` keeps the fold on the target host.
    let push_mode = mq.remote_mode == thegn_core::config::MergeRemoteMode::Push;
    if !push_mode {
        let db = match Db::open().context("remote-target guard database unavailable") {
            Ok(db) => db,
            Err(error) if json => {
                super::emit_json(&queue_error_json(&error))?;
                return Err(error);
            }
            Err(error) => return Err(queue_error_context(error)),
        };
        let guard = match crate::merge_ops::remote_target_guard(&db, &root) {
            Ok(guard) => guard,
            Err(error) if json => {
                super::emit_json(&queue_error_json(&error))?;
                return Err(error);
            }
            Err(error) => return Err(queue_error_context(error)),
        };
        if let Some(msg) = guard {
            // Guard refusal: bail so the exit code is non-zero for scripting/CI.
            anyhow::bail!("{msg}");
        }
    }
    // `--json` means EXACTLY one document on stdout (see `cmd::emit_json`), so
    // every human line below is suppressed under it — including the enqueue
    // chatter from `--all`, the banner, per-branch progress, and the push
    // footer. Previously all of those printed regardless, so `--json` emitted a
    // stream of prose with one JSON object somewhere in the middle.
    let mut add_refusal_warning = None;
    if all {
        add_refusal_warning =
            crate::merge_ops::drain_add_result(add_quiet(cfg, Vec::new(), true, json))?;
    }
    let had_add_refusals = add_refusal_warning.is_some();
    let target = integrate::resolve_target(mq, &root);
    let rows = match rows_for_repo(&root) {
        Ok(rows) => rows,
        Err(error) if json => {
            super::emit_json(&queue_error_json(&error))?;
            return Err(error.context("merge queue unreadable"));
        }
        Err(error) => return Err(queue_error_context(error)),
    };
    let items: Vec<QueueItem> = rows
        .into_iter()
        // Only settled-good rows are excluded. `gate_failed`/`gate_error`/
        // `deferred`/`needs_human` are all retried by a plain drain — an
        // environment failure especially, since it may simply be fixed by now.
        .filter(|r| r.status != "landed" && r.status != "ready")
        .map(|r| QueueItem {
            worktree: r.worktree,
            branch: r.branch,
            location: r.location,
        })
        .collect();
    if items.is_empty() {
        // The empty path is the one a cron/CI loop hits most often, so it must
        // honour `--json` like every other path rather than printing prose.
        if let Some(warning) = add_refusal_warning {
            if json {
                let mut out = merge_driver::DriveOutcome::default();
                out.warnings.push(warning);
                super::emit_json(&drain_json(&target, &out))?;
            } else {
                outln!("Warning: {warning}");
                outln!("Nothing to drain.");
            }
            anyhow::bail!("merge drain completed with pipeline worktree refusals");
        }
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
    let mut held_branches = Vec::new();
    let mut drain_items = Vec::with_capacity(items.len());
    for it in items {
        if let Err(error) = db.set_merge_target(&it.worktree, &target) {
            let active_error = error
                .chain()
                .any(|cause| cause.is::<thegn_core::store::MergeClaimActive>());
            let observed = if active_error {
                Some(db.observe_merge_claim(&it.worktree)?)
            } else {
                None
            };
            match merge_driver::target_stamp_action(
                active_error,
                observed.as_ref().and_then(Option::as_ref),
                thegn_core::util::now_ms(),
            ) {
                merge_driver::TargetStampAction::SkipHeld => {
                    held_branches.push(it.branch.clone());
                    continue;
                }
                merge_driver::TargetStampAction::RetryWithoutStamp => {
                    drain_items.push(it);
                    continue;
                }
                merge_driver::TargetStampAction::Fail => {}
            }
            if json {
                super::emit_json(&serde_json::json!({
                    "error": {
                        "kind": "queue_persistence_failed",
                        "worktree": it.worktree,
                        "message": error.to_string()
                    }
                }))?;
            }
            return Err(error);
        }
        drain_items.push(it);
    }
    let mut steps = Vec::new();
    let guard = crate::platform::InterruptGuard::install();
    let _scope = crate::fold_cancel::scope(move || guard.fired());
    let out = merge_driver::drive_queue(mq, cfg, &root, &db, drain_items, |step: &DriveStep| {
        if !crate::merge_notify::collect_step(&mut steps, step, json) {
            return;
        }
        // Only the settled transitions are worth a CLI line; folding/agent_running
        // are transient and would just be noise before the outcome.
        match MqStatus::parse(step.status) {
            Some(status) => {
                if let Some(line) = progress_line(status, step.branch, step.detail) {
                    outln!("{line}");
                }
            }
            None => outln!("  ? {} unknown queue status '{}'", step.branch, step.status),
        }
    });
    let mut out = out;
    out.warnings.extend(
        held_branches
            .into_iter()
            .map(|branch| format!("{branch}: claimed by another drain; target update skipped")),
    );
    if let Some(warning) = add_refusal_warning {
        out.warnings.push(warning);
    }
    crate::merge_notify::record_cli_drain(cfg, &db, &steps, &out);

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
        // Before the tally: a configured-but-unresolvable agent silently turns
        // the drain into "defer everything", which otherwise looks like a queue
        // that simply had nothing to fix.
        for w in &out.warnings {
            outln!("Warning: {w}");
        }
        for error in &out.persistence_errors {
            outln!("Queue state unavailable: {error}");
        }
        for failure in &out.bookkeeping_failed {
            outln!(
                "Ref advanced, bookkeeping failed: {} {} {} → {}. Verify the target ref and rerun `thegn merge drain` to reconcile.",
                failure.branch,
                failure.target,
                failure.old_oid,
                failure.new_oid
            );
        }
        outln!(
            "Done: {} landed, {} ready, {} deferred, {} agent-blocked, {} ungated, {} need a human.",
            out.landed.len(),
            out.ready.len(),
            out.deferred.len(),
            out.agent_blocked.len(),
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
        for verdict in &out.gate_verdicts {
            outln!(
                "  {}: {}/{} — {}",
                verdict.branch,
                verdict.phase,
                verdict.class,
                verdict.reason
            );
        }
    }
    if let Some(e) = push_err {
        return Err(e);
    }
    if !out.persistence_errors.is_empty() || !out.bookkeeping_failed.is_empty() {
        anyhow::bail!("merge drain partially completed; see the structured outcome above");
    }
    if had_add_refusals {
        anyhow::bail!("merge drain completed with pipeline worktree refusals");
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
        "agent_blocked": out.agent_blocked,
        // The gate could not RUN for these — reported apart from `deferred` so a
        // script never reads "the branch is bad" out of "the gate is missing".
        "gate_error": out.gate_error,
        "gate_verdicts": out.gate_verdicts.iter().map(|verdict| serde_json::json!({
            "branch": verdict.branch,
            "verdict": verdict.verdict,
            "phase": verdict.phase,
            "class": verdict.class,
            "reason": verdict.reason,
        })).collect::<Vec<_>>(),
        "needs_human": out.needs_human,
        // Non-fatal setup problems (e.g. `agent` naming no configured entry), so
        // a scripted drain can tell "nothing to fix" from "couldn't fix".
        "warnings": out.warnings,
        "persistence_errors": out.persistence_errors,
        "bookkeeping_failed": out.bookkeeping_failed.iter().map(|failure| serde_json::json!({
            "branch": failure.branch,
            "target": failure.target,
            "old_oid": failure.old_oid,
            "new_oid": failure.new_oid,
            "error": failure.error,
            "recovery": format!("verify {} at {}; rerun `thegn merge drain` to reconcile the queue row", failure.target, failure.new_oid),
        })).collect::<Vec<_>>(),
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
            "agent_blocked": out.agent_blocked.len(),
            "gate_error": out.gate_error.len(),
            "needs_human": out.needs_human.len(),
            "persistence_errors": out.persistence_errors.len(),
            "bookkeeping_failed": out.bookkeeping_failed.len(),
        },
    })
}

/// First line of a multi-line status detail (the rest is the retained gate log).
fn first_line(detail: &str) -> &str {
    detail.lines().next().unwrap_or(detail)
}

fn progress_line(status: MqStatus, branch: &str, detail: &str) -> Option<String> {
    Some(match status {
        MqStatus::Landed => format!("  ✓ landed {branch} ({detail})"),
        MqStatus::Ready => format!("  ◆ ready  {branch} ({detail})"),
        MqStatus::Deferred | MqStatus::GateFailed => {
            format!("  ✗ {branch} deferred — {}", first_line(detail))
        }
        // Not a verdict about the branch — worded so it can't be misread.
        MqStatus::GateError => {
            format!("  ! {branch} was NOT gated — {}", first_line(detail))
        }
        MqStatus::AgentBlocked => format!(
            "  ! {branch} agent dispatch blocked by environment/isolation policy — {}",
            first_line(detail)
        ),
        MqStatus::NeedsHuman => {
            format!("  ⚑ {branch} needs a human — {}", first_line(detail))
        }
        MqStatus::AgentRunning => format!("  … {branch} — {detail}"),
        MqStatus::Queued | MqStatus::Folding | MqStatus::Verifying => return None,
    })
}

fn land(cfg: &Config, worktree: Option<String>) -> Result<()> {
    let wt = crate::merge_ops::canonical_worktree(&super::resolve_worktree(worktree));
    let wt_s = wt.to_string_lossy().to_string();
    let root = integrate::main_checkout(&wt).context("not inside a git repository")?;
    let guard_db = Db::open().context("remote-target guard database unavailable")?;
    if let Some(msg) = crate::merge_ops::remote_target_guard(&guard_db, &root)? {
        // Guard refusal: bail so the exit code is non-zero for scripting/CI.
        anyhow::bail!("{msg}");
    }
    let admission = crate::merge_ops::ready_land_precheck(&guard_db, &wt_s)?;
    let guard = crate::platform::InterruptGuard::install();
    let _scope = crate::fold_cancel::scope(move || guard.fired());
    // Share the fold/gate/CAS core with `thegn land`; this queue-aware path
    // additionally records the outcome on the worktree's merge-queue row.
    let (branch, target, outcome) =
        super::land::land_branch_pinned(cfg, &wt, Some(&admission.candidate_oid))?;
    let db = Db::open()?;
    // Apply the sidebar-folder lifecycle for this worktree once we know its fate.
    let lifecycle = |event: LifecycleEvent| {
        if let Some(root) = integrate::main_checkout(&wt) {
            crate::merge_lifecycle::apply(
                &cfg.repo_merge_queue(&root),
                &db,
                &root,
                &wt_s,
                &branch,
                event,
            );
        }
    };
    // A failed land still records its fate (DB + lifecycle) below, but must exit
    // non-zero afterward so scripting/CI sees the failure rather than a clean 0.
    let mut failure: Option<String> = None;
    match outcome {
        AttemptOutcome::Landed {
            commit,
            old_oid,
            resyncs,
        } => {
            if let Err(error) = crate::merge_ops::record_ready_land(
                &db,
                &admission,
                &crate::merge_ops::ReadyLandRecord {
                    branch: &branch,
                    repo_root: &root,
                    target_branch: &target,
                    result_oid: &commit,
                    up_to_date: false,
                },
            ) {
                anyhow::bail!(
                    "ref advanced, bookkeeping failed: target {target}, old OID {old_oid}, new OID {commit}; verify the target ref and rerun `thegn merge drain` to reconcile. DB error: {error}"
                );
            }
            crate::merge_lifecycle::apply_landed(
                &cfg.repo_merge_queue(&root),
                &db,
                &root,
                &wt_s,
                &branch,
                &commit,
            );
            outln!("✓ landed {branch} → {}", &commit[..commit.len().min(12)]);
            integrate::report_resyncs(&target, &resyncs);
        }
        AttemptOutcome::UpToDate { commit } => {
            if let Err(error) = crate::merge_ops::record_ready_land(
                &db,
                &admission,
                &crate::merge_ops::ReadyLandRecord {
                    branch: &branch,
                    repo_root: &root,
                    target_branch: &target,
                    result_oid: &commit,
                    up_to_date: true,
                },
            ) {
                anyhow::bail!(
                    "ref advanced, bookkeeping failed: target {target}, old OID {commit}, new OID {commit}; verify the target ref and rerun `thegn merge drain` to reconcile. DB error: {error}"
                );
            }
            lifecycle(LifecycleEvent::Landed);
            outln!("{branch} already merged.");
        }
        AttemptOutcome::Conflict {
            paths,
            submodule_conflicts,
            driver_refusal,
        } => {
            lifecycle(LifecycleEvent::Failed);
            let detail =
                crate::integrate::conflict_details(&paths, &submodule_conflicts).join(", ");
            outln!(
                "✗ {branch} conflicts: {detail}{}",
                driver_refusal
                    .as_ref()
                    .map(|reason| format!("\n{reason}"))
                    .unwrap_or_default()
            );
            failure = Some(format!("land failed: {branch} conflicts"));
        }
        AttemptOutcome::GateFailed { .. } => {
            lifecycle(LifecycleEvent::Failed);
            outln!("✗ {branch} breaks the build (gate red).");
            failure = Some(format!("land failed: {branch} gate red"));
        }
        AttemptOutcome::GateError { reason, log, .. } => {
            // The gate never ran: record it as an environment failure, not as a
            // verdict about the branch, and keep the log so the row can say why.
            let detail = if log.trim().is_empty() {
                reason.clone()
            } else {
                format!("{reason}\n{}", log.trim())
            };
            db.update_merge_status(
                &wt_s,
                thegn_core::attention::MqStatus::GateError.as_str(),
                None,
                None,
                Some(&detail),
            )?;
            lifecycle(LifecycleEvent::Failed);
            outln!("✗ {branch} was NOT gated — {reason}.");
            outln!("  The branch was not judged; fix the gate environment and re-run.");
            failure = Some(format!("land failed: {branch} gate could not run"));
        }
        AttemptOutcome::Unreachable { detail } => {
            db.update_merge_status(
                &wt_s,
                thegn_core::attention::MqStatus::Deferred.as_str(),
                None,
                Some(&detail),
                None,
            )?;
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

#[cfg(test)]
mod conflict_tests {
    use super::parse_conflicted_paths;
    use std::path::PathBuf;

    #[test]
    fn nul_paths_preserve_whitespace_quotes_and_newlines() {
        let paths = parse_conflicted_paths(b"space name.rs\0quote\"name.rs\0line\nbreak.rs\0")
            .expect("valid raw paths");
        assert_eq!(
            paths,
            vec![
                PathBuf::from("space name.rs"),
                PathBuf::from("quote\"name.rs"),
                PathBuf::from("line\nbreak.rs"),
            ]
        );
    }
}

#[cfg(test)]
mod routing_tests {
    use super::*;

    #[test]
    fn provisioned_remote_without_return_credential_fails_before_local_db() {
        let _env = crate::testenv::EnvVarGuard::set(&[
            ("THEGN_REMOTE_ENV", "1"),
            ("THEGN_CONTROL_URL", ""),
            ("THEGN_CONTROL_TOKEN", ""),
        ]);
        let error = add_quiet(&Config::default(), Vec::new(), false, true)
            .unwrap_err()
            .to_string();
        assert!(error.contains("route_to_host credential is missing"));
        assert!(error.contains("no local queue row was created"));
    }

    #[test]
    fn trusted_repo_remote_mode_override_matches_provisioning_authority() {
        let root = std::path::Path::new("/repos/acme");
        let mut cfg = Config::default();
        cfg.merge_queue.remote_mode = thegn_core::config::MergeRemoteMode::Push;
        cfg.workspace.insert(
            thegn_core::config::workspace_slug(root),
            thegn_core::config::WorkspaceConfig {
                merge_queue: thegn_core::config::MergeQueueOverlay {
                    remote_mode: Some(thegn_core::config::MergeRemoteMode::RouteToHost),
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        assert_eq!(
            effective_remote_mode(&cfg, Some(root), false),
            thegn_core::config::MergeRemoteMode::RouteToHost
        );

        cfg.merge_queue.remote_mode = thegn_core::config::MergeRemoteMode::RouteToHost;
        cfg.workspace
            .get_mut(&thegn_core::config::workspace_slug(root))
            .unwrap()
            .merge_queue
            .remote_mode = Some(thegn_core::config::MergeRemoteMode::Push);
        assert_eq!(
            effective_remote_mode(&cfg, Some(root), true),
            thegn_core::config::MergeRemoteMode::Push,
            "repo-resolved push must ignore a stale return credential"
        );
        assert_eq!(
            effective_remote_mode(&cfg, None, true),
            thegn_core::config::MergeRemoteMode::RouteToHost,
            "credential is the authority hint only when no repo can be resolved"
        );
    }
}

#[cfg(test)]
mod queue_error_tests {
    use super::queue_error_kind;
    use std::time::Duration;

    #[test]
    fn classifies_busy_database_through_anyhow_context() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("queue.sqlite");
        let first = rusqlite::Connection::open(&path).unwrap();
        first
            .execute_batch("CREATE TABLE held (value INTEGER);")
            .unwrap();
        first.execute_batch("BEGIN EXCLUSIVE;").unwrap();
        let second = rusqlite::Connection::open(&path).unwrap();
        second.busy_timeout(Duration::ZERO).unwrap();
        let error = second
            .query_row("SELECT value FROM held", [], |row| row.get::<_, i64>(0))
            .unwrap_err();
        let error = anyhow::Error::from(error).context("merge queue unreadable");
        assert_eq!(queue_error_kind(&error), ("queue_busy", true));
    }

    #[test]
    fn classifies_corrupt_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("corrupt.sqlite");
        std::fs::write(&path, b"not a sqlite database").unwrap();
        let conn = rusqlite::Connection::open(&path).unwrap();
        let error = conn
            .query_row("SELECT value FROM held", [], |row| row.get::<_, i64>(0))
            .unwrap_err();
        assert_eq!(
            queue_error_kind(&anyhow::Error::from(error)),
            ("queue_corrupt", false)
        );
    }

    #[test]
    fn classifies_incompatible_schema_access() {
        let error = anyhow::Error::new(thegn_core::db::MigrationAccessError::NewerSchema {
            observed: 99,
            supported: 1,
        });
        assert_eq!(queue_error_kind(&error), ("queue_incompatible", false));
    }

    #[test]
    fn unknown_database_failures_remain_unavailable() {
        let error = anyhow::anyhow!("unspecified queue failure");
        assert_eq!(queue_error_kind(&error), ("queue_unavailable", false));
    }
}

#[cfg(test)]
mod progress_tests {
    use super::progress_line;
    use thegn_core::attention::MqStatus;

    #[test]
    fn progress_is_defined_for_every_persisted_status() {
        for status in MqStatus::ALL {
            let line = progress_line(status, "feature", "detail\nmore");
            let expected = status.is_held()
                || matches!(
                    status,
                    MqStatus::Landed | MqStatus::Ready | MqStatus::AgentRunning
                );
            assert_eq!(line.is_some(), expected, "{status:?}");
        }
        assert!(
            progress_line(MqStatus::GateError, "feature", "reason")
                .unwrap()
                .contains("NOT gated")
        );
        assert!(
            progress_line(MqStatus::AgentBlocked, "feature", "reason")
                .unwrap()
                .contains("environment/isolation policy")
        );
        for status in [MqStatus::Deferred, MqStatus::GateFailed] {
            assert!(
                progress_line(status, "feature", "reason")
                    .unwrap()
                    .contains("deferred")
            );
        }
    }
}

#[cfg(test)]
mod gate_verdict_tests {
    use super::drain_json;
    use crate::merge_driver::{DriveOutcome, GateVerdictReport};

    #[test]
    fn drain_json_reports_gate_phase_and_class_and_keeps_compatibility_list() {
        let outcome = DriveOutcome {
            gate_error: vec!["feature".into()],
            gate_verdicts: vec![GateVerdictReport {
                branch: "feature".into(),
                verdict: "error".into(),
                phase: "setup".into(),
                class: "timeout".into(),
                reason: "setup command timed out".into(),
            }],
            ..Default::default()
        };
        let json = drain_json("main", &outcome);
        assert_eq!(json["gate_error"][0], "feature");
        assert_eq!(json["gate_verdicts"][0]["branch"], "feature");
        assert_eq!(json["gate_verdicts"][0]["verdict"], "error");
        assert_eq!(json["gate_verdicts"][0]["phase"], "setup");
        assert_eq!(json["gate_verdicts"][0]["class"], "timeout");
    }
}

#[cfg(test)]
mod retry_tests {
    use super::{MergeRetryOutcome, retry_message};

    #[test]
    fn retry_messages_are_distinct() {
        let worktree = "/private/repo/feature";
        assert_eq!(
            retry_message(worktree, &MergeRetryOutcome::Rearmed),
            Ok("Re-queued for the next drain.")
        );
        assert_eq!(
            retry_message(worktree, &MergeRetryOutcome::NotQueued),
            Err(format!("{worktree} is not in the merge queue."))
        );
        assert_eq!(
            retry_message(
                worktree,
                &MergeRetryOutcome::NotRetryable {
                    status: "ready".into()
                }
            ),
            Err("cannot retry a ready row: retry re-arms deferred, gate_failed, gate_error, agent_blocked or needs_human rows".into())
        );
        assert_eq!(
            retry_message(worktree, &MergeRetryOutcome::Claimed),
            Err(format!(
                "{worktree} is being worked by a drain; retry it after the drain finishes"
            ))
        );
    }
}

#[cfg(test)]
mod list_claim_tests {
    use super::{ClaimJson, ListedRow, claim_json, claim_suffix};
    use std::path::Path;
    use thegn_core::db::{Db, MergeQueueRow};
    use thegn_core::store::{MergeClaimRequest, MergeClaimView, WorkspaceStore, WorktreeAuxStore};
    use thegn_core::util;

    fn claim_view(phase: &str, lease_until_ms: Option<i64>) -> MergeClaimView {
        MergeClaimView {
            worktree: "/repo/worktree".to_owned(),
            generation: 7,
            phase: phase.to_owned(),
            lease_until_ms,
            owner_hint: Some("thegn pid 42".to_owned()),
        }
    }

    fn row() -> MergeQueueRow {
        MergeQueueRow {
            worktree: "/repo/worktree".to_owned(),
            branch: "feature".to_owned(),
            target_branch: "main".to_owned(),
            status: "ready".to_owned(),
            queued_at: 1,
            updated_at: 2,
            result_oid: Some("fold-oid".to_owned()),
            conflict_paths: None,
            error_detail: None,
            location: "local".to_owned(),
            agent_attempts: 3,
        }
    }

    #[test]
    fn claim_json_is_null_for_unclaimed_rows() {
        assert_eq!(claim_json(None, 100), None);
        assert_eq!(
            claim_json(Some(&claim_view("unclaimed", Some(200))), 100),
            None
        );
    }

    #[test]
    fn claim_json_marks_live_and_expired_claims() {
        let live_view = claim_view("claimed_no_child", Some(101));
        let live = claim_json(Some(&live_view), 100).unwrap();
        assert_eq!(
            live,
            ClaimJson {
                phase: "claimed_no_child".to_owned(),
                generation: 7,
                lease_until_ms: Some(101),
                owner_hint: Some("thegn pid 42".to_owned()),
                live: true,
            }
        );

        let expired_view = claim_view("claimed_no_child", Some(99));
        let expired = claim_json(Some(&expired_view), 100).unwrap();
        assert!(!expired.live);
        assert_eq!(expired.phase, "claimed_no_child");
        assert_eq!(expired.generation, 7);
        assert_eq!(expired.lease_until_ms, Some(99));
        assert_eq!(expired.owner_hint.as_deref(), Some("thegn pid 42"));
    }

    #[test]
    fn listed_row_json_keeps_row_fields_and_adds_claim() {
        let view = claim_view("claimed_no_child", Some(101));
        let fixture_row = row();
        let listed = ListedRow {
            row: &fixture_row,
            claim: claim_json(Some(&view), 100),
        };
        let value = serde_json::to_value(listed).unwrap();
        assert_eq!(value["worktree"], "/repo/worktree");
        assert_eq!(value["branch"], "feature");
        assert_eq!(value["status"], "ready");
        assert_eq!(value["agent_attempts"], 3);
        assert_eq!(value["claim"]["phase"], "claimed_no_child");
        assert!(value["claim"]["live"].as_bool().unwrap());
        let serialized = value.to_string();
        assert!(!serialized.contains("owner_token"));

        let unclaimed = ListedRow {
            row: &fixture_row,
            claim: claim_json(None, 100),
        };
        let unclaimed_value = serde_json::to_value(unclaimed).unwrap();
        assert!(unclaimed_value["claim"].is_null());
    }

    #[test]
    fn claim_suffix_names_owner_phase_and_lease() {
        assert_eq!(claim_suffix(None, 1_000), "");
        let live = ClaimJson {
            phase: "claimed_no_child".to_owned(),
            generation: 1,
            lease_until_ms: Some(61_000),
            owner_hint: Some("drain-a".to_owned()),
            live: true,
        };
        assert_eq!(
            claim_suffix(Some(&live), 1_000),
            "  [claimed by drain-a, claimed_no_child, lease 60s left]"
        );
        let expired = ClaimJson {
            live: false,
            lease_until_ms: Some(1_000),
            ..live.clone()
        };
        assert_eq!(
            claim_suffix(Some(&expired), 6_000),
            "  [claim by drain-a expired 5s ago, recoverable]"
        );
        let unavailable = ClaimJson {
            live: false,
            lease_until_ms: None,
            ..live.clone()
        };
        assert_eq!(
            claim_suffix(Some(&unavailable), 1_000),
            "  [claim by drain-a, claimed_no_child, lease unavailable]"
        );
        let unknown = ClaimJson {
            owner_hint: None,
            ..live
        };
        assert!(claim_suffix(Some(&unknown), 1_000).contains("unknown owner"));
    }

    #[expect(
        clippy::disallowed_methods,
        reason = "deterministic real-Git fixture for the store observation"
    )]
    fn init_repo(path: &Path) {
        std::fs::create_dir_all(path).unwrap();
        let output = util::git_cmd(path)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args(["init", "-q", "-b", "main"])
            .output()
            .unwrap();
        assert!(output.status.success());
        for args in [
            &["config", "user.name", "Thegn Test"][..],
            &["config", "user.email", "thegn@example.invalid"][..],
            &["config", "commit.gpgsign", "false"][..],
        ] {
            let output = util::git_cmd(path)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .args(args)
                .output()
                .unwrap();
            assert!(output.status.success());
        }
        std::fs::write(path.join("base.txt"), "base\n").unwrap();
        for args in [
            &["add", "-A"][..],
            &["-c", "commit.gpgsign=false", "commit", "-q", "-m", "base"][..],
        ] {
            let output = util::git_cmd(path)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .args(args)
                .output()
                .unwrap();
            assert!(output.status.success());
        }
    }

    #[test]
    fn observed_claim_round_trips_through_the_store() {
        let temp = tempfile::tempdir().unwrap();
        init_repo(temp.path());
        let worktree = temp.path().to_string_lossy().into_owned();
        let db = Db::open_memory().unwrap();
        db.put_worktree("main", &worktree, &worktree, "main", None, None)
            .unwrap();
        db.enqueue_merge(&worktree, "main", "main").unwrap();
        db.update_merge_status(&worktree, "ready", Some("fold-oid"), None, None)
            .unwrap();
        db.admit_merge_claim(&MergeClaimRequest {
            worktree: worktree.clone(),
            branch: "main".to_owned(),
            repo_root: worktree.clone(),
            target_branch: "main".to_owned(),
            location: String::new(),
            expected_status: "ready".to_owned(),
            lease_until_ms: util::now_ms() + 60_000,
            owner_hint: Some("test-drain".to_owned()),
        })
        .unwrap()
        .unwrap();

        // Positive control: the token-free observation API already persists and reads this claim.
        let observed = db.observe_merge_claim(&worktree).unwrap();
        let claim = claim_json(observed.as_ref(), util::now_ms()).unwrap();
        assert!(claim.live);
        assert_eq!(claim.owner_hint.as_deref(), Some("test-drain"));
    }
}

#[cfg(test)]
mod claim_conflict_tests {
    use super::*;

    #[test]
    fn clear_keeps_the_kept_claimed_message_unwrapped() {
        let kept = clear_error_context(anyhow::Error::new(crate::merge_ops::ClearKeptClaimed {
            removed: 1,
            kept: vec!["/r/a".into()],
        }));
        assert!(!kept.to_string().contains("merge queue unavailable"));
        assert!(kept.to_string().contains("kept 1 claimed"));

        let database = clear_error_context(anyhow::anyhow!("boom"));
        assert!(database.to_string().contains("merge queue unavailable"));
    }

    #[test]
    fn rm_names_the_drain() {
        assert!(rm_claimed_message("/r/a").contains("being worked by a drain"));
    }
}
