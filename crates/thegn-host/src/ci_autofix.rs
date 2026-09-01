//! Guarded CI-failure handoff to the existing PR-queue agent seam.
//!
//! This coordinator is deliberately small: CI owns evidence collection, the
//! PR queue owns agent policy, and [`crate::agent_run`] owns process mechanics.
//! Every entry point here is called from a blocking refresh worker.

use std::path::Path;

use thegn_core::ci_log::CiLogEntry;
use thegn_core::config::{CiAutofixMode, Config};
use thegn_core::forge::{FetchedPr, Forge};
use thegn_core::pr_queue::{Blocker, PrqStatus};
use thegn_core::store::{CacheStore, NotificationStore, WorktreeAuxStore};

/// Consider one newly cached failed-job log.  Missing or stale context is
/// surfaced as a deduplicated notification; it is never treated as permission
/// to dispatch an agent.
pub(crate) fn consider(full: &Config, db: &thegn_core::db::Db, entry: &CiLogEntry) {
    consider_candidate(full, db, entry, false);
}

/// Authorize a human-requested handoff from a cached CI-log candidate.  The
/// cache entry is re-read in the blocking action worker, then all of the same
/// PR/head/agent gates used by automatic mode are applied before the atomic
/// dedupe claim and spawn.
pub(crate) fn authorize(
    full: &Config,
    db: &thegn_core::db::Db,
    candidate: &thegn_core::ci_log::CiLogCandidate,
) {
    let Some(entry) = db
        .get_ci_log(&candidate.worktree, &candidate.run_id, &candidate.job_id)
        .ok()
        .flatten()
    else {
        return;
    };
    if entry.candidate() != *candidate || entry.text.trim().is_empty() {
        return;
    }
    consider_candidate(full, db, &entry, true);
}

/// Parse the candidate carried by a `pr_queue_needs_human` notification.  The
/// worktree is a separate notification field, while the source reference keeps
/// the run/job/head identity opaque to the generic notification store.
pub(crate) fn candidate_from_notification(
    n: &thegn_core::notification::Notification,
) -> Option<thegn_core::ci_log::CiLogCandidate> {
    let body = n.source_ref.strip_prefix("ci-autofix:")?;
    let prefix = format!("{}:", n.worktree_path);
    let fields = body.strip_prefix(&prefix)?;
    let mut fields = fields.splitn(3, ':');
    let run_id = fields.next()?.trim();
    let job_id = fields.next()?.trim();
    let head_sha = fields.next()?.trim();
    if run_id.is_empty() || job_id.is_empty() || head_sha.is_empty() {
        return None;
    }
    Some(thegn_core::ci_log::CiLogCandidate {
        worktree: n.worktree_path.clone(),
        run_id: run_id.to_string(),
        job_id: job_id.to_string(),
        head_sha: head_sha.to_string(),
    })
}

