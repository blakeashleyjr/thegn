//! The PR-queue driver: refresh each queued pull request, work out what is
//! blocking it, and act — or explain why it didn't.
//!
//! The team-mode counterpart to [`crate::merge_driver`], and deliberately the
//! same shape: a synchronous loop that runs **off** the event loop (the CLI
//! calls it directly; the host runs it from `spawn_blocking`), writing status
//! transitions as it goes and reporting each through a `progress` callback the
//! caller uses to print (CLI) or repaint (host).
//!
//! What it does NOT contain is any policy. Every "may we act" question —
//! don't stomp a teammate's push, don't merge unreviewed work, don't touch
//! someone else's PR — is decided by the pure [`thegn_core::pr_queue`] layer and
//! merely executed here. That split is what makes the safety rules testable
//! without a forge.

use std::path::Path;

use thegn_core::agent_task::{TaskKind, TaskVars};
use thegn_core::config::{Config, PrMergeMethod, PrQueueConfig, PrWatchKind};
use thegn_core::db::{Db, PrQueueRow};
use thegn_core::forge::model::{MergeMethod, ReviewThreadRow, owner_repo_from_url};
use thegn_core::forge::{FetchedPr, Forge, ForgeError, PrRef, RepoRef};
use thegn_core::issue::AgentDispatchStatus;
use thegn_core::pr_queue::{self, Blocker, PrQueueFacts, PrqStatus, QueueAction, QueueRowSnapshot};
use thegn_core::pr_review_tasks::{
    ExistingReviewTask, PrReviewTaskContext, ReviewTaskEvent, reconcile_review_tasks,
};
use thegn_core::remote::GitLoc;
use thegn_core::store::{CacheStore, WorktreeAuxStore};

/// One queued pull request to process.
#[derive(Debug, Clone)]
pub(crate) struct PrItem {
    pub key: String,
    pub number: u64,
    /// The queue status word the row had when this pass loaded it.
    pub status: String,
    pub branch: String,
    pub worktree: Option<String>,
    pub agent_attempts: u32,
    pub last_head_oid: Option<String>,
    pub forge: String,
    pub pr_url: Option<String>,
    pub checkout_gen: i64,
}

impl From<&PrQueueRow> for PrItem {
    fn from(r: &PrQueueRow) -> Self {
        PrItem {
            key: r.key.clone(),
            number: r.number,
            status: r.status.clone(),
            branch: r.branch.clone(),
            worktree: r.worktree.clone(),
            agent_attempts: r.agent_attempts,
            last_head_oid: r.last_head_oid.clone(),
            forge: r.forge.clone(),
            pr_url: r.pr_url.clone(),
            checkout_gen: r.checkout_gen,
        }
    }
}

/// One status transition, handed to the caller's `progress` callback (the DB row
/// is already written when this fires).
pub(crate) struct PrStep<'a> {
    pub key: &'a str,
    pub number: u64,
    pub branch: &'a str,
    pub status: &'a str,
    pub detail: &'a str,
}

/// Summary of one pass over the queue.
#[derive(Debug, Default, Clone)]
pub(crate) struct PrOutcome {
    /// Merges confirmed by the forge during this pass, including a merge thegn
    /// requested earlier and a later poll confirmed at the requested head.
    pub merged: Vec<u64>,
    /// Auto-merge was enabled; the forge may merge these after its own rules pass.
    pub auto_merge_armed: Vec<u64>,
    /// The forge accepted or may have accepted a merge request, pending confirmation.
    pub merge_requested: Vec<u64>,
    /// Green and mergeable, held for a human (`merge_mode = "ready"`).
    pub ready: Vec<u64>,
    /// Still blocked; will be re-examined next pass.
    pub blocked: Vec<u64>,
    /// Needs a person.
    pub needs_human: Vec<u64>,
    /// Left the queue (merged or closed on the forge).
    pub dropped: Vec<u64>,
    /// Non-fatal problems worth surfacing — an unreachable forge, an agent that
    /// was configured but could not be resolved. Reported rather than swallowed:
    /// "couldn't look" must never read as "nothing to do".
    pub warnings: Vec<String>,
    /// Durable create/revision events for the TUI drain. The worker fills this
    /// only after the roster upsert succeeds; a future automation consumer can
    /// observe the same deduped event without starting a second agent here.
    pub review_events: Vec<(ReviewTaskEvent, bool)>,
}

fn later_poll_confirms_requested_merge(item: &PrItem, state: &str, head: &str) -> bool {
    state.eq_ignore_ascii_case("MERGED")
        && item.status == PrqStatus::Merging.as_str()
        && !head.is_empty()
        && item.last_head_oid.as_deref() == Some(head)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReviewRefresh {
    NotWatched,
    /// Deep provider data was transiently unavailable. Existing tasks remain
    /// untouched and aggregate review dispatch stays paused for this pass.
    Stale,
    /// A complete snapshot was reconciled. `actionable` distinguishes an empty
    /// provider aggregate from a persistence failure; `has_task` is read back
    /// from the durable roster rather than inferred from a proposed upsert.
    Reconciled {
        actionable: bool,
        has_task: bool,
    },
}

/// Queue rows belonging to one repo, oldest first.
pub(crate) fn rows_for_repo(db: &Db, root: &Path) -> Vec<PrQueueRow> {
    let root_s = root.to_string_lossy();
    db.list_pr_queue()
        .unwrap_or_default()
        .into_iter()
        .filter(|r| r.repo_root == root_s)
        .collect()
}

// --- fetch health: per-row failure backoff ---------------------------------
//
// A forge that is down (or rate-limiting) must not be re-hammered every tick.
// Same shape as `ci_refresh`'s: consecutive failures double the wait, capped,
// and a success clears it. Keyed by row so one unreachable PR never stalls the
// rest of the queue.

/// `key → (consecutive failures, epoch seconds until which to skip)`.
fn health() -> &'static std::sync::Mutex<std::collections::HashMap<String, (u32, i64)>> {
    static H: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, (u32, i64)>>> =
        std::sync::OnceLock::new();
    H.get_or_init(Default::default)
}

/// Whether this row is inside its backoff window.
fn backoff_active(key: &str, now: i64) -> bool {
    health()
        .lock()
        .ok()
        .and_then(|m| m.get(key).map(|(_, until)| now < *until))
        .unwrap_or(false)
}

/// Record a failed fetch and arm the next window. Returns the wait in seconds.
fn record_failure(key: &str, now: i64, poll_secs: u64) -> u64 {
    let Ok(mut m) = health().lock() else { return 0 };
    let e = m.entry(key.to_string()).or_insert((0, 0));
    e.0 = e.0.saturating_add(1);
    let wait = crate::ci_refresh::backoff_secs(e.0, poll_secs);
    e.1 = thegn_core::time_policy::deadline_seconds(now, wait).unwrap_or(i64::MAX);
    wait
}

/// Clear a row's failure state after a good fetch.
fn record_success(key: &str) {
    if let Ok(mut m) = health().lock() {
        m.remove(key);
    }
}

/// Process every queued pull request once.
///
/// Best-effort DB writes throughout (the DB is a cache; the forge is the source
/// of truth), which is why the status writes use `let _ =`.
pub(crate) fn drive_queue(
    cfg: &PrQueueConfig,
    full: &Config,
    forge: &dyn Forge,
    repo_root: &Path,
    db: &Db,
    items: Vec<PrItem>,
    mut progress: impl FnMut(&PrStep),
) -> PrOutcome {
    let mut out = PrOutcome::default();

    // Resolved once per pass, like the merge queue's.
    let agent_cmd = thegn_core::agent_task::resolve_agent(full, &cfg.agent, &cfg.agent_command);
    if agent_cmd.is_none() && !cfg.agent.trim().is_empty() {
        let msg = format!(
            "pr_queue.agent = {:?} matches no [[agents]]/[[tools]] entry; \
             blockers will be reported but not fixed",
            cfg.agent
        );
        tracing::warn!(target: "thegn::prq", "{msg}");
        out.warnings.push(msg);
    }

    let now = thegn_core::util::now();
    for item in items {
        let step = |status: &str, detail: &str, progress: &mut dyn FnMut(&PrStep)| {
            progress(&PrStep {
                key: &item.key,
                number: item.number,
                branch: &item.branch,
                status,
                detail,
            });
        };

        match db.pr_agent_lease_view(&item.key) {
            Ok(Some(view))
                if pr_queue::pr_agent_lease_live(Some(&view), thegn_core::util::now_ms()) =>
            {
                let owner_suffix = match view.owner_hint.as_deref() {
                    Some(hint) => format!(", held by {hint}"),
                    None => String::new(),
                };
                let detail = format!(
                    "PR #{}: a PR agent is running (lease generation {}{}); refresh skipped, agent not dispatched",
                    item.number, view.generation, owner_suffix
                );
                tracing::warn!(target: "thegn::prq", "{detail}");
                out.warnings.push(detail.clone());
                out.blocked.push(item.number);
                step(PrqStatus::AgentRunning.as_str(), &detail, &mut progress);
                continue;
            }
            Err(error) => {
                let detail = format!(
                    "PR #{}: could not read the PR-agent lease ({error}); refresh skipped",
                    item.number
                );
                tracing::warn!(target: "thegn::prq", "{detail}");
                out.warnings.push(detail.clone());
                out.blocked.push(item.number);
                step(PrqStatus::AgentRunning.as_str(), &detail, &mut progress);
                continue;
            }
            Ok(Some(view)) => {
                match crate::pr_agent_lease::reconcile_expired(db, &item.key, item.number, &view) {
                    Ok(Some(warning)) => {
                        tracing::warn!(target: "thegn::prq", "{warning}");
                        out.warnings.push(warning);
                    }
                    Ok(None) => {}
                    Err(error) => {
                        let detail = format!(
                            "PR #{}: could not reconcile the expired PR-agent lease ({error}); refresh skipped",
                            item.number
                        );
                        tracing::warn!(target: "thegn::prq", "{detail}");
                        out.warnings.push(detail.clone());
                        out.blocked.push(item.number);
                        step(PrqStatus::AgentRunning.as_str(), &detail, &mut progress);
                        continue;
                    }
                }
            }
            Ok(None) => {}
        }

        // Where to run forge commands: the PR's own worktree when it has one,
        // else the repo root — `gh` only needs *a* checkout of the repo, so a PR
        // with no worktree is still fetchable. `None` location = local.
        let loc = GitLoc::from_db(
            &item
                .worktree
                .clone()
                .unwrap_or_else(|| repo_root.to_string_lossy().into_owned()),
            None,
        );

        if item.forge != forge.id() {
            let detail = format!(
                "queued forge {} differs from resolved forge {}; re-add (rebind) PR #{} to authorize it",
                item.forge,
                forge.id(),
                item.number
            );
            if let Err(error) = db.update_pr_status(
                &item.key,
                PrqStatus::NeedsHuman.as_str(),
                None,
                Some(&detail),
                None,
            ) {
                tracing::debug!(target: "thegn::prq", %error, "could not persist forge mismatch hold");
            }
            tracing::warn!(target: "thegn::prq", "PR #{}: {detail}", item.number);
            out.warnings.push(format!("PR #{}: {detail}", item.number));
            out.needs_human.push(item.number);
            step(PrqStatus::NeedsHuman.as_str(), &detail, &mut progress);
            continue;
        }
        // Still inside a failure window — skip without touching the row, so a
        // forge outage degrades to "stale" rather than to a queue full of
        // "unreachable" notes. Forge mismatches are held before this skip.
        if backoff_active(&item.key, now) {
            continue;
        }
        let fetched = match forge.fetch_pr(&loc, item.number) {
            Ok(f) => f,
            Err(e) => {
                // A fetch failure is a fact about the network, never a verdict
                // about the PR — leave the row's status and last known head
                // alone so `foreign_push` still has something to compare, and
                // let the poller's backoff handle the retry.
                let wait = record_failure(&item.key, now, cfg.poll_secs());
                let msg = format!(
                    "PR #{}: {} — retrying in {wait}s",
                    item.number,
                    describe(&e)
                );
                tracing::warn!(target: "thegn::prq", "{msg}");
                // best-effort: cache write: the note is a cache; the status column is never written on a failed fetch
                if let Err(error) = db.update_pr_detail(&item.key, &msg) {
                    tracing::debug!(target: "thegn::prq", %error, "could not persist forge fetch retry note");
                }
                out.warnings.push(msg);
                continue;
            }
        };

        record_success(&item.key);
        if let Err(detail) = pr_queue::attest_pr_identity(
            item.pr_url.as_deref(),
            item.number,
            &fetched.pr.url,
            fetched.pr.number,
        ) {
            if let Err(error) = db.update_pr_status(
                &item.key,
                PrqStatus::NeedsHuman.as_str(),
                None,
                Some(&detail),
                None,
            ) {
                tracing::debug!(target: "thegn::prq", %error, "could not persist PR identity hold");
            }
            tracing::warn!(target: "thegn::prq", "PR #{}: {detail}", item.number);
            out.warnings.push(format!("PR #{}: {detail}", item.number));
            out.needs_human.push(item.number);
            step(PrqStatus::NeedsHuman.as_str(), &detail, &mut progress);
            continue;
        }
        let head = fetched.pr.head_ref_oid.clone();
        let blocker = pr_queue::classify(&fetched.pr, cfg);
        let review_refresh =
            refresh_review_tasks(cfg, forge, repo_root, db, &item, &fetched, &mut out);

        // A head thegn didn't produce means a teammate pushed. Two consequences,
        // both deliberate: the attempt budget refills (a long-lived PR must not
        // stay stuck), and — under `pause_on_foreign_push` — we stop rather than
        // race whoever is working in there.
        let moved = pr_queue::foreign_push(item.last_head_oid.as_deref(), &head);
        let mut attempts = item.agent_attempts;
        if moved && pr_queue::attempts_reset(item.last_head_oid.as_deref(), &head, cfg) {
            attempts = 0;
            if let Err(error) = db.set_pr_agent_attempts(&item.key, 0) {
                tracing::debug!(target: "thegn::prq", %error, "could not persist PR agent budget reset after head change");
            }
        }

        let facts = PrQueueFacts {
            worktree: item.worktree.clone(),
            agent_attempts: attempts,
            last_head_oid: item.last_head_oid.clone(),
            is_own: true, // fresh shared admission runs before any dispatch side effect
            agent_available: agent_cmd.is_some(),
        };

        let action = suppress_aggregate_review_dispatch(
            pr_queue::decide(&blocker, &facts, cfg),
            &blocker,
            review_refresh,
        );

        // The foreign-push guard applies only where it means something: we are
        // about to *write*. Merging a PR someone else advanced is fine (it is
        // still green); pushing over them is not.
        let action = pr_queue::pause_for_foreign_push(action, moved, cfg);

        match action {
            QueueAction::Drop => {
                let confirmed_later =
                    later_poll_confirms_requested_merge(&item, &fetched.pr.state, &head);
                let status = if fetched.pr.state.eq_ignore_ascii_case("MERGED") {
                    PrqStatus::Merged
                } else {
                    PrqStatus::Closed
                };
                let persisted_detail = if confirmed_later {
                    "merged (confirmed on a later poll)".to_owned()
                } else {
                    format!("{} on the forge", status.as_str())
                };
                // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
                let _ = db.update_pr_status(
                    &item.key,
                    status.as_str(),
                    Some(blocker.as_str()),
                    Some(&persisted_detail),
                    Some(&head),
                );
                step(
                    status.as_str(),
                    if confirmed_later {
                        &persisted_detail
                    } else {
                        ""
                    },
                    &mut progress,
                );
                if confirmed_later {
                    out.merged.push(item.number);
                } else {
                    out.dropped.push(item.number);
                }
            }

            QueueAction::Wait => {
                let status = PrqStatus::for_blocker(&blocker);
                let detail = blocker_detail(&blocker, &fetched);
                // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
                let _ = db.update_pr_status(
                    &item.key,
                    status.as_str(),
                    Some(blocker.as_str()),
                    Some(&detail),
                    Some(&head),
                );
                step(status.as_str(), &detail, &mut progress);
                if !matches!(blocker, Blocker::None) {
                    out.blocked.push(item.number);
                }
            }

            QueueAction::MarkReady => {
                // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
                let _ = db.update_pr_status(
                    &item.key,
                    PrqStatus::Ready.as_str(),
                    Some(blocker.as_str()),
                    Some("green and mergeable"),
                    Some(&head),
                );
                step(
                    PrqStatus::Ready.as_str(),
                    "green and mergeable",
                    &mut progress,
                );
                out.ready.push(item.number);
            }

            QueueAction::EnableAutoMerge | QueueAction::Merge => {
                let direct = matches!(action, QueueAction::Merge);
                let method = merge_method(cfg.merge_method);
                if let Err(reason) = reattest(db, forge, &loc, &item, &fetched) {
                    let detail = effect_detail(reason, "merge");
                    let moved = detail.contains("head moved before merge");
                    let status = if moved {
                        PrqStatus::Watching
                    } else {
                        PrqStatus::NeedsHuman
                    };
                    if let Err(error) =
                        db.update_pr_status(&item.key, status.as_str(), None, Some(&detail), None)
                    {
                        tracing::debug!(target: "thegn::prq", %error, "could not persist pre-merge attestation hold");
                    }
                    out.warnings.push(format!("PR #{}: {detail}", item.number));
                    if moved {
                        out.blocked.push(item.number);
                    } else {
                        out.needs_human.push(item.number);
                    }
                    step(status.as_str(), &detail, &mut progress);
                    continue;
                }
                // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
                let _ = db.update_pr_status(
                    &item.key,
                    PrqStatus::Merging.as_str(),
                    None,
                    None,
                    Some(&head),
                );
                step(PrqStatus::Merging.as_str(), "", &mut progress);

                // Direct merge now, or ask the forge to merge itself once its
                // own rules allow (branch protection + required reviews stay
                // in charge) — the same op with `auto` flipped.
                let res = forge.merge_pr(
                    &loc,
                    PrRef::Number(item.number),
                    method,
                    cfg.delete_branch_on_merge,
                    Some(&head),
                    !direct,
                );
                let mut after_unavailable = None;
                let after = match forge.pr_status(&loc, PrRef::Number(item.number)) {
                    Ok(status) => match pr_queue::attest_pr_identity(
                        item.pr_url.as_deref(),
                        item.number,
                        &status.url,
                        status.number,
                    ) {
                        Ok(()) => Some(status),
                        Err(reason) => {
                            after_unavailable =
                                Some(format!("PR identity could not be re-attested: {reason}"));
                            tracing::warn!(target: "thegn::prq", "PR #{} merge re-read identity mismatch: {reason}", item.number);
                            None
                        }
                    },
                    Err(error) => {
                        after_unavailable =
                            Some(format!("could not re-read PR status: {}", describe(&error)));
                        tracing::debug!(target: "thegn::prq", %error, "could not re-read PR after merge request");
                        None
                    }
                };
                let outcome = match (after_unavailable, &res) {
                    (Some(reason), Ok(())) => {
                        thegn_core::pr_merge_outcome::MergeOutcome::Unknown(reason)
                    }
                    (_, result) => thegn_core::pr_merge_outcome::classify_merge(
                        direct,
                        result,
                        after.as_ref(),
                        Some(&head),
                    ),
                };
                match outcome {
                    thegn_core::pr_merge_outcome::MergeOutcome::Confirmed => {
                        let detail = "merged";
                        if let Err(error) = db.update_pr_status(
                            &item.key,
                            PrqStatus::Merged.as_str(),
                            Some(blocker.as_str()),
                            Some(detail),
                            Some(&head),
                        ) {
                            tracing::debug!(target: "thegn::prq", %error, "could not persist confirmed merge");
                        }
                        step(PrqStatus::Merged.as_str(), detail, &mut progress);
                        out.merged.push(item.number);
                    }
                    thegn_core::pr_merge_outcome::MergeOutcome::AutoMergeArmed => {
                        let detail = "auto-merge enabled — the forge will merge it";
                        if let Err(error) = db.update_pr_status(
                            &item.key,
                            PrqStatus::Ready.as_str(),
                            Some(blocker.as_str()),
                            Some(detail),
                            Some(&head),
                        ) {
                            tracing::debug!(target: "thegn::prq", %error, "could not persist armed auto-merge");
                        }
                        step(PrqStatus::Ready.as_str(), detail, &mut progress);
                        out.auto_merge_armed.push(item.number);
                    }
                    thegn_core::pr_merge_outcome::MergeOutcome::Requested => {
                        let detail = "merge requested; waiting for the forge to confirm";
                        if let Err(error) = db.update_pr_status(
                            &item.key,
                            PrqStatus::Merging.as_str(),
                            Some(blocker.as_str()),
                            Some(detail),
                            Some(&head),
                        ) {
                            tracing::debug!(target: "thegn::prq", %error, "could not persist pending merge request");
                        }
                        step(PrqStatus::Merging.as_str(), detail, &mut progress);
                        out.merge_requested.push(item.number);
                    }
                    thegn_core::pr_merge_outcome::MergeOutcome::StaleHead { observed } => {
                        let old = head.chars().take(7).collect::<String>();
                        let new = observed.chars().take(7).collect::<String>();
                        let detail =
                            format!("head moved from {old} to {new} before the merge; refreshing");
                        if let Err(error) = db.update_pr_status(
                            &item.key,
                            PrqStatus::Watching.as_str(),
                            Some(blocker.as_str()),
                            Some(&detail),
                            Some(&head),
                        ) {
                            tracing::debug!(target: "thegn::prq", %error, "could not persist stale-head refresh");
                        }
                        step(PrqStatus::Watching.as_str(), &detail, &mut progress);
                        out.blocked.push(item.number);
                    }
                    thegn_core::pr_merge_outcome::MergeOutcome::Unknown(why) => {
                        let detail = format!("merge outcome unknown: {why}; re-checking next pass");
                        tracing::warn!(target: "thegn::prq", "PR #{}: {detail}", item.number);
                        if let Err(error) = db.update_pr_status(
                            &item.key,
                            PrqStatus::Merging.as_str(),
                            Some(blocker.as_str()),
                            Some(&detail),
                            Some(&head),
                        ) {
                            tracing::debug!(target: "thegn::prq", %error, "could not persist unknown merge outcome");
                        }
                        step(PrqStatus::Merging.as_str(), &detail, &mut progress);
                        out.merge_requested.push(item.number);
                        out.warnings.push(format!("PR #{}: {detail}", item.number));
                    }
                    thegn_core::pr_merge_outcome::MergeOutcome::Rejected(why) => {
                        let detail = format!("merge refused: {why}");
                        if let Err(error) = db.update_pr_status(
                            &item.key,
                            PrqStatus::NeedsHuman.as_str(),
                            Some(blocker.as_str()),
                            Some(&detail),
                            Some(&head),
                        ) {
                            tracing::debug!(target: "thegn::prq", %error, "could not persist refused merge");
                        }
                        step(PrqStatus::NeedsHuman.as_str(), &detail, &mut progress);
                        out.needs_human.push(item.number);
                    }
                }
            }

            QueueAction::NeedsHuman(reason) => {
                // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
                let _ = db.update_pr_status(
                    &item.key,
                    PrqStatus::NeedsHuman.as_str(),
                    Some(blocker.as_str()),
                    Some(&reason),
                    Some(&head),
                );
                step(PrqStatus::NeedsHuman.as_str(), &reason, &mut progress);
                out.needs_human.push(item.number);
            }

            QueueAction::DispatchAgent(kind) => {
                if let Some(reason) = crate::agent_run::headless_unsupported() {
                    if let Err(error) = db.update_pr_status(
                        &item.key,
                        PrqStatus::BlockedCi.as_str(),
                        Some(blocker.as_str()),
                        Some(&reason),
                        Some(&head),
                    ) {
                        tracing::debug!(%error, "could not persist unsupported-platform PR hold");
                    }
                    out.blocked.push(item.number);
                    step(PrqStatus::BlockedCi.as_str(), &reason, &mut progress);
                    continue;
                }
                let authorship = match crate::pr_authorship::acquire(
                    cfg.own_prs_only,
                    db,
                    forge,
                    &loc,
                    &item.forge,
                    item.number,
                    &fetched.pr,
                ) {
                    Ok(proof) => proof,
                    Err(reason) => {
                        out.warnings.push(format!("PR #{}: {reason}", item.number));
                        out.needs_human.push(item.number);
                        step(PrqStatus::NeedsHuman.as_str(), reason, &mut progress);
                        continue;
                    }
                };
                // Before waking an agent on a red build, try the cheap thing: a
                // lot of red CI is a flake, and a re-run costs nothing but a
                // little wall-clock.
                if kind == TaskKind::PrCiFailure && attempts == 0 {
                    if let Err(reason) = reattest(db, forge, &loc, &item, &fetched) {
                        let detail = effect_detail(reason, "CI rerun");
                        let moved = detail.contains("head moved before CI rerun");
                        let status = if moved {
                            PrqStatus::Watching
                        } else {
                            PrqStatus::NeedsHuman
                        };
                        if let Err(error) = db.update_pr_status(
                            &item.key,
                            status.as_str(),
                            None,
                            Some(&detail),
                            None,
                        ) {
                            tracing::debug!(target: "thegn::prq", %error, "could not persist pre-rerun attestation hold");
                        }
                        out.warnings.push(format!("PR #{}: {detail}", item.number));
                        if moved {
                            out.blocked.push(item.number);
                        } else {
                            out.needs_human.push(item.number);
                        }
                        step(status.as_str(), &detail, &mut progress);
                        continue;
                    }
                    // Spend the durable per-head allowance before the network
                    // mutation. An error may mean the provider accepted the
                    // request but its response was lost; keep it spent and
                    // let this pass continue to remediation.
                    match db.reserve_pr_ci_rerun(
                        &item.key,
                        item.last_head_oid.as_deref(),
                        &head,
                    ) {
                        Ok(true) => match forge.rerun_failed(&loc, PrRef::Number(item.number)) {
                            Ok(n) if n > 0 => {
                                let detail = format!("re-ran {n} failed check(s) before dispatching");
                                // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
                                let _ = db.update_pr_status(
                                    &item.key,
                                    PrqStatus::BlockedCi.as_str(),
                                    Some(blocker.as_str()),
                                    Some(&detail),
                                    Some(&head),
                                );
                                step(PrqStatus::BlockedCi.as_str(), &detail, &mut progress);
                                out.blocked.push(item.number);
                                continue;
                            }
                            Ok(_) => {}
                            Err(error) => out.warnings.push(format!(
                                "PR #{}: CI rerun response was ambiguous; retry allowance remains spent: {}",
                                item.number,
                                describe(&error)
                            )),
                        },
                        Ok(false) => {}
                        Err(error) => out.warnings.push(format!(
                            "PR #{}: could not reserve CI rerun; continuing without rerun: {error}",
                            item.number
                        )),
                    }
                }

                // Enforce the in-force isolation floor before the handoff.
                // Gate BEFORE consuming an attempt: a fail-closed miss, or an
                // unbuildable sandbox under a demanded floor, is an INFRASTRUCTURE
                // failure — hold the PR and NEVER mark it needs_human.
                let dispatch = crate::agent_run::agent_floor_gate(
                    full,
                    item.worktree.as_deref().unwrap_or_default(),
                    cfg.agent_sandbox,
                    cfg.agent_isolation_floor,
                    cfg.agent_on_floor_miss,
                );
                let (sandbox, isolation) = match dispatch {
                    crate::agent_run::AgentDispatch::InfraHold(reason) => (
                        None,
                        thegn_core::pr_queue::IsolationAdmission::Refused(reason),
                    ),
                    crate::agent_run::AgentDispatch::RunDegraded(spec, warning) => {
                        thegn_core::msg::warn(&warning);
                        (
                            Some(spec),
                            thegn_core::pr_queue::IsolationAdmission::Degraded(warning),
                        )
                    }
                    crate::agent_run::AgentDispatch::Run(spec) => (
                        Some(spec),
                        thegn_core::pr_queue::IsolationAdmission::Admitted,
                    ),
                };
                if let Err(refusal) =
                    thegn_core::pr_queue::admit_prepared_dispatch(kind, true, &isolation)
                {
                    let reason = refusal.to_string();
                    // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
                    let _ = db.update_pr_status(
                        &item.key,
                        PrqStatus::BlockedCi.as_str(),
                        Some(blocker.as_str()),
                        Some(&reason),
                        Some(&head),
                    );
                    step(PrqStatus::BlockedCi.as_str(), &reason, &mut progress);
                    out.blocked.push(item.number);
                    continue;
                }
                let sandbox = sandbox.expect("admitted isolation carries a sandbox spec");

                let (Some(wt), Some(template)) = (item.worktree.as_deref(), agent_cmd.as_deref())
                else {
                    continue;
                };
                let ci_log = if kind == TaskKind::PrCiFailure {
                    pr_ci_log_evidence(db, wt, &head)
                } else {
                    None
                };
                let prepared =
                    compose_with_log(cfg, kind, wt, &item, &fetched, &blocker, ci_log.as_deref());
                if let Err(refusal) = thegn_core::pr_queue::admit_prepared_dispatch(
                    kind,
                    prepared.is_some(),
                    &thegn_core::pr_queue::IsolationAdmission::Admitted,
                ) {
                    let reason = refusal.to_string();
                    if let Err(error) = db.update_pr_status(
                        &item.key,
                        PrqStatus::for_blocker(&blocker).as_str(),
                        Some(blocker.as_str()),
                        Some(&reason),
                        Some(&head),
                    ) {
                        tracing::debug!(%error, "could not persist invalid PR prompt refusal");
                    }
                    out.warnings.push(format!("PR #{}: {reason}", item.number));
                    out.blocked.push(item.number);
                    step(
                        PrqStatus::for_blocker(&blocker).as_str(),
                        &reason,
                        &mut progress,
                    );
                    continue;
                }
                let (vars, prompt) =
                    prepared.expect("prepared dispatch admitted only with a prompt");
                // Preparation can block. Recheck author/account/head before
                // consuming the attempt; unknown or changed evidence holds.
                if let Err(reason) = crate::pr_authorship::revalidate(
                    authorship.as_ref(),
                    db,
                    forge,
                    &loc,
                    &fetched.pr,
                    false,
                ) {
                    out.warnings.push(format!("PR #{}: {reason}", item.number));
                    out.needs_human.push(item.number);
                    continue;
                }
                let actual_branch =
                    GitLoc::from_db(wt, None).git_out(&["rev-parse", "--abbrev-ref", "HEAD"]);
                if !checkout_matches(actual_branch.as_deref(), &fetched.pr.head_ref_name) {
                    let actual = actual_branch.as_deref().unwrap_or("unknown");
                    let reason = format!(
                        "worktree {wt} is on branch {actual}, but PR #{} head is {}; agent dispatch held",
                        item.number, fetched.pr.head_ref_name
                    );
                    if let Err(error) = db.update_pr_status(
                        &item.key,
                        PrqStatus::NeedsHuman.as_str(),
                        None,
                        Some(&reason),
                        Some(&head),
                    ) {
                        tracing::debug!(target: "thegn::prq", %error, "could not persist checkout branch hold");
                    }
                    tracing::warn!(target: "thegn::prq", "PR #{}: {reason}", item.number);
                    out.warnings.push(format!("PR #{}: {reason}", item.number));
                    out.needs_human.push(item.number);
                    step(PrqStatus::NeedsHuman.as_str(), &reason, &mut progress);
                    continue;
                }
                if let Err(reason) = reattest(db, forge, &loc, &item, &fetched) {
                    let detail = effect_detail(reason, "agent dispatch");
                    let moved = detail.contains("head moved before agent dispatch");
                    let status = if moved {
                        PrqStatus::Watching
                    } else {
                        PrqStatus::NeedsHuman
                    };
                    if let Err(error) =
                        db.update_pr_status(&item.key, status.as_str(), None, Some(&detail), None)
                    {
                        tracing::debug!(target: "thegn::prq", %error, "could not persist pre-dispatch attestation hold");
                    }
                    out.warnings.push(format!("PR #{}: {detail}", item.number));
                    if moved {
                        out.blocked.push(item.number);
                    } else {
                        out.needs_human.push(item.number);
                    }
                    step(status.as_str(), &detail, &mut progress);
                    continue;
                }
                let next = attempts + 1;
                let note = format!("agent fixing ({next}/{})", cfg.agent_max_attempts);
                let owner = crate::pr_agent_lease::owner_hint();
                let request = pr_queue::PrAgentAdmit {
                    key: &item.key,
                    expected_attempts: attempts,
                    expected_checkout_gen: item.checkout_gen,
                    max_attempts: cfg.agent_max_attempts,
                    head: &head,
                    blocker: blocker.as_str(),
                    detail: &note,
                    lease_until_ms: crate::pr_agent_lease::lease_until_ms(),
                    owner_hint: Some(&owner),
                    ci_candidate: None,
                };
                let guard = match crate::pr_agent_lease::PrAgentGuard::admit(db, &request) {
                    Ok(Ok(guard)) => guard,
                    Ok(Err(refusal)) => {
                        let text = format!("PR #{}: agent not dispatched: {refusal}", item.number);
                        out.warnings.push(text.clone());
                        out.blocked.push(item.number);
                        step(&current_status(db, &item.key), &text, &mut progress);
                        continue;
                    }
                    Err(error) => {
                        out.warnings.push(format!("PR #{}: could not record the agent dispatch ({error}); agent not started", item.number));
                        out.blocked.push(item.number);
                        continue;
                    }
                };
                step(PrqStatus::AgentRunning.as_str(), &note, &mut progress);

                let verdict = crate::agent_run::run_outcome_cancellable(
                    &crate::agent_run::AgentTaskRun {
                        kind,
                        worktree: wt,
                        prompt: &prompt,
                        command_template: template,
                        vars: &vars,
                        timeout_secs: cfg.agent_timeout_secs,
                        sandbox,
                        credential_free: true,
                    },
                    guard.cancel_flag(),
                );
                if let Some(warning) = agent_not_started_warning(item.number, &verdict) {
                    tracing::warn!(target: "thegn::prq", pr = item.number, "{warning}");
                    out.warnings.push(warning);
                }

                let fresh = authorship.as_ref().and_then(|_| {
                    forge
                        .pr_status(&loc, PrRef::Number(item.number))
                        .ok()
                        .filter(|pr| {
                            pr_queue::attest_pr_identity(
                                item.pr_url.as_deref(),
                                item.number,
                                &pr.url,
                                pr.number,
                            )
                            .is_ok()
                        })
                });
                let own = crate::pr_agent_lease::observe_own_head(&loc, &head, || {
                    if authorship.is_some() {
                        fresh.as_ref().map(|pr| pr.head_ref_oid.clone())
                    } else {
                        forge
                            .pr_status(&loc, PrRef::Number(item.number))
                            .ok()
                            .filter(|pr| {
                                pr_queue::attest_pr_identity(
                                    item.pr_url.as_deref(),
                                    item.number,
                                    &pr.url,
                                    pr.number,
                                )
                                .is_ok()
                            })
                            .map(|pr| pr.head_ref_oid)
                    }
                });
                match crate::pr_authorship::revalidate_after_agent(
                    authorship.as_ref(),
                    db,
                    forge,
                    &loc,
                    fresh.as_ref(),
                    own.as_deref(),
                ) {
                    Ok(()) => {
                        if let Some(own) = own
                            && let Some(warning) = crate::pr_agent_lease::own_head_record_warning(
                                item.number,
                                guard.record_own_head(db, &head, &own),
                            )
                        {
                            out.warnings.push(warning);
                        }
                        match guard.finish(db, None) {
                            Ok(true) => {}
                            Ok(false) => out.warnings.push(format!("PR #{}: agent lease could not be confirmed while the agent ran; the next refresh re-evaluates the row", item.number)),
                            Err(error) => out.warnings.push(format!("PR #{}: agent lease could not be confirmed while the agent ran; the next refresh re-evaluates the row ({error})", item.number)),
                        }
                    }
                    Err(reason) => {
                        match guard.finish(
                            db,
                            Some((PrqStatus::NeedsHuman.as_str(), reason)),
                        ) {
                            Ok(true) => {}
                            Ok(false) => out.warnings.push(format!("PR #{}: agent lease could not be confirmed while the agent ran; the next refresh re-evaluates the row", item.number)),
                            Err(error) => out.warnings.push(format!("PR #{}: agent lease could not be confirmed while the agent ran; the next refresh re-evaluates the row ({error})", item.number)),
                        }
                        step(PrqStatus::NeedsHuman.as_str(), reason, &mut progress);
                        out.warnings.push(format!("PR #{}: {reason}", item.number));
                        out.needs_human.push(item.number);
                    }
                }

                // The exit code decides nothing — the next refresh does, exactly
                // as in the merge queue. An agent can exit non-zero having pushed
                // a good fix, or exit zero having done nothing.
            }
        }
    }
    out
}