fn consider_candidate(
    full: &Config,
    db: &thegn_core::db::Db,
    entry: &CiLogEntry,
    human_authorized: bool,
) {
    let root = thegn_core::repo::main_worktree(Path::new(&entry.worktree))
        .unwrap_or_else(|| Path::new(&entry.worktree).to_path_buf());
    let ci = full.repo_ci(&root);
    if ci.autofix.mode == CiAutofixMode::Off {
        return;
    }

    let candidate = entry.candidate();
    let ref_id = format!(
        "ci-autofix:{}:{}:{}:{}",
        candidate.worktree, candidate.run_id, candidate.job_id, candidate.head_sha
    );
    let notify = |message: String| {
        let _ =
            db.put_notification_once("pr_queue_needs_human", &ref_id, &message, &entry.worktree);
    };

    if entry.text.trim().is_empty() {
        notify(format!(
            "CI job {} failed, but no redacted log evidence is available for autofix",
            entry.job_name
        ));
        return;
    }

    let Some(item) = db
        .list_pr_queue()
        .unwrap_or_default()
        .into_iter()
        .find(|row| row.worktree.as_deref() == Some(entry.worktree.as_str()))
    else {
        notify(format!(
            "CI job {} failed, but no queued PR context is available for autofix",
            entry.job_name
        ));
        return;
    };
    let Some(worktree) = item.worktree.as_deref().filter(|w| !w.trim().is_empty()) else {
        notify(format!(
            "CI job {} failed, but its PR has no local worktree",
            entry.job_name
        ));
        return;
    };
    if entry.head_sha.trim().is_empty() {
        notify(format!(
            "CI job {} failed, but the run has no head SHA; autofix is unavailable",
            entry.job_name
        ));
        return;
    }

    let pq = full.repo_pr_queue(&root);
    let agent = thegn_core::agent_task::resolve_agent(full, &pq.agent, &pq.agent_command);
    if !pq.enabled || agent.is_none() {
        notify(format!(
            "CI job {} failed for PR #{}; configure the PR-queue agent to enable autofix",
            entry.job_name, item.number
        ));
        return;
    }
    if item.agent_attempts >= pq.agent_max_attempts {
        notify(format!(
            "CI job {} failed for PR #{}; the PR-queue agent budget is exhausted",
            entry.job_name, item.number
        ));
        return;
    }

    let loc = thegn_core::remote::GitLoc::for_worktree(Path::new(worktree));
    let forge = crate::forge_handle::get();
    let provider = forge.for_loc(&loc);
    let Some(fetched) = provider.fetch_pr(&loc, item.number).ok() else {
        notify(format!(
            "CI job {} failed for PR #{}; the current PR head could not be verified",
            entry.job_name, item.number
        ));
        return;
    };
    if !head_is_current(provider, &loc, &fetched, entry.head_sha.as_str()) {
        notify(format!(
            "CI job {} failed for PR #{}; the PR head changed, so autofix is held",
            entry.job_name, item.number
        ));
        return;
    }
    let blocker = thegn_core::pr_queue::classify(&fetched.pr, &pq);
    if !matches!(blocker, Blocker::Ci(_)) {
        return;
    }
    if pq.own_prs_only && !is_own_pr(provider, &loc, &fetched) {
        notify(format!(
            "CI job {} failed for PR #{}; PR ownership could not be verified for autofix",
            entry.job_name, item.number
        ));
        return;
    }

    if ci.autofix.mode == CiAutofixMode::Suggest && !human_authorized {
        notify(format!(
            "CI job {} failed for PR #{}; CI log evidence is ready for a PR-agent handoff",
            entry.job_name, item.number
        ));
        return;
    }

    let dispatch = crate::agent_run::agent_floor_gate(
        full,
        worktree,
        pq.agent_sandbox,
        pq.agent_isolation_floor,
        pq.agent_on_floor_miss,
    );
    let sandbox = match dispatch {
        crate::agent_run::AgentDispatch::InfraHold(reason) => {
            notify(format!("CI autofix for PR #{} held: {reason}", item.number));
            return;
        }
        crate::agent_run::AgentDispatch::RunDegraded(spec, warning) => {
            tracing::warn!(target: "thegn::ci_autofix", "{warning}");
            spec
        }
        crate::agent_run::AgentDispatch::Run(spec) => spec,
    };
    let template = agent.as_deref().expect("agent checked above");
    let item = crate::pr_driver::PrItem::from(&item);
    let log = Some(entry.text.as_str());
    let Some((vars, prompt)) = crate::pr_driver::compose_with_log(
        &pq,
        thegn_core::agent_task::TaskKind::PrCiFailure,
        worktree,
        &item,
        &fetched,
        &blocker,
        log,
    ) else {
        notify(format!(
            "CI autofix for PR #{} was not dispatched: prompt is invalid",
            item.number
        ));
        return;
    };
    // Claim only after every safety and prompt gate has passed, immediately
    // before consuming the attempt and spawning. A refresh race therefore
    // spends at most one dispatch without permanently suppressing a candidate
    // that was held by infrastructure or rejected by prompt validation.
    if !db.claim_ci_autofix(&candidate).unwrap_or(false) {
        return;
    }
    let next_attempt = item.agent_attempts.saturating_add(1);
    let _ = db.set_pr_agent_attempts(&item.key, next_attempt);
    let note = format!(
        "CI autofix dispatched ({next_attempt}/{})",
        pq.agent_max_attempts
    );
    let _ = db.update_pr_status(
        &item.key,
        PrqStatus::AgentRunning.as_str(),
        Some("ci"),
        Some(&note),
        Some(entry.head_sha.as_str()),
    );
    let _ = crate::agent_run::run(&crate::agent_run::AgentTaskRun {
        kind: thegn_core::agent_task::TaskKind::PrCiFailure,
        worktree,
        prompt: &prompt,
        command_template: template,
        vars: &vars,
        timeout_secs: pq.agent_timeout_secs,
        sandbox,
    });
}

fn head_is_current(
    forge: &dyn Forge,
    loc: &thegn_core::remote::GitLoc,
    fetched: &FetchedPr,
    expected: &str,
) -> bool {
    fetched.pr.state.eq_ignore_ascii_case("OPEN")
        && fetched.pr.head_ref_oid == expected
        && loc.git_out(&["rev-parse", "HEAD"]).as_deref() == Some(expected)
        && !forge.id().is_empty()
}

fn is_own_pr(forge: &dyn Forge, loc: &thegn_core::remote::GitLoc, fetched: &FetchedPr) -> bool {
    let Some(me) = forge.whoami(loc).ok().map(|s| s.trim().to_string()) else {
        return false;
    };
    fetched
        .pr
        .url
        .split('/')
        .nth(3)
        .is_some_and(|owner| owner.eq_ignore_ascii_case(&me))
}