fn checkout_matches(actual: Option<&str>, expected: &str) -> bool {
    actual.is_some_and(|branch| branch == expected)
}

/// Re-read both durable queue authorization and forge identity immediately
/// before a mutation. A stale read cannot authorize an effect after a rebind.
pub(crate) fn reattest(
    db: &Db,
    forge: &dyn Forge,
    loc: &GitLoc,
    item: &PrItem,
    expected: &FetchedPr,
) -> Result<(), String> {
    let fresh = forge
        .pr_status(loc, PrRef::Number(item.number))
        .map_err(|error| format!("could not re-attest PR before effect: {}", describe(&error)))?;
    pr_queue::attest_pr_identity(
        item.pr_url.as_deref(),
        item.number,
        &fresh.url,
        fresh.number,
    )?;
    if fresh.head_ref_oid != expected.pr.head_ref_oid
        || fresh.head_ref_name != expected.pr.head_ref_name
    {
        return Err("head moved before effect; refreshing".to_owned());
    }
    if let Some(worktree) = item.worktree.as_deref() {
        let hint = format!("run `thegn pr queue rebind --pr {}`", item.number);
        let actual_root = loc
            .git_out(&["rev-parse", "--show-toplevel"])
            .ok_or_else(|| {
                format!("queued checkout is missing or no longer a Git worktree; {hint}")
            })?;
        if !checkout_root_matches(loc, &actual_root, worktree)
            .ok_or_else(|| format!("queued checkout identity could not be verified; {hint}"))?
        {
            return Err(format!(
                "queued checkout now resolves to another repository; {hint}"
            ));
        }
        let branch = loc
            .git_out(&["rev-parse", "--abbrev-ref", "HEAD"])
            .ok_or_else(|| format!("queued checkout branch could not be verified; {hint}"))?;
        if !checkout_matches(Some(&branch), &expected.pr.head_ref_name) {
            return Err(format!("queued checkout branch changed; {hint}"));
        }
    }
    let current = db
        .list_pr_queue()
        .map_err(|error| format!("could not re-read PR queue row: {error}"))?
        .into_iter()
        .find(|row| row.key == item.key);
    let snapshot = QueueRowSnapshot {
        number: item.number,
        key: item.key.clone(),
        pr_url: item.pr_url.clone(),
        forge: item.forge.clone(),
        worktree: item.worktree.clone(),
        branch: item.branch.clone(),
        checkout_gen: item.checkout_gen,
    };
    pr_queue::attest_queue_row(&snapshot, current.as_ref())?;
    let lease = db
        .pr_agent_lease_view(&item.key)
        .map_err(|error| format!("could not re-read the PR-agent lease: {error}"))?;
    if let Some(view) = lease.as_ref()
        && pr_queue::pr_agent_lease_live(Some(view), thegn_core::util::now_ms())
    {
        return Err(format!(
            "a PR agent is running (lease generation {}); not applying the effect",
            view.generation
        ));
    }
    Ok(())
}

fn checkout_root_matches(loc: &GitLoc, actual_root: &str, worktree: &str) -> Option<bool> {
    if matches!(loc, GitLoc::Local(_)) {
        // The stored binding can be a subdirectory; resolve both paths through Git.
        let expected_root =
            GitLoc::from_db(worktree, None).git_out(&["rev-parse", "--show-toplevel"])?;
        return Some(
            Path::new(actual_root.trim()).canonicalize().ok()?
                == Path::new(expected_root.trim()).canonicalize().ok()?,
        );
    }
    // Remote/provider paths belong to their Git location, not the host filesystem.
    Some(actual_root.trim() == worktree)
}

fn effect_detail(reason: String, effect: &str) -> String {
    reason.replace("before effect", &format!("before {effect}"))
}

/// Reconcile the complete THE-27 review snapshot for one explicitly watched
/// queue row. Every call is made by the existing blocking queue worker.
/// Transient provider failures return `Stale` without touching roster rows.
fn refresh_review_tasks(
    cfg: &PrQueueConfig,
    forge: &dyn Forge,
    repo_root: &Path,
    db: &Db,
    item: &PrItem,
    fetched: &FetchedPr,
    out: &mut PrOutcome,
) -> ReviewRefresh {
    if !cfg.watches(PrWatchKind::Review) {
        return ReviewRefresh::NotWatched;
    }

    let Some((owner, repo)) = owner_repo_from_url(&fetched.pr.url) else {
        out.warnings.push(format!(
            "PR #{}: review refresh could not identify the repository",
            item.number
        ));
        return ReviewRefresh::Stale;
    };
    let repository = format!("{owner}/{repo}");
    let repo_ref = RepoRef { owner, repo };
    // The repo root is only a read-only forge/cache location for by-number
    // rows. It is never an executable identity and must not enter the task.
    let query_worktree = item
        .worktree
        .as_deref()
        .unwrap_or_else(|| repo_root.to_str().unwrap_or_default());
    let execution_worktree = item.worktree.as_deref().unwrap_or_default();
    let loc = GitLoc::from_db(query_worktree, None);
    let conversation = match forge.conversation(&loc, &repo_ref, item.number) {
        Ok(conversation) => conversation,
        Err(error) => {
            out.warnings.push(format!(
                "PR #{} review is stale: {}",
                item.number,
                error.describe()
            ));
            return ReviewRefresh::Stale;
        }
    };
    let diff = match forge.pr_diff(&loc, PrRef::Number(item.number)) {
        Ok(diff) => diff,
        Err(error) => {
            out.warnings.push(format!(
                "PR #{} review is stale: {}",
                item.number,
                error.describe()
            ));
            return ReviewRefresh::Stale;
        }
    };
    let snapshot = thegn_core::review::PrReviewSnapshot {
        worktree_key: execution_worktree.to_string(),
        branch: item.branch.clone(),
        pr_number: item.number,
        head_oid: fetched.pr.head_ref_oid.clone(),
        fetched_at: thegn_core::util::now(),
        conversation,
        diff,
    };
    // Complete payload only. This is the same cache consumed by THE-27's
    // panel/diff projection; a DB miss merely makes the next hydrate stale.
    if !execution_worktree.is_empty()
        && let Err(error) = db.put_pr_review_cache(&snapshot)
    {
        tracing::warn!(target: "thegn::prq", %error, number = item.number, "review cache write failed");
    }

    let existing_rows = match db.list_review_tasks() {
        Ok(rows) => rows,
        Err(error) => {
            out.warnings.push(format!(
                "PR #{} review roster is unavailable: {error}",
                item.number
            ));
            return ReviewRefresh::Stale;
        }
    };
    let existing: Vec<ExistingReviewTask> =
        existing_rows.iter().map(ExistingReviewTask::from).collect();
    let context = PrReviewTaskContext {
        forge: item.forge.as_str(),
        repository: repository.as_str(),
        pr_url: fetched.pr.url.as_str(),
        pr_title: fetched.pr.title.as_str(),
        base: fetched.pr.base_ref_name.as_str(),
        worktree_path: execution_worktree,
        role: cfg.agent.as_str(),
        prompt_template: cfg.prompts.resolve(TaskKind::PrReview),
    };
    let plan = match reconcile_review_tasks(&snapshot, context, &existing) {
        Ok(plan) => plan,
        Err(error) => {
            out.warnings.push(format!(
                "PR #{} review task template is invalid: {error}",
                item.number
            ));
            return ReviewRefresh::Stale;
        }
    };

    for resolution in plan.resolutions {
        match db.resolve_review_task(&resolution) {
            Ok(true) => {
                if let Err(error) = db.put_review_thread_resolved_notification(&resolution) {
                    tracing::warn!(target: "thegn::prq", %error, task = resolution.dispatch_id, "review resolution notification failed");
                }
            }
            Ok(false) => {}
            Err(error) => out.warnings.push(format!(
                "PR #{} review resolution could not be persisted: {error}",
                item.number
            )),
        }
    }
    for upsert in plan.upserts {
        let revised = upsert.existing_id.is_some();
        match db.upsert_review_task(&upsert) {
            Ok(_) => {
                if let Err(error) = db.put_review_task_queued_notification(&upsert.event, revised) {
                    tracing::warn!(target: "thegn::prq", %error, source = %upsert.source_key, "review task notification failed");
                }
                tracing::info!(
                    target: "thegn::prq",
                    event = upsert.event.event,
                    source_key = %upsert.event.source_key,
                    source_revision = %upsert.event.source_revision,
                    "review task event"
                );
                out.review_events.push((upsert.event, revised));
            }
            Err(error) => out.warnings.push(format!(
                "PR #{} review task could not be persisted: {error}",
                item.number
            )),
        }
    }

    let issue_id = format!("pr:{}:{}#{}", item.forge, repository, item.number);
    let has_task = db.list_review_tasks().is_ok_and(|rows| {
        rows.into_iter()
            .any(|row| row.issue_id == issue_id && row.status != AgentDispatchStatus::Done)
    });
    let actionable = snapshot
        .conversation
        .threads
        .iter()
        .any(|thread| !thread.resolved && !thread.id.trim().is_empty())
        || (snapshot.conversation.threads.is_empty()
            && snapshot.conversation.reviews.iter().rev().any(|review| {
                review.state.eq_ignore_ascii_case("CHANGES_REQUESTED")
                    && !review.body.trim().is_empty()
            }));
    ReviewRefresh::Reconciled {
        actionable,
        has_task,
    }
}

/// Per-thread reconciliation owns watched review work. The aggregate queue
/// classifier remains visible, but it must never start a competing PR-wide
/// `PrReview` run. A provider that supplies no actionable body/thread is
/// explicitly parked for a human instead of receiving an empty prompt.
fn suppress_aggregate_review_dispatch(
    action: QueueAction,
    blocker: &Blocker,
    refresh: ReviewRefresh,
) -> QueueAction {
    if !matches!(blocker, Blocker::ChangesRequested)
        || !matches!(action, QueueAction::DispatchAgent(TaskKind::PrReview))
    {
        return action;
    }
    match refresh {
        ReviewRefresh::Reconciled {
            actionable: false, ..
        } => QueueAction::NeedsHuman(
            "changes requested, but the forge supplied no actionable review thread or body".into(),
        ),
        ReviewRefresh::Reconciled {
            actionable: true,
            has_task: false,
        } => QueueAction::NeedsHuman(
            "review feedback was found, but its durable task could not be queued".into(),
        ),
        ReviewRefresh::Reconciled { has_task: true, .. } | ReviewRefresh::Stale => {
            QueueAction::Wait
        }
        ReviewRefresh::NotWatched => action,
    }
}

/// The row's current status word, for a refresh that failed and must not change it.
fn current_status(db: &Db, key: &str) -> String {
    db.list_pr_queue()
        .unwrap_or_default()
        .into_iter()
        .find(|r| r.key == key)
        .map(|r| r.status)
        .unwrap_or_else(|| PrqStatus::Watching.as_str().to_string())
}

/// A one-line human detail for a blocker.
fn blocker_detail(b: &Blocker, f: &FetchedPr) -> String {
    match b {
        Blocker::None => "green and mergeable".into(),
        Blocker::Draft => "draft — mark it ready for review".into(),
        Blocker::Ci(names) => format!("failing: {}", names.join(", ")),
        Blocker::ChecksPending => "checks still running".into(),
        Blocker::Conflict => format!("conflicts with / behind {}", f.pr.base_ref_name),
        Blocker::ChangesRequested => {
            let n = f.threads.iter().filter(|t| !t.resolved).count();
            format!("changes requested ({n} unresolved thread(s))")
        }
        Blocker::AwaitingReview => "awaiting review".into(),
        Blocker::Closed => "closed on the forge".into(),
    }
}

fn merge_method(m: PrMergeMethod) -> MergeMethod {
    match m {
        PrMergeMethod::Squash => MergeMethod::Squash,
        PrMergeMethod::Merge => MergeMethod::Merge,
        PrMergeMethod::Rebase => MergeMethod::Rebase,
    }
}

fn describe(e: &ForgeError) -> String {
    e.describe()
}

/// Build the task variables and render the prompt for a PR blocker.
///
/// The mapping (which blocker fills which
/// variables) is unit-testable without spawning anything.
#[cfg(test)]
fn compose(
    cfg: &PrQueueConfig,
    kind: TaskKind,
    worktree: &str,
    item: &PrItem,
    fetched: &FetchedPr,
    blocker: &Blocker,
) -> Option<(TaskVars, String)> {
    compose_with_log(cfg, kind, worktree, item, fetched, blocker, None)
}

/// Build the task variables and render a PR prompt, optionally replacing the
/// usual check-URL evidence with a bounded CI-log excerpt.  The CI refresh
/// owns log redaction; this function still accepts only cache-shaped text so
/// the existing prompt renderer remains the one prompt seam.
pub(crate) fn compose_with_log(
    cfg: &PrQueueConfig,
    kind: TaskKind,
    worktree: &str,
    item: &PrItem,
    fetched: &FetchedPr,
    blocker: &Blocker,
    log: Option<&str>,
) -> Option<(TaskVars, String)> {
    let pr = &fetched.pr;
    let mut vars = TaskVars::new()
        .set("branch", &item.branch)
        .set("base", &pr.base_ref_name)
        .set("worktree", worktree)
        .set("pr_number", item.number.to_string())
        .set("pr_url", &pr.url)
        .set("pr_title", &pr.title);

    match kind {
        TaskKind::PrCiFailure => {
            let names = match blocker {
                Blocker::Ci(n) => n.join(", "),
                _ => String::new(),
            };
            let evidence = log
                .map(str::to_owned)
                .unwrap_or_else(|| check_urls(fetched));
            vars = vars
                .set("checks", names)
                // The forge's rollup carries no log text.  When the CI cache has
                // evidence use it; otherwise retain the established URL
                // fallback instead of inventing output it can't see.
                .set("log", evidence);
        }
        TaskKind::PrReview => {
            vars = vars.set("threads", format_threads(&fetched.threads));
        }
        // The conflict prompt needs only the identity vars set above.
        _ => {}
    }

    match thegn_core::agent_task::render_kind_prompt(kind, cfg.prompts.resolve(kind), &vars) {
        Ok(p) => Some((vars, p)),
        Err(e) => {
            tracing::warn!(
                target: "thegn::prq",
                kind = %kind,
                error = %e,
                "pr queue: prompt template is invalid; not dispatching"
            );
            None
        }
    }
}

fn pr_ci_log_evidence(db: &Db, worktree: &str, head: &str) -> Option<String> {
    let entries = match db.list_ci_logs(&GitLoc::worktree_cache_key(Path::new(worktree))) {
        Ok(entries) => entries,
        Err(error) => {
            tracing::debug!(
                target: "thegn::prq",
                %error,
                "could not read cached CI logs for the PR prompt"
            );
            return None;
        }
    };
    thegn_core::ci_log::pr_repair_excerpt(
        &entries,
        head,
        thegn_core::agent_task::input_max_bytes(TaskKind::PrCiFailure, "log"),
    )
}

/// Where the agent can read the failing runs. Bounded so a repo with dozens of
/// checks cannot bloat the prompt.
fn check_urls(f: &FetchedPr) -> String {
    use thegn_core::forge::model::{Bucket, check_bucket};
    let mut out = String::new();
    for c in
        f.pr.status_check_rollup
            .iter()
            .filter(|c| check_bucket(c) == Bucket::Fail)
            .take(5)
    {
        let name = if c.name.trim().is_empty() {
            "check"
        } else {
            &c.name
        };
        match c.details_url.as_deref() {
            Some(u) if !u.is_empty() => out.push_str(&format!("  - {name}: {u}\n")),
            _ => out.push_str(&format!("  - {name}\n")),
        }
    }
    if out.is_empty() {
        out.push_str("  (no failing check details available — inspect CI directly)\n");
    }
    out
}

/// Unresolved review threads, as prompt text. Bounded for the same reason.
fn format_threads(threads: &[ReviewThreadRow]) -> String {
    let mut out = String::new();
    for t in threads.iter().filter(|t| !t.resolved).take(20) {
        let where_ = match t.line {
            Some(l) if !t.path.is_empty() => format!("{}:{l}", t.path),
            _ if !t.path.is_empty() => t.path.clone(),
            _ => "(general)".to_string(),
        };
        out.push_str(&format!("  - {} on {where_}: {}\n", t.author, t.snippet));
    }
    if out.is_empty() {
        out.push_str("  (no unresolved threads found)\n");
    }
    out
}

/// The warning for a PR-queue agent that never started (an infrastructure failure, not a code result).
fn agent_not_started_warning(
    number: u64,
    verdict: &crate::agent_run::AgentRunVerdict,
) -> Option<String> {
    verdict.not_started().map(|reason| {
        format!("PR #{number}: agent did not start: {reason}; the attempt was still counted")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::forge::model::PrStatus;

    fn uncontained_config() -> Config {
        Config {
            sandbox: thegn_core::config::SandboxConfig {
                isolation_floor: Some(thegn_core::config::IsolationFloor::Off),
                ..thegn_core::config::SandboxConfig::default()
            },
            ..Config::default()
        }
    }

    fn cfg() -> PrQueueConfig {
        PrQueueConfig {
            enabled: true,
            ..PrQueueConfig::default()
        }
    }

    #[test]
    fn checkout_matches_requires_a_known_exact_branch() {
        assert!(checkout_matches(Some("feat"), "feat"));
        assert!(!checkout_matches(Some("other"), "feat"));
        assert!(!checkout_matches(None, "feat"));
    }

    #[expect(clippy::disallowed_methods)]
    fn git_repo_on_branch(dir: &Path, branch: &str) {
        thegn_core::fsperm::restrict_dir_to_owner(dir).unwrap();
        let git = |args: &[&str]| {
            let status = thegn_core::util::git_cmd(dir)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .args(args)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?} failed in {}", dir.display());
        };
        git(&["-c", "commit.gpgsign=false", "init"]);
        git(&["config", "user.email", "queue-test@example.invalid"]);
        git(&["config", "user.name", "Queue Test"]);
        std::fs::write(dir.join("tracked"), "fixture\n").unwrap();
        git(&["add", "tracked"]);
        git(&["-c", "commit.gpgsign=false", "commit", "-m", "fixture"]);
        git(&["checkout", "-b", branch]);
    }

    #[expect(
        clippy::disallowed_methods,
        reason = "the test fixture runs a hermetic git remote command"
    )]
    fn enqueue_pr_with_checkout(
        dir: &tempfile::TempDir,
        db: &Db,
        number: u64,
        branch: &str,
        pr_url: &str,
    ) -> (std::path::PathBuf, PrItem) {
        let checkout = dir.path().join(format!("checkout-{number}"));
        std::fs::create_dir(&checkout).unwrap();
        git_repo_on_branch(&checkout, branch);
        let suffix = format!("/pull/{number}");
        let repo_url = pr_url
            .strip_suffix(suffix.as_str())
            .expect("fixture PR URL ends with its number");
        let git = thegn_core::util::git_cmd(&checkout)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .args(["remote", "add", "origin", repo_url])
            .status()
            .unwrap();
        assert!(
            git.success(),
            "could not set origin for {}",
            checkout.display()
        );
        let checkout = checkout.canonicalize().unwrap();
        let checkout_s = checkout.to_string_lossy().into_owned();
        db.enqueue_bound_pr(
            &checkout_s,
            number,
            Some(&checkout_s),
            branch,
            "main",
            "github",
            pr_url,
        )
        .unwrap();
        let row = db
            .list_pr_queue()
            .unwrap()
            .into_iter()
            .find(|row| row.number == number)
            .expect("fixture row was persisted with its bound identity");
        (checkout, PrItem::from(&row))
    }

    #[test]
    fn checkout_branch_mismatch_holds_before_spending_attempt_or_running_agent() {
        let (dir, db) = temp_db("prq-wrong-checkout");
        let checkout = dir.path().join("checkout");
        std::fs::create_dir(&checkout).unwrap();
        git_repo_on_branch(&checkout, "other");
        let checkout = checkout.canonicalize().unwrap();
        let checkout_s = checkout.to_string_lossy().into_owned();
        db.enqueue_bound_pr(
            "/repo",
            7,
            Some(&checkout_s),
            "feat",
            "main",
            "github",
            "https://github.com/me/repo/pull/7",
        )
        .unwrap();
        let sentinel = dir.path().join("agent-must-not-run");
        let mut config = cfg();
        config.watch = vec![PrWatchKind::Ci];
        config.own_prs_only = false;
        config.pause_on_foreign_push = false;
        config.agent_command = format!("touch {}", sentinel.display());
        let forge = FakeForge::new(Some(failing_ci_pr("abc")));
        let row = db.list_pr_queue().unwrap().remove(0);
        let out = drive_queue(
            &config,
            &Config::default(),
            &forge,
            Path::new("/repo"),
            &db,
            vec![PrItem::from(&row)],
            |_| {},
        );
        let row = db.list_pr_queue().unwrap().remove(0);
        assert_eq!(out.needs_human, vec![7]);
        assert!(!sentinel.exists());
        assert_eq!(row.agent_attempts, 0);
        assert_eq!(row.status, PrqStatus::NeedsHuman.as_str());
        let detail = row.detail.unwrap();
        assert!(
            detail.contains("queued checkout branch changed; run `thegn pr queue rebind --pr 7`"),
            "{detail}"
        );
    }

    #[test]
    fn identity_mismatch_and_unbound_rows_hold_before_any_effect_or_attempt_change() {
        for (label, bound_url, fetched_url) in [
            (
                "project-swap",
                Some("https://github.com/me/repo/pull/7"),
                "https://github.com/other/repo/pull/7",
            ),
            ("unbound", None, "https://github.com/me/repo/pull/7"),
        ] {
            let (dir, db) = temp_db(&format!("prq-identity-{label}"));
            if let Some(url) = bound_url {
                db.enqueue_bound_pr("/repo", 7, Some("/w/feat"), "feat", "main", "github", url)
                    .unwrap();
            } else {
                db.enqueue_pr("/repo", 7, Some("/w/feat"), "feat", "main", "github")
                    .unwrap();
            }
            let saved = db.list_pr_queue().unwrap().remove(0);
            db.set_pr_agent_attempts(&saved.key, 1).unwrap();
            let row = db.list_pr_queue().unwrap().remove(0);
            let forge = FakeForge::new(Some(failing_ci_pr("abc")));
            forge.pr.lock().unwrap().as_mut().unwrap().url = fetched_url.into();
            let mut config = cfg();
            config.watch = vec![PrWatchKind::Ci];
            config.own_prs_only = false;
            let sentinel = dir.path().join("agent-must-not-run");
            config.agent_command = format!("touch {}", sentinel.display());
            let out = drive_queue(
                &config,
                &Config::default(),
                &forge,
                Path::new("/repo"),
                &db,
                vec![PrItem::from(&row)],
                |_| {},
            );
            let held = db.list_pr_queue().unwrap().remove(0);
            assert_eq!(out.needs_human, vec![7], "{label}");
            assert_eq!(held.status, "needs_human", "{label}");
            assert_eq!(held.agent_attempts, 1, "{label}");
            assert_eq!(held.ci_rerun_head, None, "{label}");
            assert!(
                out.warnings
                    .iter()
                    .any(|warning| warning.contains("identity"))
            );
            assert!(
                forge
                    .calls()
                    .iter()
                    .all(|call| { !call.starts_with("merge") && !call.starts_with("rerun") }),
                "{label}: {:?}",
                forge.calls()
            );
            assert!(!sentinel.exists());
        }
    }

    fn item() -> PrItem {
        PrItem {
            key: "/repo#7".into(),
            number: 7,
            status: "ready".into(),
            branch: "feat".into(),
            worktree: Some("/w/feat".into()),
            agent_attempts: 0,
            last_head_oid: Some("abc".into()),
            forge: "github".into(),
            pr_url: Some("https://github.com/me/repo/pull/7".into()),
            checkout_gen: 0,
        }
    }

    /// Persist the authorization used by driver tests, then pass the exact row
    /// snapshot the production queue loader would construct.
    fn saved_bound_item(dir: &tempfile::TempDir, db: &Db, mut item: PrItem) -> PrItem {
        let checkout = dir.path().join("checkout-feat");
        if item.worktree.is_some() {
            if !checkout.exists() {
                std::fs::create_dir(&checkout).unwrap();
                git_repo_on_branch(&checkout, &item.branch);
            }
            item.worktree = Some(
                checkout
                    .canonicalize()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
            );
        }
        db.enqueue_bound_pr(
            "/repo",
            item.number,
            item.worktree.as_deref(),
            &item.branch,
            "main",
            &item.forge,
            item.pr_url
                .as_deref()
                .expect("bound driver fixture needs a PR URL"),
        )
        .unwrap();
        let rows = db.list_pr_queue().unwrap();
        let row = rows
            .iter()
            .find(|row| row.number == item.number)
            .expect("fixture row was persisted with its bound identity");
        PrItem::from(row)
    }

    fn thread(author: &str, path: &str, line: Option<u64>, resolved: bool) -> ReviewThreadRow {
        ReviewThreadRow {
            author: author.into(),
            path: path.into(),
            line,
            snippet: "please rename this".into(),
            resolved,
            created_at: String::new(),
        }
    }

    fn fetched(threads: Vec<ReviewThreadRow>) -> FetchedPr {
        FetchedPr {
            pr: PrStatus {
                number: 7,
                title: "Add widget".into(),
                state: "OPEN".into(),
                url: "https://github.com/me/repo/pull/7".into(),
                author: None,
                is_draft: false,
                head_ref_name: "feat".into(),
                head_ref_oid: "abc".into(),
                base_ref_name: "main".into(),
                mergeable: "MERGEABLE".into(),
                merge_state_status: "CLEAN".into(),
                review_decision: None,
                status_check_rollup: vec![],
                checks: Default::default(),
            },
            threads,
        }
    }

    #[test]
    fn the_conflict_prompt_names_the_pr_and_its_base() {
        let f = fetched(vec![]);
        let (vars, p) = compose(
            &cfg(),
            TaskKind::PrConflict,
            "/w/feat",
            &item(),
            &f,
            &Blocker::Conflict,
        )
        .unwrap();
        assert_eq!(vars.get("pr_number"), Some("7"));
        assert_eq!(vars.get("base"), Some("main"));
        assert!(p.contains("#7") && p.contains("Add widget") && p.contains("main"));
        // The PR family's rules, not the merge queue's.
        assert!(p.contains("DO push") && p.contains("--force-with-lease"));
        assert!(p.contains("Do NOT merge"));
    }

    #[test]
    fn the_ci_prompt_lists_the_failing_checks() {
        let f = fetched(vec![]);
        let (vars, p) = compose(
            &cfg(),
            TaskKind::PrCiFailure,
            "/w/feat",
            &item(),
            &f,
            &Blocker::Ci(vec!["clippy".into(), "test".into()]),
        )
        .unwrap();
        assert_eq!(vars.get("checks"), Some("clippy, test"));
        assert!(p.contains("clippy, test"));
        // With no rollup details, the prompt says so instead of implying output.
        assert!(p.contains("no failing check details"));
    }

    #[test]
    fn the_ci_prompt_accepts_bounded_log_evidence() {
        let f = fetched(vec![]);
        let (_, p) = compose_with_log(
            &cfg(),
            TaskKind::PrCiFailure,
            "/w/feat",
            &item(),
            &f,
            &Blocker::Ci(vec!["test".into()]),
            Some("error: boom\ntoken=***redacted***\n"),
        )
        .unwrap();
        assert!(p.contains("error: boom"));
        assert!(p.contains("***redacted***"));
    }

    #[test]
    fn the_ci_prompt_uses_cached_log_for_the_pr_head() {
        let db = Db::open_memory().unwrap();
        db.put_ci_log(&thegn_core::ci_log::CiLogEntry {
            worktree: "/w/feat".into(),
            run_id: "11".into(),
            job_id: "22".into(),
            job_name: "test".into(),
            text: "compile ok\nerror: boom\n".into(),
            head_sha: "abc".into(),
            fetched_at: 5,
            ..Default::default()
        })
        .unwrap();
        let evidence = pr_ci_log_evidence(&db, "/w/feat", "abc");
        assert!(evidence.is_some());
        let (_, prompt) = compose_with_log(
            &cfg(),
            TaskKind::PrCiFailure,
            "/w/feat",
            &item(),
            &fetched(vec![]),
            &Blocker::Ci(vec!["test".into()]),
            evidence.as_deref(),
        )
        .unwrap();
        assert!(prompt.contains("error: boom"));
        assert!(!prompt.contains("no failing check details"));
    }

    #[test]
    fn the_ci_prompt_keeps_urls_without_cached_head_evidence() {
        let db = Db::open_memory().unwrap();
        db.put_ci_log(&thegn_core::ci_log::CiLogEntry {
            worktree: "/w/feat".into(),
            run_id: "11".into(),
            job_id: "22".into(),
            job_name: "test".into(),
            text: "compile ok\nerror: boom\n".into(),
            head_sha: "abc".into(),
            fetched_at: 5,
            ..Default::default()
        })
        .unwrap();
        let evidence = pr_ci_log_evidence(&db, "/w/feat", "zzz");
        assert!(evidence.is_none());
        let (_, prompt) = compose_with_log(
            &cfg(),
            TaskKind::PrCiFailure,
            "/w/feat",
            &item(),
            &fetched(vec![]),
            &Blocker::Ci(vec!["test".into()]),
            evidence.as_deref(),
        )
        .unwrap();
        assert!(prompt.contains("no failing check details"));
    }

    #[test]
    fn the_review_prompt_lists_only_unresolved_threads() {
        let f = fetched(vec![
            thread("alice", "src/a.rs", Some(12), false),
            thread("bob", "src/b.rs", Some(3), true),
        ]);
        let (_, p) = compose(
            &cfg(),
            TaskKind::PrReview,
            "/w/feat",
            &item(),
            &f,
            &Blocker::ChangesRequested,
        )
        .unwrap();
        assert!(p.contains("alice") && p.contains("src/a.rs:12"));
        assert!(
            !p.contains("bob"),
            "a resolved thread is not outstanding work"
        );
        // And the agent is told to leave resolution to the reviewer.
        assert!(p.contains("Do NOT resolve"));
    }

    #[test]
    fn a_review_with_nothing_unresolved_says_so_rather_than_going_blank() {
        let f = fetched(vec![thread("bob", "x", None, true)]);
        let (_, p) = compose(
            &cfg(),
            TaskKind::PrReview,
            "/w/feat",
            &item(),
            &f,
            &Blocker::ChangesRequested,
        )
        .unwrap();
        assert!(p.contains("no unresolved threads"));
    }

    #[test]
    fn a_custom_template_replaces_the_builtin() {
        let mut c = cfg();
        c.prompts.conflict = "rebase {branch} onto {base} for #{pr_number}".into();
        let f = fetched(vec![]);
        let (_, p) = compose(
            &c,
            TaskKind::PrConflict,
            "/w/feat",
            &item(),
            &f,
            &Blocker::Conflict,
        )
        .unwrap();
        assert_eq!(p, "rebase feat onto main for #7");
    }

    #[test]
    fn a_broken_template_refuses_to_dispatch() {
        let mut c = cfg();
        c.prompts.review = "{nope}".into();
        let f = fetched(vec![]);
        assert!(
            compose(
                &c,
                TaskKind::PrReview,
                "/w/feat",
                &item(),
                &f,
                &Blocker::ChangesRequested
            )
            .is_none(),
            "a typo'd placeholder must not reach the agent as a blank"
        );
    }

    #[test]
    fn empty_or_stale_aggregate_review_never_launches_a_generic_agent() {
        let dispatch = QueueAction::DispatchAgent(TaskKind::PrReview);
        assert!(matches!(
            suppress_aggregate_review_dispatch(
                dispatch.clone(),
                &Blocker::ChangesRequested,
                ReviewRefresh::Reconciled {
                    actionable: false,
                    has_task: false,
                },
            ),
            QueueAction::NeedsHuman(reason) if reason.contains("no actionable")
        ));
        assert_eq!(
            suppress_aggregate_review_dispatch(
                dispatch,
                &Blocker::ChangesRequested,
                ReviewRefresh::Stale,
            ),
            QueueAction::Wait
        );
    }

    #[test]
    fn blocker_details_are_human_readable() {
        let f = fetched(vec![
            thread("a", "x", None, false),
            thread("b", "y", None, false),
        ]);
        assert!(blocker_detail(&Blocker::Ci(vec!["t".into()]), &f).contains("failing: t"));
        assert!(blocker_detail(&Blocker::Conflict, &f).contains("main"));
        assert!(blocker_detail(&Blocker::ChangesRequested, &f).contains("2 unresolved"));
        assert!(blocker_detail(&Blocker::Draft, &f).contains("draft"));
        assert!(blocker_detail(&Blocker::None, &f).contains("mergeable"));
    }

    #[test]
    fn a_failing_row_backs_off_and_a_success_clears_it() {
        // Distinct key per test run: the registry is process-global.
        let key = "/repo#backoff-test";
        let now = 1_000i64;
        assert!(!backoff_active(key, now), "clean row is never in backoff");

        let w1 = record_failure(key, now, 60);
        assert!(w1 > 0, "a failure must arm a window");
        assert!(backoff_active(key, now), "armed window skips the row");
        assert!(
            !backoff_active(key, now + w1 as i64),
            "the window expires rather than sticking"
        );

        // Consecutive failures widen it, so a forge outage isn't re-hammered.
        let w2 = record_failure(key, now, 60);
        assert!(w2 > w1, "backoff must grow: {w1} → {w2}");

        record_success(key);
        assert!(!backoff_active(key, now), "a good fetch clears the window");
        // ...and the failure count resets, so the next blip starts small again.
        assert_eq!(record_failure(key, now, 60), w1);
        record_success(key);
    }

    #[test]
    fn merge_methods_map_across() {
        assert_eq!(merge_method(PrMergeMethod::Squash), MergeMethod::Squash);
        assert_eq!(merge_method(PrMergeMethod::Merge), MergeMethod::Merge);
        assert_eq!(merge_method(PrMergeMethod::Rebase), MergeMethod::Rebase);
    }

    #[test]
    fn check_urls_are_bounded_and_prefer_details_links() {
        use thegn_core::forge::model::CheckRun;
        let mk = |n: &str, url: Option<&str>| CheckRun {
            name: n.into(),
            status: "COMPLETED".into(),
            conclusion: Some("FAILURE".into()),
            state: None,
            workflow_name: None,
            details_url: url.map(String::from),
            started_at: None,
            completed_at: None,
        };
        let mut f = fetched(vec![]);
        f.pr.status_check_rollup = (0..9).map(|i| mk(&format!("c{i}"), Some("u"))).collect();
        let s = check_urls(&f);
        assert_eq!(s.lines().count(), 5, "bounded so the prompt can't bloat");
        assert!(s.contains("c0: u"));
    }

    // ---- the fake forge the trait was written for ----------------------------

    /// Records every call; answers `fetch` with a configurable PR.
    struct FakeForge {
        pr: std::sync::Mutex<Option<PrStatus>>,
        conversation: std::sync::Mutex<Option<thegn_core::forge::model::PrConversation>>,
        calls: std::sync::Mutex<Vec<String>>,
        status_sequence: std::sync::Mutex<Vec<PrStatus>>,
        rerun_count: std::sync::atomic::AtomicU32,
        rerun_error: std::sync::atomic::AtomicBool,
        merge_error: std::sync::Mutex<Option<ForgeError>>,
        merge_head_after: std::sync::Mutex<Option<String>>,
        confirm_direct: std::sync::atomic::AtomicBool,
    }
    impl FakeForge {
        fn new(pr: Option<PrStatus>) -> Self {
            FakeForge {
                pr: std::sync::Mutex::new(pr),
                conversation: std::sync::Mutex::new(Some(Default::default())),
                calls: Default::default(),
                status_sequence: Default::default(),
                rerun_count: std::sync::atomic::AtomicU32::new(0),
                rerun_error: std::sync::atomic::AtomicBool::new(false),
                merge_error: Default::default(),
                merge_head_after: Default::default(),
                confirm_direct: std::sync::atomic::AtomicBool::new(true),
            }
        }
        fn with_conversation(self, conversation: thegn_core::forge::model::PrConversation) -> Self {
            *self.conversation.lock().unwrap() = Some(conversation);
            self
        }
        fn fail_conversation(self) -> Self {
            *self.conversation.lock().unwrap() = None;
            self
        }
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
        fn with_rerun_count(self, count: u32) -> Self {
            self.rerun_count
                .store(count, std::sync::atomic::Ordering::SeqCst);
            self
        }
        fn with_status_sequence(self, sequence: Vec<PrStatus>) -> Self {
            *self.status_sequence.lock().unwrap() = sequence;
            self
        }
        fn with_rerun_error(self) -> Self {
            self.rerun_error
                .store(true, std::sync::atomic::Ordering::SeqCst);
            self
        }
        fn with_merge_error(self, error: ForgeError) -> Self {
            *self.merge_error.lock().unwrap() = Some(error);
            self
        }
        fn with_merge_head_after(self, head: &str) -> Self {
            *self.merge_head_after.lock().unwrap() = Some(head.into());
            self
        }
        fn without_direct_confirmation(self) -> Self {
            self.confirm_direct
                .store(false, std::sync::atomic::Ordering::SeqCst);
            self
        }
        fn record(&self, s: impl Into<String>) {
            self.calls.lock().unwrap().push(s.into());
        }
    }
    impl thegn_core::seam::Probe for FakeForge {
        fn probe(&self) -> thegn_core::seam::ProbeReport {
            thegn_core::seam::ProbeReport::new(
                "forge",
                "fake",
                thegn_core::seam::Availability::Ready,
            )
        }
    }
    impl Forge for FakeForge {
        fn id(&self) -> &'static str {
            "github"
        }
        fn caps(&self) -> thegn_core::forge::ForgeCaps {
            thegn_core::forge::ForgeCaps::ALL
        }
        fn repo_ref(&self, _: &GitLoc) -> Option<thegn_core::forge::RepoRef> {
            None
        }
        fn pr_status(&self, _: &GitLoc, pr: PrRef) -> Result<PrStatus, ForgeError> {
            self.record(format!("pr_status {pr:?}"));
            let status = {
                let mut sequence = self.status_sequence.lock().unwrap();
                (!sequence.is_empty()).then(|| sequence.remove(0))
            };
            if let Some(status) = status {
                return Ok(status);
            }
            self.pr.lock().unwrap().clone().ok_or(ForgeError::NoPr)
        }
        fn pr_list(
            &self,
            _: &GitLoc,
            _: usize,
        ) -> Result<Vec<thegn_core::forge::model::PrHeader>, ForgeError> {
            Ok(vec![])
        }
        fn merge_pr(
            &self,
            _: &GitLoc,
            pr: PrRef,
            method: MergeMethod,
            delete_branch: bool,
            expected_head: Option<&str>,
            auto: bool,
        ) -> Result<(), ForgeError> {
            self.record(format!(
                "merge {pr:?} {method:?} del={delete_branch} head={expected_head:?} auto={auto}"
            ));
            if let Some(head) = self.merge_head_after.lock().unwrap().take()
                && let Some(status) = self.pr.lock().unwrap().as_mut()
            {
                status.head_ref_oid = head;
            }
            if let Some(error) = self.merge_error.lock().unwrap().clone() {
                return Err(error);
            }
            if !auto
                && self
                    .confirm_direct
                    .load(std::sync::atomic::Ordering::SeqCst)
                && let Some(status) = self.pr.lock().unwrap().as_mut()
            {
                status.state = "MERGED".into();
            }
            Ok(())
        }
        fn rerun_failed(&self, _: &GitLoc, pr: PrRef) -> Result<u32, ForgeError> {
            self.record(format!("rerun {pr:?}"));
            if self.rerun_error.load(std::sync::atomic::Ordering::SeqCst) {
                Err(ForgeError::Offline)
            } else {
                Ok(self.rerun_count.load(std::sync::atomic::Ordering::SeqCst))
            }
        }
        fn whoami(&self, _: &GitLoc) -> Result<String, ForgeError> {
            self.record("whoami");
            Ok("me".into())
        }
        fn conversation(
            &self,
            _: &GitLoc,
            _: &thegn_core::forge::RepoRef,
            _: u64,
        ) -> Result<thegn_core::forge::model::PrConversation, ForgeError> {
            self.record("conversation");
            self.conversation
                .lock()
                .unwrap()
                .clone()
                .ok_or(ForgeError::Offline)
        }
        fn pr_diff(
            &self,
            _: &GitLoc,
            _: PrRef,
        ) -> Result<thegn_core::forge::model::PrDiff, ForgeError> {
            self.record("pr_diff");
            Ok(Default::default())
        }
    }

    fn green_pr() -> PrStatus {
        let mut pr = fetched(vec![]).pr;
        pr.review_decision = Some("APPROVED".into());
        pr
    }

    fn requested_pr() -> PrStatus {
        let mut pr = fetched(vec![]).pr;
        pr.review_decision = Some("CHANGES_REQUESTED".into());
        pr
    }

    fn failing_ci_pr(head: &str) -> PrStatus {
        let mut pr = fetched(vec![]).pr;
        pr.head_ref_oid = head.into();
        pr.status_check_rollup = vec![thegn_core::forge::model::CheckRun {
            name: "test".into(),
            status: "COMPLETED".into(),
            conclusion: Some("FAILURE".into()),
            state: None,
            workflow_name: None,
            details_url: None,
            started_at: None,
            completed_at: None,
        }];
        pr
    }

    fn review_conversation(body: &str) -> thegn_core::forge::model::PrConversation {
        use thegn_core::forge::model::{PrComment, PrConversation, ReviewThread};
        PrConversation {
            threads: vec![ReviewThread {
                id: "thread-7".into(),
                path: "src/lib.rs".into(),
                line: Some(42),
                comments: vec![PrComment {
                    id: "comment-1".into(),
                    author: "reviewer".into(),
                    body: body.into(),
                    created_at: "2026-08-30T00:00:00Z".into(),
                }],
                ..ReviewThread::default()
            }],
            ..PrConversation::default()
        }
    }

    fn temp_db(name: &str) -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_at(&dir.path().join(format!("{name}.db"))).unwrap();
        (dir, db)
    }

    fn enqueue_merge_test_row(dir: &tempfile::TempDir, db: &Db) -> PrQueueRow {
        let item = saved_bound_item(dir, db, item());
        db.update_pr_status(&item.key, "ready", None, None, Some("abc"))
            .unwrap();
        db.list_pr_queue()
            .unwrap()
            .into_iter()
            .find(|row| row.key == item.key)
            .unwrap()
    }

    #[test]
    fn compose_bounds_large_pr_log_and_review_threads() {
        let mut config = cfg();
        config.own_prs_only = false;
        let mut pr = fetched(vec![thread("reviewer", "src/lib.rs", Some(1), false)]);
        pr.pr.title = "P".repeat(40_000);
        let ci = compose_with_log(
            &config,
            TaskKind::PrCiFailure,
            "/w/feat",
            &item(),
            &pr,
            &Blocker::Ci(vec!["test".into()]),
            Some(&"L".repeat(40_000)),
        )
        .unwrap()
        .1;
        assert!(ci.len() <= thegn_core::agent_task::MAX_RENDERED_PROMPT_BYTES);
        assert!(ci.contains("task data omitted"));

        let mut review_pr = fetched(vec![thread("reviewer", "src/lib.rs", Some(1), false)]);
        review_pr.threads[0].snippet = "R".repeat(40_000);
        let (_, review) = compose_with_log(
            &config,
            TaskKind::PrReview,
            "/w/feat",
            &item(),
            &review_pr,
            &Blocker::ChangesRequested,
            None,
        )
        .unwrap();
        assert!(review.contains("src/lib.rs"));
        assert!(review.contains("task data omitted"));
        assert!(review.len() <= thegn_core::agent_task::MAX_RENDERED_PROMPT_BYTES);
    }

    #[test]
    fn own_pr_denial_never_reruns_ci_spends_budget_or_launches_an_agent() {
        for mismatched_number in [false, true] {
            use crate::pr_authorship::tests::Fixture;
            use std::sync::atomic::Ordering;
            let fixture = Fixture::new();
            let path = fixture.dir.path().to_str().unwrap();
            let db = Db::open_at(&fixture.dir.path().join("queue.db")).unwrap();
            db.enqueue_bound_pr(
                path,
                7,
                Some(path),
                "fixture",
                "main",
                "github",
                "https://github.com/organization/project/pull/7",
            )
            .unwrap();
            let sentinel = fixture.dir.path().join("agent-must-not-run");
            let mut cfg = cfg();
            cfg.watch = vec![PrWatchKind::Ci];
            cfg.agent_command = format!("touch {}", sentinel.display());
            cfg.own_prs_only = true;
            let mut proof = fixture.proof.clone();
            if mismatched_number {
                proof.number = 8;
                proof.pr_id = "PR_8".into();
            } else {
                proof.viewer.id = "U_other".into();
            }
            let mut forge = fixture.forge(vec![proof]);
            if mismatched_number {
                forge.pr.number = 8;
                forge.pr.url = "https://github.com/organization/project/pull/8".into();
            }
            forge.pr.status_check_rollup = vec![
                serde_json::from_value(serde_json::json!({"name":"test", "conclusion":"FAILURE"}))
                    .unwrap(),
            ];
            let items = db
                .list_pr_queue()
                .unwrap()
                .iter()
                .map(PrItem::from)
                .collect();
            let out = drive_queue(
                &cfg,
                &Config::default(),
                &forge,
                fixture.dir.path(),
                &db,
                items,
                |_| {},
            );
            let expected_warning = if mismatched_number {
                "identity"
            } else {
                "own-PR automation held"
            };
            assert!(
                out.warnings.iter().any(|s| s.contains(expected_warning)),
                "{out:?}"
            );
            assert_eq!(
                forge.proof_calls.load(Ordering::SeqCst),
                usize::from(!mismatched_number)
            );
            assert_eq!(forge.reruns.load(Ordering::SeqCst), 0);
            assert_eq!(db.list_pr_queue().unwrap()[0].agent_attempts, 0);
            assert!(!sentinel.exists());
        }
    }

    #[test]
    fn after_agent_identity_change_holds_the_row_needs_human() {
        use crate::pr_authorship::tests::Fixture;
        use std::sync::atomic::Ordering;

        let fixture = Fixture::new();
        let path = fixture.dir.path().to_str().unwrap();
        let db = &fixture.db;
        db.enqueue_bound_pr(
            path,
            7,
            Some(path),
            "fixture",
            "main",
            "github",
            &fixture.pr.url,
        )
        .unwrap();
        let row = db.list_pr_queue().unwrap().remove(0);
        db.update_pr_status(
            &row.key,
            PrqStatus::BlockedCi.as_str(),
            None,
            None,
            Some(&fixture.pr.head_ref_oid),
        )
        .unwrap();
        db.set_pr_agent_attempts(&row.key, 1).unwrap();
        let row = db.list_pr_queue().unwrap().remove(0);
        let sentinel = fixture.dir.path().join("agent-ran");
        let mut config = cfg();
        config.watch = vec![PrWatchKind::Ci];
        config.agent_command = format!("touch {}", sentinel.display());
        let mut foreign = fixture.proof.clone();
        foreign.viewer.id = "U_someone_else".into();
        let mut forge = fixture.forge(vec![fixture.proof.clone(), fixture.proof.clone(), foreign]);
        forge.pr.status_check_rollup = vec![
            serde_json::from_value(serde_json::json!({"name":"test", "conclusion":"FAILURE"}))
                .unwrap(),
        ];

        // A failed-CI row with an existing attempt bypasses the zero-attempt CI rerun branch at
        // drive_queue's `if kind == PrCiFailure && attempts == 0` check and reaches DispatchAgent.
        let full = uncontained_config();
        let out = drive_queue(
            &config,
            &full,
            &forge,
            fixture.dir.path(),
            db,
            vec![PrItem::from(&row)],
            |_| {},
        );

        let held = db.list_pr_queue().unwrap().remove(0);
        assert!(
            sentinel.exists(),
            "the agent should run before post-agent recheck"
        );
        assert_eq!(
            forge.proof_calls.load(Ordering::SeqCst),
            3,
            "expected acquire, pre-dispatch revalidate, and post-agent proof calls"
        );
        assert_eq!(out.needs_human, vec![7]);
        assert!(
            out.warnings
                .iter()
                .any(|warning| { warning.contains("held after the agent ran") })
        );
        assert_eq!(held.status, PrqStatus::NeedsHuman.as_str());
        assert!(
            held.detail
                .as_deref()
                .is_some_and(|detail| detail.contains("held after the agent ran"))
        );
    }

    #[test]
    fn fake_forge_drives_a_green_pr_to_auto_merge() {
        let (_dir, db) = temp_db("prq-auto");
        db.enqueue_bound_pr(
            "/repo",
            7,
            None,
            "feat",
            "main",
            "github",
            "https://github.com/me/repo/pull/7",
        )
        .unwrap();
        let row = db.list_pr_queue().unwrap().remove(0);
        let forge = FakeForge::new(Some(green_pr()));
        let cfg = cfg(); // merge_mode = auto_merge (default)
        let out = drive_queue(
            &cfg,
            &Config::default(),
            &forge,
            Path::new("/repo"),
            &db,
            vec![PrItem::from(&row)],
            |_| {},
        );
        assert!(out.merged.is_empty(), "{out:?}");
        assert_eq!(out.auto_merge_armed, vec![7], "{out:?}");
        let calls = forge.calls();
        assert!(
            !calls.iter().any(|c| c == "whoami"),
            "namespace identity is never an authorization source: {calls:?}"
        );
        assert!(
            calls.iter().any(|c| c.starts_with("pr_status Number(7)")),
            "{calls:?}"
        );
        assert!(
            calls
                .iter()
                .any(|c| c.starts_with("merge Number(7)") && c.ends_with("auto=true")),
            "auto-merge is merge_pr with auto=true: {calls:?}"
        );
    }

    #[test]
    fn fake_forge_direct_mode_merges_now() {
        let (_dir, db) = temp_db("prq-direct");
        db.enqueue_bound_pr(
            "/repo",
            7,
            None,
            "feat",
            "main",
            "github",
            "https://github.com/me/repo/pull/7",
        )
        .unwrap();
        let row = db.list_pr_queue().unwrap().remove(0);
        let forge = FakeForge::new(Some(green_pr()));
        let mut cfg = cfg();
        cfg.merge_mode = thegn_core::config::PrMergeMode::Thegn;
        let out = drive_queue(
            &cfg,
            &Config::default(),
            &forge,
            Path::new("/repo"),
            &db,
            vec![PrItem::from(&row)],
            |_| {},
        );
        assert_eq!(out.merged, vec![7], "{out:?}");
        assert!(
            forge
                .calls()
                .iter()
                .any(|c| c.starts_with("merge Number(7)") && c.ends_with("auto=false")),
            "{:?}",
            forge.calls()
        );
    }

    #[test]
    fn forge_mismatch_is_held_before_any_forge_call() {
        let (_dir, db) = temp_db("prq-forge-mismatch");
        db.enqueue_pr("/repo", 7, Some("/w/feat"), "feat", "main", "gitlab")
            .unwrap();
        let row = db.list_pr_queue().unwrap().remove(0);
        let forge = FakeForge::new(Some(green_pr()));
        let mut cfg = cfg();
        cfg.merge_mode = thegn_core::config::PrMergeMode::Thegn;
        let mut steps = Vec::new();
        let out = drive_queue(
            &cfg,
            &Config::default(),
            &forge,
            Path::new("/repo"),
            &db,
            vec![PrItem::from(&row)],
            |step| steps.push(step.status.to_string()),
        );
        let row = db.list_pr_queue().unwrap().remove(0);
        assert!(forge.calls().is_empty());
        assert_eq!(out.needs_human, vec![7]);
        assert!(steps.contains(&PrqStatus::NeedsHuman.as_str().to_string()));
        assert_eq!(row.status, PrqStatus::NeedsHuman.as_str());
        assert_eq!(row.agent_attempts, 0);
        let detail = row.detail.unwrap();
        assert!(
            detail.contains("gitlab") && detail.contains("github"),
            "{detail}"
        );
        assert!(detail.contains("re-add"), "{detail}");
    }

    #[test]
    fn successful_ci_rerun_is_bounded_across_database_reopen_then_dispatches() {
        let dir = tempfile::tempdir().unwrap();
        let checkout = dir.path().join("checkout");
        std::fs::create_dir(&checkout).unwrap();
        git_repo_on_branch(&checkout, "feat");
        let checkout = checkout.canonicalize().unwrap();
        let checkout_s = checkout.to_string_lossy().into_owned();
        let path = dir.path().join("pr-rerun.db");
        let mut cfg = cfg();
        cfg.watch = vec![PrWatchKind::Ci];
        cfg.agent_command = "true {prompt}".into();
        cfg.own_prs_only = false;
        let forge = FakeForge::new(Some(failing_ci_pr("abc"))).with_rerun_count(1);
        {
            let db = Db::open_at(&path).unwrap();
            db.enqueue_bound_pr(
                "/repo",
                7,
                Some(&checkout_s),
                "feat",
                "main",
                "github",
                "https://github.com/me/repo/pull/7",
            )
            .unwrap();
            let out = drive_queue(
                &cfg,
                &uncontained_config(),
                &forge,
                Path::new("/repo"),
                &db,
                vec![PrItem::from(&db.list_pr_queue().unwrap()[0])],
                |_| {},
            );
            assert_eq!(
                out.blocked,
                vec![7],
                "the successful rerun gets a fresh CI pass"
            );
            assert_eq!(
                forge
                    .calls()
                    .iter()
                    .filter(|call| call.starts_with("rerun"))
                    .count(),
                1
            );

            let mut pending = failing_ci_pr("abc");
            pending.status_check_rollup[0].status = "IN_PROGRESS".into();
            pending.status_check_rollup[0].conclusion = None;
            *forge.pr.lock().unwrap() = Some(pending);
            let row = db.list_pr_queue().unwrap().remove(0);
            let out = drive_queue(
                &cfg,
                &uncontained_config(),
                &forge,
                Path::new("/repo"),
                &db,
                vec![PrItem::from(&row)],
                |_| {},
            );
            assert_eq!(
                out.blocked,
                vec![7],
                "in-flight CI remains blocked while the rerun is pending"
            );
            assert!(out.needs_human.is_empty());
            assert_eq!(db.list_pr_queue().unwrap()[0].agent_attempts, 0);
            assert_eq!(
                forge
                    .calls()
                    .iter()
                    .filter(|call| call.starts_with("rerun"))
                    .count(),
                1,
                "in-flight checks wait without spending another rerun or agent attempt"
            );

            *forge.pr.lock().unwrap() = Some(failing_ci_pr("abc"));
        }

        let db = Db::open_at(&path).unwrap();
        let row = db.list_pr_queue().unwrap().remove(0);
        assert_eq!(row.ci_rerun_head.as_deref(), Some("abc"));
        let out = drive_queue(
            &cfg,
            &uncontained_config(),
            &forge,
            Path::new("/repo"),
            &db,
            vec![PrItem::from(&row)],
            |_| {},
        );
        assert!(out.blocked.is_empty());
        assert_eq!(
            forge
                .calls()
                .iter()
                .filter(|call| call.starts_with("rerun"))
                .count(),
            1,
            "reopening and repeated deterministic failure cannot repeat the mutation"
        );
        assert_eq!(db.list_pr_queue().unwrap()[0].agent_attempts, 1);
    }

    #[test]
    fn ambiguous_rerun_response_keeps_allowance_spent() {
        let (dir, db) = temp_db("prq-rerun-ambiguous");
        let mut cfg = cfg();
        cfg.watch = vec![PrWatchKind::Ci];
        cfg.pause_on_foreign_push = false;
        cfg.agent_command = "true {prompt}".into();
        cfg.own_prs_only = false;
        let (checkout, row) =
            enqueue_pr_with_checkout(&dir, &db, 7, "feat", "https://github.com/me/repo/pull/7");
        let forge = FakeForge::new(Some(failing_ci_pr("abc"))).with_rerun_error();
        let out = drive_queue(
            &cfg,
            &Config::default(),
            &forge,
            &checkout,
            &db,
            vec![row.clone()],
            |_| {},
        );
        assert!(
            out.warnings
                .iter()
                .any(|warning| warning.contains("ambiguous"))
        );
        assert_eq!(
            db.list_pr_queue().unwrap()[0].ci_rerun_head.as_deref(),
            Some("abc")
        );
        assert_eq!(
            forge
                .calls()
                .iter()
                .filter(|call| call.starts_with("rerun"))
                .count(),
            1
        );
    }

    #[test]
    fn a_new_pr_head_rearms_the_ci_rerun_allowance() {
        let (dir, db) = temp_db("prq-rerun-head-change");
        let mut cfg = cfg();
        cfg.watch = vec![PrWatchKind::Ci];
        cfg.pause_on_foreign_push = false;
        cfg.agent_command = "true {prompt}".into();
        cfg.own_prs_only = false;
        let (checkout, row) =
            enqueue_pr_with_checkout(&dir, &db, 7, "feat", "https://github.com/me/repo/pull/7");
        let forge = FakeForge::new(Some(failing_ci_pr("abc"))).with_rerun_count(1);
        drive_queue(
            &cfg,
            &Config::default(),
            &forge,
            &checkout,
            &db,
            vec![row.clone()],
            |_| {},
        );
        *forge.pr.lock().unwrap() = Some(failing_ci_pr("def"));
        let row = db.list_pr_queue().unwrap().remove(0);
        drive_queue(
            &cfg,
            &Config::default(),
            &forge,
            &checkout,
            &db,
            vec![PrItem::from(&row)],
            |_| {},
        );
        assert_eq!(
            forge
                .calls()
                .iter()
                .filter(|call| call.starts_with("rerun"))
                .count(),
            2
        );
        assert_eq!(
            db.list_pr_queue().unwrap()[0].ci_rerun_head.as_deref(),
            Some("def")
        );
    }

    #[test]
    fn concurrent_drivers_can_only_reserve_one_rerun_for_a_head() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pr-rerun-race.db");
        let db = Db::open_at(&path).unwrap();
        let (checkout, row) =
            enqueue_pr_with_checkout(&dir, &db, 7, "feat", "https://github.com/me/repo/pull/7");
        drop(db);

        let forge =
            std::sync::Arc::new(FakeForge::new(Some(failing_ci_pr("abc"))).with_rerun_count(1));
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let workers: Vec<_> = (0..2)
            .map(|_| {
                let path = path.clone();
                let forge = forge.clone();
                let barrier = barrier.clone();
                let row = row.clone();
                let checkout = checkout.clone();
                std::thread::spawn(move || {
                    let db = Db::open_at(&path).unwrap();
                    let mut cfg = cfg();
                    cfg.watch = vec![PrWatchKind::Ci];
                    cfg.agent_command = "true {prompt}".into();
                    cfg.own_prs_only = false;
                    barrier.wait();
                    drive_queue(
                        &cfg,
                        &Config::default(),
                        forge.as_ref(),
                        &checkout,
                        &db,
                        vec![row.clone()],
                        |_| {},
                    );
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(
            forge
                .calls()
                .iter()
                .filter(|call| call.starts_with("rerun"))
                .count(),
            1,
            "driver workers racing the same persisted head share one reservation"
        );
    }

    #[test]
    fn fake_forge_fetch_failure_is_a_warning_not_a_verdict() {
        let (dir, db) = temp_db("prq-gone");
        let forge = FakeForge::new(None); // every fetch → NoPr
        let out = drive_queue(
            &cfg(),
            &Config::default(),
            &forge,
            Path::new("/repo"),
            &db,
            vec![saved_bound_item(&dir, &db, item())],
            |_| {},
        );
        assert!(out.merged.is_empty());
        assert!(
            !forge.calls().iter().any(|c| c.starts_with("merge")),
            "never merges what it could not fetch: {:?}",
            forge.calls()
        );
    }

    #[test]
    fn fetch_failure_never_writes_status() {
        let (dir, db) = temp_db("prq-hold");
        let saved = saved_bound_item(
            &dir,
            &db,
            PrItem {
                key: String::new(),
                number: 8754,
                pr_url: Some("https://github.com/me/repo/pull/8754".into()),
                ..item()
            },
        );
        db.update_pr_status(
            &saved.key,
            PrqStatus::NeedsHuman.as_str(),
            Some("ci"),
            Some("held by a human"),
            None,
        )
        .unwrap();

        let conn = rusqlite::Connection::open(dir.path().join("prq-hold.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE status_writes(n INTEGER);
             CREATE TRIGGER count_status_writes AFTER UPDATE OF status ON pr_queue
             BEGIN INSERT INTO status_writes VALUES (1); END;",
        )
        .unwrap();

        drive_queue(
            &cfg(),
            &Config::default(),
            &FakeForge::new(None),
            Path::new("/repo"),
            &db,
            vec![saved.clone()],
            |_| {},
        );

        let status_writes = conn
            .query_row("SELECT COUNT(*) FROM status_writes", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap();
        assert_eq!(
            status_writes, 0,
            "fetch failure must not issue UPDATE OF status"
        );
        let row = db
            .list_pr_queue()
            .unwrap()
            .into_iter()
            .find(|row| row.key == saved.key)
            .unwrap();
        assert_eq!(row.status, PrqStatus::NeedsHuman.as_str());
        assert!(row.detail.as_deref().unwrap().starts_with("PR #8754: "));
    }

    #[test]
    fn explicit_review_watch_is_required_for_deep_fetch() {
        let (dir, db) = temp_db("prq-review-filter");
        let forge = FakeForge::new(Some(requested_pr()))
            .with_conversation(review_conversation("rename this"));
        let mut cfg = cfg();
        cfg.watch = vec![PrWatchKind::Ci];
        drive_queue(
            &cfg,
            &Config::default(),
            &forge,
            Path::new("/repo"),
            &db,
            vec![saved_bound_item(&dir, &db, item())],
            |_| {},
        );
        assert!(
            !forge.calls().iter().any(|call| call == "conversation"),
            "unwatched review data must not be fetched: {:?}",
            forge.calls()
        );
        assert!(db.list_review_tasks().unwrap().is_empty());
    }

    #[test]
    fn per_thread_task_suppresses_the_generic_review_agent() {
        let (dir, db) = temp_db("prq-review-dedupe");
        let forge = FakeForge::new(Some(requested_pr()))
            .with_conversation(review_conversation("rename this"));
        let mut cfg = cfg();
        cfg.agent_command = "true {prompt}".into();
        let mut statuses = Vec::new();
        let out = drive_queue(
            &cfg,
            &Config::default(),
            &forge,
            Path::new("/repo"),
            &db,
            vec![saved_bound_item(&dir, &db, item())],
            |step| statuses.push(step.status.to_string()),
        );
        assert_eq!(db.list_review_tasks().unwrap().len(), 1);
        assert_eq!(out.review_events.len(), 1);
        assert_eq!(statuses, vec![PrqStatus::BlockedReview.as_str()]);
        assert!(
            !statuses.contains(&PrqStatus::AgentRunning.as_str().to_string()),
            "aggregate PrReview must not launch beside a thread task"
        );

        let unchanged = drive_queue(
            &cfg,
            &Config::default(),
            &forge,
            Path::new("/repo"),
            &db,
            vec![saved_bound_item(&dir, &db, item())],
            |_| {},
        );
        assert!(unchanged.review_events.is_empty());

        let revised_forge = FakeForge::new(Some(requested_pr()))
            .with_conversation(review_conversation("a newer comment"));
        let revised = drive_queue(
            &cfg,
            &Config::default(),
            &revised_forge,
            Path::new("/repo"),
            &db,
            vec![saved_bound_item(&dir, &db, item())],
            |_| {},
        );
        assert_eq!(revised.review_events.len(), 1);
    }

    #[test]
    fn by_number_review_task_keeps_missing_worktree_human_only() {
        let (dir, db) = temp_db("prq-review-by-number");
        let forge = FakeForge::new(Some(requested_pr()))
            .with_conversation(review_conversation("rename this"));
        let mut cfg = cfg();
        cfg.agent_command = "true {prompt}".into();
        let mut number_only = item();
        number_only.worktree = None;
        let mut statuses = Vec::new();
        let out = drive_queue(
            &cfg,
            &Config::default(),
            &forge,
            Path::new("/repo"),
            &db,
            vec![saved_bound_item(&dir, &db, number_only)],
            |step| statuses.push(step.status.to_string()),
        );
        let task = db.list_review_tasks().unwrap().pop().unwrap();
        assert!(task.worktree_path.is_empty());
        assert_eq!(out.review_events.len(), 1);
        assert_eq!(out.review_events[0].0.worktree_path, "");
        assert!(!statuses.contains(&PrqStatus::AgentRunning.as_str().to_string()));
    }

    #[test]
    fn transient_review_failure_keeps_the_durable_task_stale() {
        let (dir, db) = temp_db("prq-review-stale");
        let first = FakeForge::new(Some(requested_pr()))
            .with_conversation(review_conversation("first revision"));
        drive_queue(
            &cfg(),
            &Config::default(),
            &first,
            Path::new("/repo"),
            &db,
            vec![saved_bound_item(&dir, &db, item())],
            |_| {},
        );
        let before = db.list_review_tasks().unwrap();
        assert_eq!(before.len(), 1);

        let failed = FakeForge::new(Some(requested_pr())).fail_conversation();
        let out = drive_queue(
            &cfg(),
            &Config::default(),
            &failed,
            Path::new("/repo"),
            &db,
            vec![saved_bound_item(&dir, &db, item())],
            |_| {},
        );
        assert_eq!(db.list_review_tasks().unwrap(), before);
        assert!(out.warnings.iter().any(|warning| warning.contains("stale")));
    }

    #[test]
    fn unsupported_platform_holds_pr_before_rerun_reservation_or_agent_attempt() {
        let (dir, db) = temp_db("prq-unsupported-platform");
        let sentinel = dir.path().join("agent-was-run");
        let mut config = cfg();
        config.watch = vec![PrWatchKind::Ci];
        config.own_prs_only = false;
        config.agent_command = format!("touch {}", sentinel.display());
        let (checkout, row) =
            enqueue_pr_with_checkout(&dir, &db, 7, "feat", "https://github.com/me/repo/pull/7");
        let forge = FakeForge::new(Some(failing_ci_pr("abc"))).with_rerun_count(1);
        let unsupported = crate::agent_run::force_headless_unsupported();
        let out = drive_queue(
            &config,
            &Config::default(),
            &forge,
            &checkout,
            &db,
            vec![row.clone()],
            |_| {},
        );
        drop(unsupported);

        assert_eq!(out.blocked, vec![7]);
        assert!(!forge.calls().iter().any(|call| call.starts_with("rerun")));
        let row = db.list_pr_queue().unwrap().remove(0);
        assert_eq!(row.ci_rerun_head, None);
        assert_eq!(row.agent_attempts, 0);
        assert_eq!(row.status, "blocked_ci");
        assert!(!sentinel.exists());
        assert!(row.detail.unwrap().contains("THE-235"));
    }

    #[test]
    fn invalid_prompt_template_is_a_durable_pr_refusal_without_attempt_spend() {
        let (dir, db) = temp_db("prq-invalid-prompt");
        let mut config = cfg();
        config.watch = vec![PrWatchKind::Ci];
        config.own_prs_only = false;
        config.prompts.ci_failure = "{nope}".into();
        config.agent_command = "true {prompt}".into();
        let (checkout, row) =
            enqueue_pr_with_checkout(&dir, &db, 7, "feat", "https://github.com/me/repo/pull/7");
        let forge = FakeForge::new(Some(failing_ci_pr("abc"))).with_rerun_count(0);
        let out = drive_queue(
            &config,
            &uncontained_config(),
            &forge,
            &checkout,
            &db,
            vec![row.clone()],
            |_| {},
        );
        assert_eq!(out.blocked, vec![7]);
        assert!(
            out.warnings
                .iter()
                .any(|warning| warning.contains("template for pr_ci_failure is invalid"))
        );
        let row = db.list_pr_queue().unwrap().remove(0);
        assert_eq!(row.status, "blocked_ci");
        assert!(row.detail.unwrap().contains("template"));
        assert_eq!(row.agent_attempts, 0);
    }

    #[test]
    fn isolation_refusal_is_a_durable_blocked_ci_without_attempt_spend() {
        let (dir, db) = temp_db("prq-isolation-refusal");
        let mut config = cfg();
        config.watch = vec![PrWatchKind::Ci];
        config.own_prs_only = false;
        config.agent_sandbox = true;
        let sentinel = dir.path().join("agent-was-run");
        config.agent_command = format!("touch {}", sentinel.display());
        let (checkout, row) =
            enqueue_pr_with_checkout(&dir, &db, 7, "feat", "https://github.com/me/repo/pull/7");
        std::fs::write(
            checkout.join(".thegn.toml"),
            "[sandbox\nnetwork = \"none\"\n",
        )
        .unwrap();
        let forge = FakeForge::new(Some(failing_ci_pr("abc"))).with_rerun_count(0);

        let out = drive_queue(
            &config,
            &Config::default(),
            &forge,
            &checkout,
            &db,
            vec![row],
            |_| {},
        );

        assert_eq!(out.blocked, vec![7]);
        let row = db.list_pr_queue().unwrap().remove(0);
        assert_eq!(row.status, "blocked_ci");
        assert_eq!(row.agent_attempts, 0);
        assert!(row.detail.unwrap().contains(".thegn.toml"));
        assert!(!sentinel.exists());
    }

    #[test]
    fn identity_change_between_read_and_merge_holds_without_merge() {
        let (_dir, db) = temp_db("prq-read-effect-identity");
        db.enqueue_bound_pr(
            "/repo",
            7,
            None,
            "feature",
            "main",
            "github",
            "https://github.com/me/repo/pull/7",
        )
        .unwrap();
        let row = db.list_pr_queue().unwrap().remove(0);
        let mut changed = green_pr();
        changed.url = "https://github.com/other/repo/pull/7".into();
        let forge =
            FakeForge::new(Some(green_pr())).with_status_sequence(vec![green_pr(), changed]);
        let mut config = cfg();
        config.merge_mode = thegn_core::config::PrMergeMode::Thegn;
        let out = drive_queue(
            &config,
            &Config::default(),
            &forge,
            Path::new("/repo"),
            &db,
            vec![PrItem::from(&row)],
            |_| {},
        );
        assert!(out.needs_human.contains(&7));
        assert!(!forge.calls().iter().any(|call| call.starts_with("merge")));
    }

    #[test]
    fn moved_head_between_read_and_rerun_spends_no_allowance() {
        let (dir, db) = temp_db("prq-read-effect-head");
        let (checkout, row) =
            enqueue_pr_with_checkout(&dir, &db, 7, "feat", "https://github.com/me/repo/pull/7");
        let original = failing_ci_pr("head-a");
        let mut moved = original.clone();
        moved.head_ref_oid = "head-b".into();
        let forge =
            FakeForge::new(Some(original.clone())).with_status_sequence(vec![original, moved]);
        let mut config = cfg();
        config.watch = vec![PrWatchKind::Ci];
        config.own_prs_only = false;
        config.agent_command = "true {prompt}".into();
        let out = drive_queue(
            &config,
            &Config::default(),
            &forge,
            &checkout,
            &db,
            vec![row.clone()],
            |_| {},
        );
        assert!(out.blocked.contains(&7));
        assert!(!forge.calls().iter().any(|call| call.starts_with("rerun")));
        let current = db.list_pr_queue().unwrap().remove(0);
        assert_eq!(current.ci_rerun_head, None);
        assert_eq!(current.agent_attempts, 0);
    }

    #[test]
    fn foreign_push_during_wait_persists_budget_refill_for_later_ci_failure() {
        let (dir, db) = temp_db("prq-wait-push-budget");
        let (checkout, initial_row) =
            enqueue_pr_with_checkout(&dir, &db, 7, "feat", "https://github.com/me/repo/pull/7");
        let key = initial_row.key;
        db.set_pr_agent_attempts(&key, 3).unwrap();
        db.update_pr_status(&key, "blocked_ci", Some("ci"), None, Some("head-a"))
            .unwrap();
        let mut config = cfg();
        config.watch = vec![PrWatchKind::Ci];
        config.own_prs_only = false;
        config.pause_on_foreign_push = false;
        config.agent_command = "true {prompt}".into();
        let mut pending = failing_ci_pr("head-b");
        pending.status_check_rollup[0].status = "IN_PROGRESS".into();
        pending.status_check_rollup[0].conclusion = None;
        let forge = FakeForge::new(Some(pending)).with_rerun_count(1);
        let row = db.list_pr_queue().unwrap().remove(0);
        drive_queue(
            &config,
            &Config::default(),
            &forge,
            &checkout,
            &db,
            vec![PrItem::from(&row)],
            |_| {},
        );
        assert_eq!(
            db.list_pr_queue().unwrap()[0].agent_attempts,
            0,
            "Wait persists the foreign-push refill"
        );
        *forge.pr.lock().unwrap() = Some(failing_ci_pr("head-b"));
        let row = db.list_pr_queue().unwrap().remove(0);
        drive_queue(
            &config,
            &Config::default(),
            &forge,
            &checkout,
            &db,
            vec![PrItem::from(&row)],
            |_| {},
        );
        assert_eq!(
            forge
                .calls()
                .iter()
                .filter(|call| call.starts_with("rerun"))
                .count(),
            1,
            "the later CI failure dispatches the rerun using the refilled budget"
        );
    }

    #[test]
    fn rebind_between_read_and_dispatch_holds_without_agent() {
        let (dir, db) = temp_db("prq-rebind-before-dispatch");
        let checkout = dir.path().join("checkout");
        std::fs::create_dir(&checkout).unwrap();
        git_repo_on_branch(&checkout, "feat");
        let checkout = checkout.canonicalize().unwrap();
        let checkout = checkout.to_string_lossy().into_owned();
        let sentinel = dir.path().join("agent-ran");
        db.enqueue_bound_pr(
            "/repo",
            7,
            Some(&checkout),
            "feat",
            "main",
            "github",
            "https://github.com/me/repo/pull/7",
        )
        .unwrap();
        let stale = PrItem::from(&db.list_pr_queue().unwrap()[0]);
        db.rebind_pr(
            &stale.key,
            &thegn_core::db::PrRebind {
                worktree: Some(checkout),
                branch: "feat".into(),
                base_branch: "main".into(),
                forge: "github".into(),
                pr_url: "https://github.com/me/repo/pull/7".into(),
            },
        )
        .unwrap();
        let mut config = cfg();
        config.watch = vec![PrWatchKind::Ci];
        config.agent_command = format!("touch {}", sentinel.display());
        config.own_prs_only = false;
        let forge = FakeForge::new(Some(failing_ci_pr("abc")));
        let out = drive_queue(
            &config,
            &Config::default(),
            &forge,
            Path::new("/repo"),
            &db,
            vec![stale],
            |_| {},
        );
        assert!(out.needs_human.contains(&7));
        assert_eq!(db.list_pr_queue().unwrap()[0].agent_attempts, 0);
        assert!(!sentinel.exists());
        assert!(!forge.calls().iter().any(|call| call.starts_with("rerun")));
    }

    #[test]
    fn same_number_other_project_never_reaches_effects() {
        let (_dir, db) = temp_db("prq-same-number-other-project");
        db.enqueue_bound_pr(
            "/repo",
            7,
            None,
            "feature",
            "main",
            "github",
            "https://github.com/other/project/pull/7",
        )
        .unwrap();
        let row = db.list_pr_queue().unwrap().remove(0);
        let mut config = cfg();
        config.merge_mode = thegn_core::config::PrMergeMode::Thegn;
        let mut originally_bound = green_pr();
        originally_bound.url = "https://github.com/other/project/pull/7".into();
        let forge = FakeForge::new(Some(green_pr()))
            .with_status_sequence(vec![originally_bound, green_pr()]);
        let out = drive_queue(
            &config,
            &Config::default(),
            &forge,
            Path::new("/repo"),
            &db,
            vec![PrItem::from(&row)],
            |_| {},
        );
        assert!(out.needs_human.contains(&7));
        assert!(
            forge
                .calls()
                .iter()
                .all(|call| !call.starts_with("merge") && !call.starts_with("rerun"))
        );
        assert_eq!(db.list_pr_queue().unwrap()[0].agent_attempts, 0);
    }

    #[test]
    fn remote_checkout_identity_uses_remote_git_paths_without_local_canonicalization() {
        let remote = GitLoc::Provider {
            control_prefix: vec!["fixture-provider".into()],
            path: "/workspace/project".into(),
        };
        assert_eq!(
            checkout_root_matches(&remote, " /workspace/project\n", "/workspace/project"),
            Some(true)
        );
        assert_eq!(
            checkout_root_matches(&remote, "/workspace/other", "/workspace/project"),
            Some(false)
        );
    }

    #[test]
    fn direct_merge_passes_the_reviewed_head() {
        let (dir, db) = temp_db("prq-reviewed-head");
        let row = enqueue_merge_test_row(&dir, &db);
        let forge = FakeForge::new(Some(green_pr()));
        let mut config = cfg();
        config.merge_mode = thegn_core::config::PrMergeMode::Thegn;
        let out = drive_queue(
            &config,
            &Config::default(),
            &forge,
            Path::new("/repo"),
            &db,
            vec![PrItem::from(&row)],
            |_| {},
        );
        assert_eq!(out.merged, vec![7]);
        assert!(forge.calls().iter().any(|call| {
            call.starts_with("merge Number(7)") && call.contains("head=Some(\"abc\")")
        }));
    }

    #[test]
    fn direct_merge_without_confirmation_is_requested_not_merged() {
        let (dir, db) = temp_db("prq-merge-requested");
        let row = enqueue_merge_test_row(&dir, &db);
        let forge = FakeForge::new(Some(green_pr())).without_direct_confirmation();
        let mut config = cfg();
        config.merge_mode = thegn_core::config::PrMergeMode::Thegn;
        let mut steps = Vec::new();
        let out = drive_queue(
            &config,
            &Config::default(),
            &forge,
            Path::new("/repo"),
            &db,
            vec![PrItem::from(&row)],
            |step| steps.push(step.status.to_owned()),
        );
        assert!(out.merged.is_empty());
        assert_eq!(out.merge_requested, vec![7]);
        assert!(steps.contains(&PrqStatus::Merging.as_str().to_owned()));
    }

    #[test]
    fn stale_head_refusal_is_refresh_required_not_needs_human() {
        let (dir, db) = temp_db("prq-stale-head");
        let row = enqueue_merge_test_row(&dir, &db);
        db.set_pr_agent_attempts(&row.key, 3).unwrap();
        let row = db.list_pr_queue().unwrap().remove(0);
        let forge = FakeForge::new(Some(green_pr()))
            .with_merge_error(ForgeError::Other("head mismatch".into()))
            .with_merge_head_after("new-head");
        let mut config = cfg();
        config.merge_mode = thegn_core::config::PrMergeMode::Thegn;
        let mut steps = Vec::new();
        let out = drive_queue(
            &config,
            &Config::default(),
            &forge,
            Path::new("/repo"),
            &db,
            vec![PrItem::from(&row)],
            |step| steps.push(step.status.to_owned()),
        );
        let row_after = db.list_pr_queue().unwrap().remove(0);
        assert_eq!(out.blocked, vec![7]);
        assert!(out.needs_human.is_empty());
        assert_eq!(steps.last().map(String::as_str), Some("watching"));
        assert_eq!(row_after.agent_attempts, 3);
        assert_eq!(row_after.status, PrqStatus::Watching.as_str());
        assert_eq!(row_after.last_head_oid.as_deref(), Some("abc"));
        let observed = forge.pr.lock().unwrap().clone().unwrap();
        assert_eq!(observed.state, "OPEN");
        assert_eq!(observed.head_ref_oid, "new-head");
        assert!(
            forge
                .calls()
                .iter()
                .any(|call| call.contains("head=Some(\"abc\")"))
        );

        let sentinel = dir.path().join("agent-must-not-run-after-foreign-push");
        let mut second_config = cfg();
        second_config.watch = vec![PrWatchKind::Ci];
        second_config.own_prs_only = false;
        second_config.pause_on_foreign_push = true;
        second_config.agent_command = format!("touch {}", sentinel.display());
        second_config.merge_mode = thegn_core::config::PrMergeMode::Thegn;
        let second_row = db.list_pr_queue().unwrap().remove(0);
        let failing_ci = FakeForge::new(Some(failing_ci_pr("new-head")));
        let second = drive_queue(
            &second_config,
            &Config::default(),
            &failing_ci,
            Path::new("/repo"),
            &db,
            vec![PrItem::from(&second_row)],
            |_| {},
        );
        let second_row_after = db.list_pr_queue().unwrap().remove(0);
        assert_eq!(second.needs_human, vec![7]);
        assert_eq!(second_row_after.status, PrqStatus::NeedsHuman.as_str());
        assert!(
            second_row_after
                .detail
                .unwrap()
                .contains("someone else pushed")
        );
        assert!(!sentinel.exists());
        assert!(
            !failing_ci
                .calls()
                .iter()
                .any(|call| call.starts_with("agent"))
        );
    }

    #[test]
    fn ambiguous_network_error_keeps_merging_and_reports_unknown() {
        let (dir, db) = temp_db("prq-merge-unknown");
        let row = enqueue_merge_test_row(&dir, &db);
        let forge = FakeForge::new(Some(green_pr())).with_merge_error(ForgeError::Offline);
        let mut config = cfg();
        config.merge_mode = thegn_core::config::PrMergeMode::Thegn;
        let mut steps = Vec::new();
        let out = drive_queue(
            &config,
            &Config::default(),
            &forge,
            Path::new("/repo"),
            &db,
            vec![PrItem::from(&row)],
            |step| steps.push(step.status.to_owned()),
        );
        assert!(out.merged.is_empty());
        assert_eq!(out.merge_requested, vec![7]);
        assert!(
            out.warnings
                .iter()
                .any(|warning| warning.contains("merge outcome unknown"))
        );
        assert_eq!(steps.last().map(String::as_str), Some("merging"));
    }

    #[test]
    fn armed_auto_merge_is_rearmed_each_pass_and_never_counted_merged() {
        let (dir, db) = temp_db("prq-auto-cancelled");
        let row = enqueue_merge_test_row(&dir, &db);
        let forge = FakeForge::new(Some(green_pr()));
        let config = cfg();
        let first = drive_queue(
            &config,
            &Config::default(),
            &forge,
            Path::new("/repo"),
            &db,
            vec![PrItem::from(&row)],
            |_| {},
        );
        let second = drive_queue(
            &config,
            &Config::default(),
            &forge,
            Path::new("/repo"),
            &db,
            vec![PrItem::from(&db.list_pr_queue().unwrap()[0])],
            |_| {},
        );
        assert_eq!(first.auto_merge_armed, vec![7]);
        assert_eq!(second.auto_merge_armed, vec![7]);
        assert!(first.merged.is_empty() && second.merged.is_empty());
        assert_eq!(
            forge
                .calls()
                .iter()
                .filter(|call| call.ends_with("auto=true"))
                .count(),
            2
        );
    }

    #[test]
    fn requested_merge_is_confirmed_by_a_later_poll() {
        let (dir, db) = temp_db("prq-later-confirmation");
        let row = enqueue_merge_test_row(&dir, &db);
        let forge = FakeForge::new(Some(green_pr())).without_direct_confirmation();
        let mut config = cfg();
        config.merge_mode = thegn_core::config::PrMergeMode::Thegn;
        let first = drive_queue(
            &config,
            &Config::default(),
            &forge,
            Path::new("/repo"),
            &db,
            vec![PrItem::from(&row)],
            |_| {},
        );
        assert_eq!(first.merge_requested, vec![7]);
        forge.pr.lock().unwrap().as_mut().unwrap().state = "MERGED".into();
        let second = drive_queue(
            &config,
            &Config::default(),
            &forge,
            Path::new("/repo"),
            &db,
            vec![PrItem::from(&db.list_pr_queue().unwrap()[0])],
            |_| {},
        );
        assert_eq!(second.merged, vec![7]);
        assert!(second.dropped.is_empty());
        let confirmed = db.list_pr_queue().unwrap().remove(0);
        assert_eq!(confirmed.status, "merged");
        assert_eq!(
            confirmed.detail.as_deref(),
            Some("merged (confirmed on a later poll)")
        );
    }

    #[test]
    fn later_poll_confirms_requested_merge_predicate() {
        let mut merging = item();
        merging.status = "merging".into();
        assert!(later_poll_confirms_requested_merge(
            &merging, "MERGED", "abc"
        ));

        let mut ready = merging.clone();
        ready.status = "ready".into();
        assert!(!later_poll_confirms_requested_merge(
            &ready, "MERGED", "abc"
        ));
        assert!(!later_poll_confirms_requested_merge(
            &merging, "CLOSED", "abc"
        ));
        assert!(!later_poll_confirms_requested_merge(
            &merging, "MERGED", "def"
        ));
        assert!(!later_poll_confirms_requested_merge(&merging, "MERGED", ""));
    }

    #[test]
    fn offline_merge_error_then_merged_poll_counts_merged() {
        let (dir, db) = temp_db("prq-offline-later-confirmation");
        let row = enqueue_merge_test_row(&dir, &db);
        let forge = FakeForge::new(Some(green_pr())).with_merge_error(ForgeError::Offline);
        let mut config = cfg();
        config.merge_mode = thegn_core::config::PrMergeMode::Thegn;
        let first = drive_queue(
            &config,
            &Config::default(),
            &forge,
            Path::new("/repo"),
            &db,
            vec![PrItem::from(&row)],
            |_| {},
        );
        assert_eq!(first.merge_requested, vec![7]);

        forge.pr.lock().unwrap().as_mut().unwrap().state = "MERGED".into();
        let second = drive_queue(
            &config,
            &Config::default(),
            &forge,
            Path::new("/repo"),
            &db,
            vec![PrItem::from(&db.list_pr_queue().unwrap()[0])],
            |_| {},
        );
        assert_eq!(second.merged, vec![7]);
        assert!(second.dropped.is_empty());
    }

    // Positive control: a merge at a different head is still reported as having left the queue.
    #[test]
    fn merged_at_another_head_after_request_is_dropped_not_merged() {
        let (dir, db) = temp_db("prq-other-head-merged");
        let row = enqueue_merge_test_row(&dir, &db);
        let forge = FakeForge::new(Some(green_pr())).without_direct_confirmation();
        let mut config = cfg();
        config.merge_mode = thegn_core::config::PrMergeMode::Thegn;
        let first = drive_queue(
            &config,
            &Config::default(),
            &forge,
            Path::new("/repo"),
            &db,
            vec![PrItem::from(&row)],
            |_| {},
        );
        assert_eq!(first.merge_requested, vec![7]);
        {
            let mut pr = forge.pr.lock().unwrap();
            let pr = pr.as_mut().unwrap();
            pr.head_ref_oid = "def".into();
            pr.state = "MERGED".into();
        }
        let second = drive_queue(
            &config,
            &Config::default(),
            &forge,
            Path::new("/repo"),
            &db,
            vec![PrItem::from(&db.list_pr_queue().unwrap()[0])],
            |_| {},
        );
        assert_eq!(second.dropped, vec![7]);
        assert!(second.merged.is_empty());
    }

    // Positive control: an externally merged ready row is not attributed to thegn.
    #[test]
    fn externally_merged_ready_row_is_dropped_not_merged() {
        let (dir, db) = temp_db("prq-external-ready-merged");
        let row = enqueue_merge_test_row(&dir, &db);
        let forge = FakeForge::new(Some(green_pr()));
        forge.pr.lock().unwrap().as_mut().unwrap().state = "MERGED".into();
        let outcome = drive_queue(
            &cfg(),
            &Config::default(),
            &forge,
            Path::new("/repo"),
            &db,
            vec![PrItem::from(&row)],
            |_| {},
        );
        assert_eq!(outcome.dropped, vec![7]);
        assert!(outcome.merged.is_empty());
    }

    // Positive control: an armed auto-merge that is later closed remains a drop.
    #[test]
    fn armed_auto_merge_then_closed_pr_is_dropped_never_merged() {
        let (dir, db) = temp_db("prq-auto-merge-closed");
        let row = enqueue_merge_test_row(&dir, &db);
        let forge = FakeForge::new(Some(green_pr()));
        let first = drive_queue(
            &cfg(),
            &Config::default(),
            &forge,
            Path::new("/repo"),
            &db,
            vec![PrItem::from(&row)],
            |_| {},
        );
        assert_eq!(first.auto_merge_armed, vec![7]);
        forge.pr.lock().unwrap().as_mut().unwrap().state = "CLOSED".into();
        let second = drive_queue(
            &cfg(),
            &Config::default(),
            &forge,
            Path::new("/repo"),
            &db,
            vec![PrItem::from(&db.list_pr_queue().unwrap()[0])],
            |_| {},
        );
        assert_eq!(second.dropped, vec![7]);
        assert!(second.merged.is_empty());
        assert_eq!(db.list_pr_queue().unwrap()[0].status, "closed");
    }

    #[expect(
        clippy::disallowed_methods,
        reason = "driver identity fixtures run hermetic git commands"
    )]
    fn git_status(dir: &Path, args: &[&str]) {
        let status = thegn_core::util::git_cmd(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed in {}", dir.display());
    }

    fn ci_agent_config(sentinel: &Path) -> PrQueueConfig {
        PrQueueConfig {
            watch: vec![PrWatchKind::Ci],
            own_prs_only: false,
            agent_command: format!("touch {}", sentinel.display()),
            ..cfg()
        }
    }

    fn assert_ci_agent_held_without_effects(
        checkout: &Path,
        db: &Db,
        row: &PrItem,
        forge: &FakeForge,
        config: &PrQueueConfig,
        sentinel: &Path,
    ) {
        let out = drive_queue(
            config,
            &Config::default(),
            forge,
            checkout,
            db,
            vec![row.clone()],
            |_| {},
        );
        assert!(out.needs_human.contains(&7), "{out:?}");
        assert!(
            forge
                .calls()
                .iter()
                .all(|call| !call.starts_with("rerun") && !call.starts_with("merge")),
            "unexpected forge mutation: {:?}",
            forge.calls()
        );
        assert!(!sentinel.exists(), "agent callback ran");
        let held = db.list_pr_queue().unwrap().remove(0);
        assert_eq!(held.status, PrqStatus::NeedsHuman.as_str());
        assert_eq!(held.ci_rerun_head, None);
        assert_eq!(held.agent_attempts, 0);
    }

    #[test]
    fn origin_moved_to_another_project_holds_before_any_effect() {
        let (dir, db) = temp_db("prq-origin-moved-project");
        let (checkout, row) =
            enqueue_pr_with_checkout(&dir, &db, 7, "feat", "https://github.com/me/repo/pull/7");
        git_status(
            &checkout,
            &[
                "remote",
                "set-url",
                "origin",
                "https://github.com/other/repo",
            ],
        );
        let mut moved = failing_ci_pr("abc");
        moved.url = "https://github.com/other/repo/pull/7".into();
        let forge = FakeForge::new(Some(failing_ci_pr("abc")))
            .with_status_sequence(vec![failing_ci_pr("abc"), moved]);
        let sentinel = dir.path().join("agent-ran");
        let config = ci_agent_config(&sentinel);

        assert_ci_agent_held_without_effects(&checkout, &db, &row, &forge, &config, &sentinel);
        assert!(
            db.list_pr_queue().unwrap()[0]
                .detail
                .as_deref()
                .is_some_and(|detail| detail.contains("identity"))
        );
    }

    #[test]
    fn forge_host_change_holds_before_any_effect() {
        let (dir, db) = temp_db("prq-forge-host-changed");
        let (_checkout, row) =
            enqueue_pr_with_checkout(&dir, &db, 7, "feat", "https://github.com/me/repo/pull/7");
        let mut changed = green_pr();
        changed.url = "https://github.example.com/me/repo/pull/7".into();
        let forge =
            FakeForge::new(Some(green_pr())).with_status_sequence(vec![green_pr(), changed]);
        let mut config = cfg();
        config.merge_mode = thegn_core::config::PrMergeMode::Thegn;
        db.update_pr_status(&row.key, "ready", None, None, Some("abc"))
            .unwrap();

        let out = drive_queue(
            &config,
            &Config::default(),
            &forge,
            Path::new("/repo"),
            &db,
            vec![row],
            |_| {},
        );
        assert!(out.needs_human.contains(&7), "{out:?}");
        assert!(
            forge
                .calls()
                .iter()
                .all(|call| !call.starts_with("merge") && !call.starts_with("rerun"))
        );
        let held = db.list_pr_queue().unwrap().remove(0);
        assert_eq!(held.status, PrqStatus::NeedsHuman.as_str());
        assert_eq!(held.agent_attempts, 0);
        assert_eq!(held.ci_rerun_head, None);
        assert!(
            held.detail
                .as_deref()
                .is_some_and(|detail| detail.contains("identity"))
        );
    }

    #[test]
    fn deleted_checkout_holds_before_any_effect() {
        let (dir, db) = temp_db("prq-deleted-checkout");
        let (checkout, row) =
            enqueue_pr_with_checkout(&dir, &db, 7, "feat", "https://github.com/me/repo/pull/7");
        std::fs::remove_dir_all(&checkout).unwrap();
        let pr = green_pr();
        let forge = FakeForge::new(Some(pr.clone())).with_status_sequence(vec![pr.clone(), pr]);
        let sentinel = dir.path().join("agent-ran");
        let mut config = cfg();
        config.merge_mode = thegn_core::config::PrMergeMode::Thegn;
        db.update_pr_status(&row.key, "ready", None, None, Some("abc"))
            .unwrap();

        let out = drive_queue(
            &config,
            &Config::default(),
            &forge,
            &checkout,
            &db,
            vec![row],
            |_| {},
        );
        assert!(out.needs_human.contains(&7), "{out:?}");
        assert!(
            forge
                .calls()
                .iter()
                .all(|call| !call.starts_with("merge") && !call.starts_with("rerun"))
        );
        assert!(!sentinel.exists());
        let held = db.list_pr_queue().unwrap().remove(0);
        assert_eq!(held.status, PrqStatus::NeedsHuman.as_str());
        assert_eq!(held.agent_attempts, 0);
        assert_eq!(held.ci_rerun_head, None);
        assert!(
            held.detail
                .as_deref()
                .is_some_and(|detail| detail.contains("thegn pr queue rebind --pr 7"))
        );
    }

    #[test]
    fn reused_checkout_path_with_another_repository_holds() {
        let (dir, db) = temp_db("prq-reused-checkout-other-repo");
        let (checkout, row) =
            enqueue_pr_with_checkout(&dir, &db, 7, "feat", "https://github.com/me/repo/pull/7");
        std::fs::remove_dir_all(&checkout).unwrap();
        std::fs::create_dir(&checkout).unwrap();
        git_repo_on_branch(&checkout, "feat");
        git_status(
            &checkout,
            &["remote", "add", "origin", "https://github.com/other/repo"],
        );
        let mut moved = failing_ci_pr("abc");
        moved.url = "https://github.com/other/repo/pull/7".into();
        let forge = FakeForge::new(Some(failing_ci_pr("abc")))
            .with_status_sequence(vec![failing_ci_pr("abc"), moved]);
        let sentinel = dir.path().join("agent-ran");
        let config = ci_agent_config(&sentinel);

        assert_ci_agent_held_without_effects(&checkout, &db, &row, &forge, &config, &sentinel);
    }

    #[test]
    fn renamed_checkout_branch_holds_before_dispatch() {
        let (dir, db) = temp_db("prq-renamed-checkout-branch");
        let (checkout, row) =
            enqueue_pr_with_checkout(&dir, &db, 7, "feat", "https://github.com/me/repo/pull/7");
        git_status(&checkout, &["branch", "-m", "feat-renamed"]);
        let pr = failing_ci_pr("abc");
        let forge = FakeForge::new(Some(pr.clone())).with_status_sequence(vec![pr.clone(), pr]);
        let sentinel = dir.path().join("agent-ran");
        let config = ci_agent_config(&sentinel);

        assert_ci_agent_held_without_effects(&checkout, &db, &row, &forge, &config, &sentinel);
    }

    #[test]
    fn switched_checkout_branch_holds_before_dispatch() {
        let (dir, db) = temp_db("prq-switched-checkout-branch");
        let (checkout, row) =
            enqueue_pr_with_checkout(&dir, &db, 7, "feat", "https://github.com/me/repo/pull/7");
        git_status(&checkout, &["checkout", "-b", "other"]);
        let pr = failing_ci_pr("abc");
        let forge = FakeForge::new(Some(pr.clone())).with_status_sequence(vec![pr.clone(), pr]);
        let sentinel = dir.path().join("agent-ran");
        let config = ci_agent_config(&sentinel);

        assert_ci_agent_held_without_effects(&checkout, &db, &row, &forge, &config, &sentinel);
    }

    #[test]
    fn agent_dispatch_holds_while_ci_autofix_lease_is_live() {
        let (dir, db) = temp_db("prq-agent-lease-held");
        let (checkout, item) =
            enqueue_pr_with_checkout(&dir, &db, 7, "feat", "https://github.com/me/repo/pull/7");
        let head = GitLoc::from_db(checkout.to_str().unwrap(), None)
            .git_out(&["rev-parse", "HEAD"])
            .unwrap();
        let request = pr_queue::PrAgentAdmit {
            key: &item.key,
            expected_attempts: 0,
            expected_checkout_gen: item.checkout_gen,
            max_attempts: 3,
            head: &head,
            blocker: "ci",
            detail: "CI autofix dispatch",
            lease_until_ms: crate::pr_agent_lease::lease_until_ms(),
            owner_hint: Some("CI autofix"),
            ci_candidate: None,
        };
        assert!(matches!(
            db.admit_pr_agent(&request).unwrap(),
            pr_queue::PrAgentAdmission::Admitted(_)
        ));
        let sentinel = dir.path().join("agent-must-not-run");
        let config = ci_agent_config(&sentinel);
        let forge = FakeForge::new(Some(failing_ci_pr(&head)));
        let out = drive_queue(
            &config,
            &uncontained_config(),
            &forge,
            &checkout,
            &db,
            vec![item],
            |_| {},
        );
        assert!(!sentinel.exists());
        assert_eq!(db.list_pr_queue().unwrap()[0].agent_attempts, 1);
        assert!(out.blocked.contains(&7), "{out:?}");
        assert!(
            out.warnings
                .iter()
                .any(|warning| warning.contains("agent not dispatched")),
            "{out:?}"
        );
    }

    #[test]
    fn leased_row_is_skipped_without_forge_calls_or_writes() {
        let (dir, db) = temp_db("prq-live-lease-refresh-skip");
        let (checkout, item) =
            enqueue_pr_with_checkout(&dir, &db, 7, "feat", "https://github.com/me/repo/pull/7");
        let head = GitLoc::from_db(checkout.to_str().unwrap(), None)
            .git_out(&["rev-parse", "HEAD"])
            .unwrap();
        let request = pr_queue::PrAgentAdmit {
            key: &item.key,
            expected_attempts: 0,
            expected_checkout_gen: item.checkout_gen,
            max_attempts: 3,
            head: &head,
            blocker: "ci",
            detail: "agent is changing this PR",
            lease_until_ms: crate::pr_agent_lease::lease_until_ms(),
            owner_hint: Some("active worker"),
            ci_candidate: None,
        };
        assert!(matches!(
            db.admit_pr_agent(&request).unwrap(),
            pr_queue::PrAgentAdmission::Admitted(_)
        ));
        let forge = FakeForge::new(Some(green_pr()));

        let out = drive_queue(
            &cfg(),
            &uncontained_config(),
            &forge,
            &checkout,
            &db,
            vec![item],
            |_| {},
        );

        assert!(forge.calls().is_empty(), "{:?}", forge.calls());
        let row = db.list_pr_queue().unwrap().remove(0);
        assert_eq!(row.status, PrqStatus::AgentRunning.as_str());
        assert_eq!(row.detail.as_deref(), Some("agent is changing this PR"));
        assert_eq!(row.last_head_oid.as_deref(), Some(head.as_str()));
        assert_eq!(row.agent_attempts, 1);
        assert!(out.blocked.contains(&7), "{out:?}");
        assert!(
            out.warnings
                .iter()
                .any(|warning| warning.contains("refresh skipped")
                    && warning.contains("held by active worker")),
            "{out:?}"
        );
    }

    #[test]
    fn reattest_refuses_while_a_lease_is_live() {
        let (_, db) = temp_db("prq-reattest-live-lease");
        db.enqueue_bound_pr(
            "/repo",
            7,
            None,
            "feat",
            "main",
            "github",
            "https://github.com/me/repo/pull/7",
        )
        .unwrap();
        let row = db.list_pr_queue().unwrap().remove(0);
        let item = PrItem::from(&row);
        let request = pr_queue::PrAgentAdmit {
            key: &item.key,
            expected_attempts: 0,
            expected_checkout_gen: item.checkout_gen,
            max_attempts: 3,
            head: "abc",
            blocker: "ci",
            detail: "active worker",
            lease_until_ms: crate::pr_agent_lease::lease_until_ms(),
            owner_hint: Some("active worker"),
            ci_candidate: None,
        };
        assert!(matches!(
            db.admit_pr_agent(&request).unwrap(),
            pr_queue::PrAgentAdmission::Admitted(_)
        ));
        let expected = fetched(vec![]);
        let forge = FakeForge::new(Some(green_pr()));

        let result = reattest(
            &db,
            &forge,
            &GitLoc::from_db("/repo", None),
            &item,
            &expected,
        );

        assert!(matches!(result, Err(ref error) if error.contains("a PR agent is running")));
    }

    #[test]
    fn expired_agent_lease_is_reconciled_before_refresh() {
        let (dir, db) = temp_db("prq-expired-lease-refresh");
        let (checkout, item) =
            enqueue_pr_with_checkout(&dir, &db, 7, "feat", "https://github.com/me/repo/pull/7");
        let head = GitLoc::from_db(checkout.to_str().unwrap(), None)
            .git_out(&["rev-parse", "HEAD"])
            .unwrap();
        let request = pr_queue::PrAgentAdmit {
            key: &item.key,
            expected_attempts: 0,
            expected_checkout_gen: item.checkout_gen,
            max_attempts: 3,
            head: &head,
            blocker: "ci",
            detail: "previous agent",
            lease_until_ms: crate::pr_agent_lease::lease_until_ms(),
            owner_hint: Some("dead worker"),
            ci_candidate: None,
        };
        assert!(matches!(
            db.admit_pr_agent(&request).unwrap(),
            pr_queue::PrAgentAdmission::Admitted(_)
        ));
        rusqlite::Connection::open(dir.path().join("prq-expired-lease-refresh.db"))
            .unwrap()
            .execute(
                "UPDATE pr_queue SET agent_claim_lease_until_ms=0 WHERE key=?1",
                [&item.key],
            )
            .unwrap();

        let forge = FakeForge::new(Some(green_pr()));
        let out = drive_queue(
            &cfg(),
            &uncontained_config(),
            &forge,
            &checkout,
            &db,
            vec![item],
            |_| {},
        );

        assert!(!forge.calls().is_empty(), "{:?}", forge.calls());
        let row = db.list_pr_queue().unwrap().remove(0);
        assert_ne!(row.status, PrqStatus::AgentRunning.as_str());
        assert_eq!(row.agent_attempts, 1);
        assert_eq!(out.warnings.len(), 1, "{out:?}");
        assert!(out.warnings[0].contains("lost its lease"), "{out:?}");
    }

    #[test]
    fn agent_not_started_warning_names_the_reason() {
        let warning = agent_not_started_warning(
            7,
            &crate::agent_run::AgentRunVerdict::NotStarted("sandbox entry refused".into()),
        );

        assert!(warning.is_some_and(|warning| {
            warning.contains("PR #7: agent did not start: sandbox entry refused")
        }));
    }

    #[test]
    fn agent_not_started_warning_is_silent_for_runs_that_started() {
        for verdict in [
            crate::agent_run::AgentRunVerdict::Exited(Some(0)),
            crate::agent_run::AgentRunVerdict::Exited(Some(1)),
            crate::agent_run::AgentRunVerdict::Exited(None),
            crate::agent_run::AgentRunVerdict::Interrupted("timed out".into()),
        ] {
            assert_eq!(agent_not_started_warning(7, &verdict), None);
        }
    }

    #[test]
    fn agent_not_started_verdict_is_consumed_by_the_pr_queue() {
        let source = include_str!("pr_driver.rs");
        let production = source
            .split_once("\n#[cfg(test)]\nmod tests {")
            .expect("test module boundary exists")
            .0;
        let run_call = production
            .find("let verdict = crate::agent_run::run_outcome_cancellable(")
            .expect("PR queue binds the agent run verdict");
        let warning_call = production
            .find("agent_not_started_warning(item.number, &verdict)")
            .expect("PR queue turns NotStarted into a warning");

        assert!(warning_call > run_call);
    }
}
