//! Guarded CI-failure handoff to the existing PR-queue agent seam.
//!
//! This coordinator is deliberately small: CI owns evidence collection, the
//! PR queue owns agent policy, and [`crate::agent_run`] owns process mechanics.
//! Every entry point here is called from a blocking refresh worker.

use std::path::Path;

use thegn_core::ci_log::CiLogEntry;
use thegn_core::config::{CiAutofixMode, Config};
use thegn_core::forge::{FetchedPr, Forge, PrRef};
use thegn_core::pr_queue::{Blocker, PrQueueFacts, PrqStatus};
use thegn_core::store::{CacheStore, NotificationStore, WorkspaceStore, WorktreeAuxStore};

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
    let root = thegn_core::repo::main_worktree(Path::new(&entry.worktree))
        .unwrap_or_else(|| Path::new(&entry.worktree).to_path_buf());
    if full.repo_ci(&root).autofix.mode == CiAutofixMode::Off {
        return;
    }
    let location = db.location_for(&entry.worktree).ok().flatten();
    let loc = thegn_core::remote::GitLoc::from_db(&entry.worktree, location.as_deref());
    // Remote/provider readers deliberately have no current context: resolving it would run git
    // through a remote control path, so known-context cached logs fail closed until refreshed.
    let current = crate::ci_refresh::reader_cache_context(&loc, &full.repo_ci(&root));
    authorize_entry(full, db, &entry, current.as_deref(), None);
}

/// The notification source reference for one cached-log candidate. The format is parsed back by
/// [`candidate_from_notification`].
fn candidate_ref_id(candidate: &thegn_core::ci_log::CiLogCandidate) -> String {
    let mut ref_id = format!(
        "ci-autofix:{}:{}:{}:{}",
        candidate.worktree, candidate.run_id, candidate.job_id, candidate.head_sha
    );
    if let Some(attempt) = candidate.attempt {
        ref_id.push_str(":@");
        ref_id.push_str(&attempt.to_string());
    }
    if !candidate.context.is_empty() {
        ref_id.push(':');
        ref_id.push_str(&candidate.context);
    }
    ref_id
}

fn ci_run_ref_id(entry: &CiLogEntry) -> String {
    format!("ci-run:{}:{}", entry.worktree, entry.run_id)
}

fn notify_ci_run_failed(db: &thegn_core::db::Db, entry: &CiLogEntry) {
    let kind = thegn_core::notification::NotificationKind::CiRunFailed.as_str();
    let source_ref = ci_run_ref_id(entry);
    let notifications = db.get_all_notifications(usize::MAX).unwrap_or_default();
    let prefix = format!("ci-run:{}:", entry.worktree);
    let this_run = entry.run_id.parse::<u64>().ok();
    let newer_exists = notifications.iter().any(|n| {
        n.kind.as_str() == kind
            && n.worktree_path == entry.worktree
            && n.source_ref
                .strip_prefix(&prefix)
                .and_then(|run| run.parse::<u64>().ok())
                .zip(this_run)
                .is_some_and(|(other, this)| other > this)
    });
    if newer_exists {
        return;
    }
    let mut job_names = notifications
        .iter()
        .find(|notification| {
            notification.kind.as_str() == kind
                && notification.source_ref == source_ref
                && notification.worktree_path == entry.worktree
        })
        .and_then(|notification| {
            notification
                .message
                .split_once("failed jobs: ")
                .map(|(_, jobs)| jobs.to_string())
        })
        .map(|jobs| {
            jobs.split(", ")
                .filter(|job| !job.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if !job_names.iter().any(|job| job == &entry.job_name) {
        job_names.push(entry.job_name.clone());
    }
    job_names.sort();
    let message = format!(
        "CI run {} failed; failed jobs: {}",
        entry.run_id,
        job_names.join(", ")
    );

    for notification in notifications {
        if notification.kind.as_str() == kind
            && notification.worktree_path == entry.worktree
            && notification.source_ref.starts_with("ci-run:")
            && notification.source_ref != source_ref
        {
            // Best-effort: the notification table is a cache, and supersession
            // must not block the CI refresh if one old row cannot be updated.
            if let Err(error) =
                db.mark_notification_read_by_source(kind, &notification.source_ref, &entry.worktree)
            {
                tracing::debug!(target: "thegn::ci_autofix", %error, "could not supersede an older CI run notification");
            }
        }
    }

    match db.update_notification_message_by_source(kind, &source_ref, &message, &entry.worktree) {
        Ok(0) => match db.has_notification(kind, &source_ref) {
            Ok(true) => {}
            Ok(false) => {
                if let Err(error) =
                    db.put_notification_once(kind, &source_ref, &message, &entry.worktree)
                {
                    tracing::debug!(target: "thegn::ci_autofix", %error, "could not save CI run failure notification");
                }
            }
            Err(error) => {
                tracing::debug!(target: "thegn::ci_autofix", %error, "could not check CI run failure notification");
            }
        },
        Ok(_) => {}
        Err(error) => {
            tracing::debug!(target: "thegn::ci_autofix", %error, "could not update CI run failure notification");
        }
    }
}

/// The human-authorized gate once the entry is read and the worktree's CURRENT provider context is known:
/// a log cached for another (or an unknown) context is never authorization to dispatch an agent.
fn authorize_entry(
    full: &Config,
    db: &thegn_core::db::Db,
    entry: &CiLogEntry,
    current_context: Option<&str>,
    selected_forge: Option<&dyn Forge>,
) {
    // Keep Off mode silent even when a human clicks a candidate that has gone stale.
    let root = thegn_core::repo::main_worktree(Path::new(&entry.worktree))
        .unwrap_or_else(|| Path::new(&entry.worktree).to_path_buf());
    if full.repo_ci(&root).autofix.mode == CiAutofixMode::Off {
        return;
    }
    if !thegn_core::ci_log::cached_log_context_matches(entry, current_context) {
        tracing::info!(
            target: "thegn::ci_autofix",
            job = %entry.job_name,
            "holding CI autofix authorization because cached provider context differs"
        );
        let ref_id = candidate_ref_id(&entry.candidate());
        match db.has_notification("pr_queue_needs_human", &ref_id) {
            Ok(true) => return,
            Ok(false) => {}
            Err(error) => {
                tracing::debug!(
                    target: "thegn::ci_autofix",
                    %error,
                    "could not check for an existing CI autofix notification"
                );
                return;
            }
        }
        let message = format!(
            "CI job {} failed, but its cached log belongs to another provider context than this worktree's current one; autofix is held until the log is refreshed",
            entry.job_name
        );
        if let Err(error) =
            db.put_notification_once("pr_queue_needs_human", &ref_id, &message, &entry.worktree)
        {
            tracing::debug!(target: "thegn::ci_autofix", %error, "could not save CI autofix context notification");
        }
        return;
    }
    consider_candidate_with_forge(full, db, entry, true, selected_forge);
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
    let mut fields = fields.splitn(4, ':');
    let run_id = fields.next()?.trim();
    let job_id = fields.next()?.trim();
    let head_sha = fields.next()?.trim();
    let rest = fields.next().unwrap_or("").trim();
    let (attempt, context) = if let Some(tail) = rest.strip_prefix('@') {
        let (number, context) = tail.split_once(':').unwrap_or((tail, ""));
        (Some(number.parse::<u64>().ok()?), context)
    } else {
        (None, rest)
    };
    if run_id.is_empty() || job_id.is_empty() || head_sha.is_empty() {
        return None;
    }
    Some(thegn_core::ci_log::CiLogCandidate {
        worktree: n.worktree_path.clone(),
        run_id: run_id.to_string(),
        job_id: job_id.to_string(),
        head_sha: head_sha.to_string(),
        context: context.to_string(),
        attempt,
    })
}

fn consider_candidate(
    full: &Config,
    db: &thegn_core::db::Db,
    entry: &CiLogEntry,
    human_authorized: bool,
) {
    consider_candidate_with_forge(full, db, entry, human_authorized, None);
}

fn consider_candidate_with_forge(
    full: &Config,
    db: &thegn_core::db::Db,
    entry: &CiLogEntry,
    human_authorized: bool,
    selected_forge: Option<&dyn Forge>,
) {
    let root = thegn_core::repo::main_worktree(Path::new(&entry.worktree))
        .unwrap_or_else(|| Path::new(&entry.worktree).to_path_buf());
    let ci = full.repo_ci(&root);
    if ci.autofix.mode == CiAutofixMode::Off {
        return;
    }

    let candidate = entry.candidate();
    let ref_id = candidate_ref_id(&candidate);
    let notify = |message: String| {
        if let Err(error) =
            db.put_notification_once("pr_queue_needs_human", &ref_id, &message, &entry.worktree)
        {
            tracing::debug!(target: "thegn::ci_autofix", %error, "could not save CI autofix notification");
        }
    };

    if entry.text.trim().is_empty() {
        notify(format!(
            "CI job {} failed, but no redacted log evidence is available for autofix",
            entry.job_name
        ));
        return;
    }

    let item = match queue_row_for_worktree(db.list_pr_queue().unwrap_or_default(), &entry.worktree)
    {
        QueueRowPick::One(row) => *row,
        QueueRowPick::None => {
            notify_ci_run_failed(db, entry);
            return;
        }
        QueueRowPick::Ambiguous(numbers) => {
            let numbers = numbers
                .iter()
                .map(|number| format!("#{number}"))
                .collect::<Vec<_>>()
                .join(", ");
            notify(format!(
                "CI job {} failed, but PR queue rows {numbers} share this worktree; autofix is held until one remains (remove the stale row with `thegn pr queue rm <number>`)",
                entry.job_name
            ));
            return;
        }
    };
    let item = match db.pr_agent_lease_view(&item.key) {
        Ok(Some(view))
            if thegn_core::pr_queue::pr_agent_lease_expired(
                Some(&view),
                thegn_core::util::now_ms(),
            ) =>
        {
            match crate::pr_agent_lease::reconcile_expired(db, &item.key, item.number, &view) {
                Ok(Some(warning)) => {
                    tracing::warn!(target: "thegn::ci_autofix", "{warning}");
                }
                Ok(None) => {}
                Err(error) => {
                    notify(format!(
                        "CI autofix for PR #{} held: could not reconcile the expired PR-agent lease ({error}); agent dispatch refused",
                        item.number
                    ));
                    return;
                }
            }
            let reread = db.list_pr_queue().and_then(|rows| {
                rows.into_iter()
                    .find(|row| row.key == item.key)
                    .ok_or_else(|| anyhow::anyhow!("PR queue row disappeared"))
            });
            match reread {
                Ok(row) => row,
                Err(error) => {
                    tracing::warn!(
                        target: "thegn::ci_autofix",
                        pr = item.number,
                        %error,
                        "could not re-read the PR queue row after reconciling an expired agent lease"
                    );
                    notify(format!(
                        "CI autofix for PR #{} held: could not re-read the PR queue row after reconciling an expired agent lease",
                        item.number
                    ));
                    return;
                }
            }
        }
        _ => item,
    };
    let row_status = match PrqStatus::parse(&item.status) {
        Some(status)
            if !status.is_settled()
                && !matches!(status, PrqStatus::AgentRunning | PrqStatus::Merging) =>
        {
            status
        }
        Some(status) => {
            notify(format!(
                "CI autofix for PR #{} held: PR queue row is `{}`; agent dispatch refused",
                item.number,
                status.as_str()
            ));
            return;
        }
        None => {
            notify(format!(
                "CI autofix for PR #{} held: unknown PR queue status `{}`; agent dispatch refused",
                item.number, item.status
            ));
            return;
        }
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

    let location = match db.location_for(worktree) {
        Ok(location) => location,
        Err(_) if pq.own_prs_only => {
            notify(crate::pr_authorship::HELD.into());
            return;
        }
        Err(_) => None,
    };
    let loc = thegn_core::remote::GitLoc::from_db(worktree, location.as_deref());
    let forges;
    let provider = if let Some(forge) = selected_forge {
        forge
    } else {
        forges = crate::forge_handle::get();
        forges.for_loc(&loc)
    };
    if item.forge != provider.id() {
        hold_identity_mismatch(
            db,
            &item.key,
            item.number,
            &format!(
                "queued forge {} differs from resolved forge {}; re-add with `thegn pr queue add --pr {}` to verify it",
                item.forge,
                provider.id(),
                item.number
            ),
            &notify,
        );
        return;
    }
    let Some(fetched) = provider.fetch_pr(&loc, item.number).ok() else {
        notify(format!(
            "CI job {} failed for PR #{}; the current PR head could not be verified",
            entry.job_name, item.number
        ));
        return;
    };
    if let Err(detail) = thegn_core::pr_queue::attest_pr_identity(
        item.pr_url.as_deref(),
        item.number,
        &fetched.pr.url,
        fetched.pr.number,
    ) {
        hold_identity_mismatch(db, &item.key, item.number, &detail, &notify);
        return;
    }
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
    let facts = PrQueueFacts {
        worktree: item.worktree.clone(),
        agent_attempts: item.agent_attempts,
        last_head_oid: item.last_head_oid.clone(),
        is_own: true, // fresh authorship proof follows before dispatch side effects
        agent_available: true,
    };
    match thegn_core::pr_queue::admit_agent_dispatch(
        row_status.as_str(),
        &blocker,
        &facts,
        &fetched.pr.head_ref_oid,
        &pq,
    ) {
        Ok(thegn_core::agent_task::TaskKind::PrCiFailure) => {}
        Ok(_) => {
            notify(format!(
                "CI autofix for PR #{} held: PR queue policy selected a different agent task",
                item.number
            ));
            return;
        }
        Err(reason) => {
            notify(format!("CI autofix for PR #{} held: {reason}", item.number));
            return;
        }
    }
    let authorship = match crate::pr_authorship::acquire(
        pq.own_prs_only,
        db,
        provider,
        &loc,
        &item.forge,
        item.number,
        &fetched.pr,
    ) {
        Ok(proof) => proof,
        Err(reason) => {
            notify(reason.into());
            return;
        }
    };

    if ci.autofix.mode == CiAutofixMode::Suggest && !human_authorized {
        notify(format!(
            "CI job {} failed for PR #{}; CI log evidence is ready for a PR-agent handoff",
            entry.job_name, item.number
        ));
        return;
    }

    if let Some(reason) = crate::agent_run::headless_unsupported() {
        notify(format!("CI autofix for PR #{} held: {reason}", item.number));
        return;
    }

    let dispatch = crate::agent_run::agent_floor_gate(
        full,
        worktree,
        pq.agent_sandbox,
        pq.agent_isolation_floor,
        pq.agent_on_floor_miss,
    );
    let (sandbox, isolation) = match dispatch {
        crate::agent_run::AgentDispatch::InfraHold(reason) => (
            None,
            thegn_core::pr_queue::IsolationAdmission::Refused(reason),
        ),
        crate::agent_run::AgentDispatch::RunDegraded(spec, warning) => {
            tracing::warn!(target: "thegn::ci_autofix", "{warning}");
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
    if let Err(reason) = thegn_core::pr_queue::admit_prepared_dispatch(
        thegn_core::agent_task::TaskKind::PrCiFailure,
        true,
        &isolation,
    ) {
        notify(format!("CI autofix for PR #{} held: {reason}", item.number));
        return;
    }
    let sandbox = sandbox.expect("admitted isolation carries a sandbox spec");
    let template = agent.as_deref().expect("agent checked above");
    let item = crate::pr_driver::PrItem::from(&item);
    let log_text = thegn_core::ci_log::failure_excerpt(
        &thegn_core::ci_log::redact(&entry.text),
        thegn_core::agent_task::input_max_bytes(
            thegn_core::agent_task::TaskKind::PrCiFailure,
            "log",
        ),
    );
    let log = Some(log_text.as_str());
    let prepared = crate::pr_driver::compose_with_log(
        &pq,
        thegn_core::agent_task::TaskKind::PrCiFailure,
        worktree,
        &item,
        &fetched,
        &blocker,
        log,
    );
    if let Err(_reason) = thegn_core::pr_queue::admit_prepared_dispatch(
        thegn_core::agent_task::TaskKind::PrCiFailure,
        prepared.is_some(),
        &thegn_core::pr_queue::IsolationAdmission::Admitted,
    ) {
        notify(format!(
            "CI autofix for PR #{} was not dispatched: prompt is invalid",
            item.number
        ));
        return;
    }
    let (vars, prompt) = prepared.expect("prepared dispatch admitted only with a prompt");
    // The candidate claim and attempt spend now share the PR-agent admission
    // transaction, after every safety and prompt gate has passed.
    if let Err(reason) = crate::pr_authorship::revalidate(
        authorship.as_ref(),
        db,
        provider,
        &loc,
        &fetched.pr,
        false,
    ) {
        notify(reason.into());
        return;
    }
    if let Err(reason) = crate::pr_driver::reattest(db, provider, &loc, &item, &fetched) {
        let detail = reason.replace("before effect", "before agent dispatch");
        let moved = detail.contains("head moved before agent dispatch");
        let status = if moved {
            PrqStatus::Watching
        } else {
            PrqStatus::NeedsHuman
        };
        if let Err(error) =
            db.update_pr_status(&item.key, status.as_str(), None, Some(&detail), None)
        {
            tracing::debug!(target: "thegn::ci_autofix", %error, "could not persist pre-dispatch attestation hold");
        }
        notify(format!("CI autofix for PR #{} held: {detail}", item.number));
        return;
    }
    let next_attempt = item.agent_attempts.saturating_add(1);
    let note = format!(
        "CI autofix dispatched ({next_attempt}/{})",
        pq.agent_max_attempts
    );
    let owner = crate::pr_agent_lease::owner_hint();
    let request = thegn_core::pr_queue::PrAgentAdmit {
        key: &item.key,
        expected_attempts: item.agent_attempts,
        expected_checkout_gen: item.checkout_gen,
        max_attempts: pq.agent_max_attempts,
        head: &entry.head_sha,
        blocker: "ci",
        detail: &note,
        lease_until_ms: crate::pr_agent_lease::lease_until_ms(),
        owner_hint: Some(&owner),
        ci_candidate: Some(&candidate),
    };
    let guard = match crate::pr_agent_lease::PrAgentGuard::admit(db, &request) {
        Ok(Ok(guard)) => guard,
        Ok(Err(thegn_core::pr_queue::PrAgentRefusal::DuplicateCandidate)) => return,
        Ok(Err(refusal)) => {
            notify(format!(
                "CI autofix for PR #{} held: {refusal}",
                item.number
            ));
            return;
        }
        Err(error) => {
            notify(format!(
                "CI autofix for PR #{} held: could not record the dispatch ({error})",
                item.number
            ));
            return;
        }
    };
    tracing::debug!(target: "thegn::ci_autofix", pr = item.number, attempt = guard.attempt(), "CI autofix agent admitted under PR lease");
    if let Err(moved) = admitted_head_still_checked_out(&loc, &entry.head_sha) {
        hold_not_started(guard, db, item.number, &moved, notify);
        return;
    }
    let verdict = crate::agent_run::run_outcome_cancellable(
        &crate::agent_run::AgentTaskRun {
            kind: thegn_core::agent_task::TaskKind::PrCiFailure,
            worktree,
            prompt: &prompt,
            command_template: template,
            vars: &vars,
            timeout_secs: pq.agent_timeout_secs,
            sandbox,
            credential_free: true,
        },
        guard.cancel_flag(),
    );
    if let Some(reason) = verdict.not_started() {
        hold_not_started(guard, db, item.number, reason, notify);
        return;
    }
    if guard.lease_lost() {
        notify(format!(
            "CI autofix for PR #{} stopped: its PR-agent lease could not be confirmed; the next refresh re-evaluates the row",
            item.number
        ));
    }
    let fresh = authorship.as_ref().and_then(|_| {
        provider
            .pr_status(&loc, PrRef::Number(item.number))
            .ok()
            .filter(|pr| {
                thegn_core::pr_queue::attest_pr_identity(
                    item.pr_url.as_deref(),
                    item.number,
                    &pr.url,
                    pr.number,
                )
                .is_ok()
            })
    });
    let remote_after: Option<String> = if authorship.is_some() {
        fresh.as_ref().map(|pr| pr.head_ref_oid.clone())
    } else {
        provider
            .pr_status(&loc, PrRef::Number(item.number))
            .ok()
            .filter(|pr| {
                thegn_core::pr_queue::attest_pr_identity(
                    item.pr_url.as_deref(),
                    item.number,
                    &pr.url,
                    pr.number,
                )
                .is_ok()
            })
            .map(|pr| pr.head_ref_oid)
    };
    let own =
        crate::pr_agent_lease::observe_own_head(&loc, &entry.head_sha, || remote_after.clone());
    let head_note = crate::pr_head_outcome::run_warning(
        &loc,
        item.number,
        &entry.head_sha,
        remote_after.as_deref(),
        !guard.lease_lost(),
        &pq,
    );
    let validation = crate::pr_authorship::revalidate_after_agent(
        authorship.as_ref(),
        db,
        provider,
        &loc,
        fresh.as_ref(),
        own.as_deref(),
    );
    if let Err(reason) = validation {
        match guard.finish(db, Some((PrqStatus::NeedsHuman.as_str(), reason))) {
            Ok(true) => {}
            Ok(false) => {
                tracing::warn!(target: "thegn::ci_autofix", pr = item.number, "CI autofix PR-agent lease was lost before release")
            }
            Err(error) => {
                tracing::warn!(target: "thegn::ci_autofix", %error, pr = item.number, "could not release CI autofix PR-agent lease")
            }
        }
        notify(format!("CI autofix for PR #{} held: {reason}", item.number));
    } else {
        if let Some(own) = own {
            match guard.record_own_head(db, &entry.head_sha, &own) {
                Ok(true) => {}
                Ok(false) => {
                    tracing::warn!(target: "thegn::ci_autofix", pr = item.number, "agent push could not be recorded under its lease; next refresh treats it as foreign")
                }
                Err(error) => {
                    tracing::warn!(target: "thegn::ci_autofix", %error, pr = item.number, "agent push could not be recorded under its lease; next refresh treats it as foreign")
                }
            }
        }
        let finished = autofix_finish_detail(&verdict, head_note.as_deref());
        match guard.finish(db, Some((PrqStatus::BlockedCi.as_str(), &finished))) {
            Ok(true) => {}
            Ok(false) => {
                tracing::warn!(target: "thegn::ci_autofix", pr = item.number, "CI autofix PR-agent lease was lost before release")
            }
            Err(error) => {
                tracing::warn!(target: "thegn::ci_autofix", %error, pr = item.number, "could not release CI autofix PR-agent lease")
            }
        }
    }
}

fn hold_identity_mismatch(
    db: &thegn_core::db::Db,
    key: &str,
    number: u64,
    detail: &str,
    notify: &dyn Fn(String),
) {
    if let Err(error) = db.update_pr_status(
        key,
        PrqStatus::NeedsHuman.as_str(),
        None,
        Some(detail),
        None,
    ) {
        tracing::debug!(target: "thegn::ci_autofix", %error, "could not hold PR queue row after CI autofix identity mismatch");
    }
    notify(format!("CI autofix for PR #{number} held: {detail}"));
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

fn hold_not_started(
    guard: crate::pr_agent_lease::PrAgentGuard,
    db: &thegn_core::db::Db,
    item_number: u64,
    reason: &str,
    notify: impl Fn(String),
) {
    match guard.finish_not_started(db, reason) {
        crate::pr_agent_lease::NotStartedRelease::Refunded => {
            notify(format!(
                "CI autofix for PR #{} held: {}",
                item_number,
                crate::pr_agent_lease::not_started_detail(reason, false)
            ));
        }
        crate::pr_agent_lease::NotStartedRelease::RefundFailed(error) => {
            let detail = crate::pr_agent_lease::not_started_detail(reason, true);
            tracing::warn!(target: "thegn::ci_autofix", pr = item_number, %error, "{detail}");
            notify(format!(
                "CI autofix for PR #{} held: {detail} ({error})",
                item_number
            ));
        }
        crate::pr_agent_lease::NotStartedRelease::LeaseLost => {
            notify(format!(
                "CI autofix for PR #{} stopped: its PR-agent lease could not be confirmed; the next refresh re-evaluates the row",
                item_number
            ));
        }
    }
}

/// Re-read the checkout's HEAD after the PR-agent lease is admitted and
/// immediately before the agent spawns. Authorship, isolation and prompt
/// gates run between the first head check and admission; a commit or
/// checkout in that window must not run the fixer on an unattested HEAD.
fn admitted_head_still_checked_out(
    loc: &thegn_core::remote::GitLoc,
    admitted: &str,
) -> Result<(), String> {
    match loc.git_out(&["rev-parse", "HEAD"]) {
        Some(head) if head == admitted => Ok(()),
        Some(_) => Err("the worktree HEAD moved after the agent was admitted".to_owned()),
        None => Err("the worktree HEAD could not be read after the agent was admitted".to_owned()),
    }
}

/// The PR-queue detail recorded when a CI autofix run ends. A not-started run is held earlier
/// through `finish_not_started` and never reaches this function. A verified PR-head observation
/// is appended when it has an operator sentence.
fn autofix_finish_detail(
    verdict: &crate::agent_run::AgentRunVerdict,
    head_note: Option<&str>,
) -> String {
    let outcome = if verdict.exited_zero() {
        "exit ok".to_string()
    } else {
        "exit failed".to_string()
    };
    match head_note {
        Some(note) => {
            format!("CI autofix finished ({outcome}); {note}; awaiting the next PR refresh")
        }
        None => format!("CI autofix finished ({outcome}); awaiting the next PR refresh"),
    }
}

/// Which PR queue row owns a worktree's CI evidence.
#[derive(Debug, PartialEq, Eq)]
enum QueueRowPick {
    None,
    One(Box<thegn_core::db::PrQueueRow>),
    Ambiguous(Vec<u64>),
}

/// Merged and closed rows for the worktree are history. Exactly one other row
/// is the candidate; two or more are ambiguous (held, never resolved by queue
/// order). With only history left, the newest row is returned so its state
/// hold is reported as before. An unknown status counts as live (fail closed).
fn queue_row_for_worktree(rows: Vec<thegn_core::db::PrQueueRow>, worktree: &str) -> QueueRowPick {
    let (history, live): (Vec<_>, Vec<_>) = rows
        .into_iter()
        .filter(|row| row.worktree.as_deref() == Some(worktree))
        .partition(|row| {
            matches!(
                PrqStatus::parse(&row.status),
                Some(PrqStatus::Merged | PrqStatus::Closed)
            )
        });

    match live.len() {
        0 => history
            .into_iter()
            .max_by_key(|row| row.queued_at)
            .map_or(QueueRowPick::None, |row| QueueRowPick::One(Box::new(row))),
        1 => match <[_; 1]>::try_from(live) {
            Ok([row]) => QueueRowPick::One(Box::new(row)),
            Err(live) => {
                let mut numbers = live.into_iter().map(|row| row.number).collect::<Vec<_>>();
                numbers.sort_unstable();
                QueueRowPick::Ambiguous(numbers)
            }
        },
        _ => {
            let mut numbers = live.into_iter().map(|row| row.number).collect::<Vec<_>>();
            numbers.sort_unstable();
            QueueRowPick::Ambiguous(numbers)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pr_authorship::tests::Fixture;
    use std::sync::atomic::Ordering;

    #[test]
    fn admitted_head_check_accepts_the_admitted_head_and_refuses_a_moved_or_unreadable_one() {
        let fixture = Fixture::new();
        let head = fixture.loc.git_out(&["rev-parse", "HEAD"]).unwrap();

        assert_eq!(admitted_head_still_checked_out(&fixture.loc, &head), Ok(()));
        let moved = admitted_head_still_checked_out(&fixture.loc, &"0".repeat(40));
        assert!(matches!(moved, Err(reason) if reason.contains("moved")));

        let empty = tempfile::tempdir().unwrap();
        std::fs::write(empty.path().join(".git"), "gitdir: missing\n").unwrap();
        let path = empty.path().to_str().unwrap();
        let unreadable = thegn_core::remote::GitLoc::from_db(path, None);
        let result = admitted_head_still_checked_out(&unreadable, &head);
        assert!(matches!(result, Err(reason) if reason.contains("could not be read")));
    }

    #[test]
    fn admitted_head_is_rechecked_after_admission_and_before_the_agent_spawns() {
        let production = include_str!("ci_autofix.rs");
        let production = production
            .split_once("\n#[cfg(test)]\nmod tests {")
            .map(|(production, _)| production)
            .unwrap();
        let admission = production
            .find("PrAgentGuard::admit(db, &request)")
            .unwrap();
        let recheck = production
            .find("admitted_head_still_checked_out(&loc, &entry.head_sha)")
            .unwrap();
        let spawn = production
            .find("let verdict = crate::agent_run::run_outcome_cancellable(")
            .unwrap();

        assert!(admission < recheck && recheck < spawn);
    }

    struct Handoff {
        db: thegn_core::db::Db,
        db_path: std::path::PathBuf,
        config: Config,
        forge: crate::pr_authorship::tests::FakeForge,
        entry: CiLogEntry,
        sentinel: std::path::PathBuf,
    }

    fn handoff(fixture: &Fixture, mode: CiAutofixMode, head_sha: &str) -> Handoff {
        let path = fixture.dir.path().to_str().unwrap();
        let db_path = fixture.dir.path().join("ci.db");
        let db = thegn_core::db::Db::open_at(&db_path).unwrap();
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
        let sentinel = fixture.dir.path().join("agent-ran");
        let mut config = Config::default();
        config.sandbox.isolation_floor = Some(thegn_core::config::IsolationFloor::Off);
        config.ci.autofix.mode = mode;
        config.pr_queue.enabled = true;
        config.pr_queue.agent_command = format!("touch {}", sentinel.display());
        let mut forge = fixture.forge(vec![fixture.proof.clone()]);
        forge.pr.status_check_rollup = vec![
            serde_json::from_value(serde_json::json!({"name":"test", "conclusion":"FAILURE"}))
                .unwrap(),
        ];
        let entry = CiLogEntry {
            worktree: path.into(),
            head_sha: head_sha.into(),
            text: "fixture failure".into(),
            run_id: "run".into(),
            job_id: "job".into(),
            ..Default::default()
        };
        Handoff {
            db,
            db_path,
            config,
            forge,
            entry,
            sentinel,
        }
    }

    fn consider(handoff: &Handoff, human_authorized: bool) {
        consider_candidate_with_forge(
            &handoff.config,
            &handoff.db,
            &handoff.entry,
            human_authorized,
            Some(&handoff.forge),
        );
    }

    #[test]
    fn off_mode_leaves_candidate_unclaimed_without_notifying_or_spending() {
        let fixture = Fixture::new();
        let handoff = handoff(&fixture, CiAutofixMode::Off, &fixture.pr.head_ref_oid);

        consider(&handoff, false);

        assert!(handoff.db.get_unread_notifications().unwrap().is_empty());
        assert!(!handoff.sentinel.exists());
        assert!(
            handoff
                .db
                .claim_ci_autofix(&handoff.entry.candidate())
                .unwrap()
        );
        assert_eq!(handoff.db.list_pr_queue().unwrap()[0].agent_attempts, 0);
    }

    #[test]
    fn stale_head_holds_auto_and_suggest_for_automatic_and_human_handoffs() {
        for mode in [CiAutofixMode::Auto, CiAutofixMode::Suggest] {
            for human_authorized in [false, true] {
                let fixture = Fixture::new();
                let handoff = handoff(&fixture, mode, "stale-head");

                consider(&handoff, human_authorized);

                assert!(!handoff.sentinel.exists());
                assert_eq!(handoff.db.list_pr_queue().unwrap()[0].agent_attempts, 0);
                assert!(
                    handoff
                        .db
                        .get_unread_notifications()
                        .unwrap()
                        .iter()
                        .any(|notification| notification.message.contains("PR head changed"))
                );
                assert!(
                    handoff
                        .db
                        .claim_ci_autofix(&handoff.entry.candidate())
                        .unwrap()
                );
            }
        }
    }

    #[test]
    fn exhausted_attempt_budget_notifies_without_dispatch() {
        let fixture = Fixture::new();
        let handoff = handoff(&fixture, CiAutofixMode::Auto, &fixture.pr.head_ref_oid);
        let row = handoff.db.list_pr_queue().unwrap().remove(0);
        handoff
            .db
            .set_pr_agent_attempts(&row.key, handoff.config.pr_queue.agent_max_attempts)
            .unwrap();

        consider(&handoff, false);

        assert!(!handoff.sentinel.exists());
        assert_eq!(
            handoff.db.list_pr_queue().unwrap()[0].agent_attempts,
            handoff.config.pr_queue.agent_max_attempts
        );
        assert!(
            handoff
                .db
                .get_unread_notifications()
                .unwrap()
                .iter()
                .any(|notification| notification.message.contains("budget is exhausted"))
        );
    }

    #[test]
    fn suggest_mode_notifies_without_claiming_or_spending_until_authorized() {
        let fixture = Fixture::new();
        let handoff = handoff(&fixture, CiAutofixMode::Suggest, &fixture.pr.head_ref_oid);

        consider(&handoff, false);

        assert!(!handoff.sentinel.exists());
        assert_eq!(handoff.db.list_pr_queue().unwrap()[0].agent_attempts, 0);
        assert!(
            handoff
                .db
                .claim_ci_autofix(&handoff.entry.candidate())
                .unwrap()
        );
        assert!(
            handoff
                .db
                .get_unread_notifications()
                .unwrap()
                .iter()
                .any(|notification| notification
                    .message
                    .contains("ready for a PR-agent handoff"))
        );
    }

    #[test]
    fn authorized_auto_and_suggest_dispatch_once_and_restamp_the_row() {
        for (mode, human_authorized) in
            [(CiAutofixMode::Auto, false), (CiAutofixMode::Suggest, true)]
        {
            let fixture = Fixture::new();
            let handoff = handoff(&fixture, mode, &fixture.pr.head_ref_oid);

            consider(&handoff, human_authorized);

            assert!(handoff.sentinel.exists(), "{mode:?}");
            let row = handoff.db.list_pr_queue().unwrap().remove(0);
            assert_eq!(row.agent_attempts, 1, "{mode:?}");
            assert_eq!(row.status, PrqStatus::BlockedCi.as_str(), "{mode:?}");
            std::fs::remove_file(&handoff.sentinel).unwrap();

            consider(&handoff, human_authorized);

            assert!(!handoff.sentinel.exists(), "duplicate ran for {mode:?}");
            assert_eq!(handoff.db.list_pr_queue().unwrap()[0].agent_attempts, 1);
        }
    }

    #[test]
    fn after_agent_identity_change_skips_the_own_head_record_and_holds_the_row() {
        let fixture = Fixture::new();
        let mut handoff = handoff(&fixture, CiAutofixMode::Auto, &fixture.pr.head_ref_oid);
        handoff.config.pr_queue.own_prs_only = true;
        let mut foreign = fixture.proof.clone();
        foreign.viewer.id = "U_someone_else".into();
        handoff.forge = fixture.forge(vec![fixture.proof.clone(), fixture.proof.clone(), foreign]);
        handoff.forge.pr.status_check_rollup = vec![
            serde_json::from_value(serde_json::json!({"name":"test", "conclusion":"FAILURE"}))
                .unwrap(),
        ];

        consider(&handoff, false);

        assert!(
            handoff.sentinel.exists(),
            "the agent should run before post-agent recheck"
        );
        assert_eq!(
            handoff.forge.proof_calls.load(Ordering::SeqCst),
            3,
            "expected acquire, pre-dispatch revalidate, and post-agent proof calls"
        );
        let row = handoff.db.list_pr_queue().unwrap().remove(0);
        assert_eq!(row.agent_attempts, 1);
        assert_eq!(row.status, PrqStatus::NeedsHuman.as_str());
        assert!(
            row.detail
                .as_deref()
                .is_some_and(|detail| detail.contains("held after the agent ran"))
        );
        assert!(
            handoff
                .db
                .get_unread_notifications()
                .unwrap()
                .iter()
                .any(|notification| notification.message.contains("held after the agent ran"))
        );
    }

    #[test]
    fn after_agent_unchanged_identity_keeps_the_existing_finish() {
        let fixture = Fixture::new();
        let mut handoff = handoff(&fixture, CiAutofixMode::Auto, &fixture.pr.head_ref_oid);
        handoff.config.pr_queue.own_prs_only = true;

        consider(&handoff, false);

        let row = handoff.db.list_pr_queue().unwrap().remove(0);
        assert_eq!(row.status, PrqStatus::BlockedCi.as_str());
        assert!(
            row.detail
                .as_deref()
                .is_some_and(|detail| detail.starts_with("CI autofix finished"))
        );
    }

    #[test]
    fn unchanged_head_is_noted_in_the_finished_row() {
        let fixture = Fixture::new();
        let handoff = handoff(&fixture, CiAutofixMode::Auto, &fixture.pr.head_ref_oid);

        consider(&handoff, false);

        let row = handoff.db.list_pr_queue().unwrap().remove(0);
        assert_eq!(row.status, PrqStatus::BlockedCi.as_str());
        let detail = row.detail.as_deref().unwrap();
        assert!(detail.starts_with("CI autofix finished"));
        assert!(detail.contains(
            "PR #7: the agent finished without changing the PR head; the next refresh re-evaluates the row"
        ));
    }

    #[test]
    fn failed_verdict_keeps_unchanged_head_note_in_the_finished_row() {
        let fixture = Fixture::new();
        let mut handoff = handoff(&fixture, CiAutofixMode::Auto, &fixture.pr.head_ref_oid);
        handoff.config.pr_queue.agent_command = "exit 1".into();

        consider(&handoff, false);

        let row = handoff.db.list_pr_queue().unwrap().remove(0);
        assert_eq!(row.status, PrqStatus::BlockedCi.as_str());
        let detail = row.detail.as_deref().unwrap();
        assert!(detail.contains("CI autofix finished (exit failed)"));
        assert!(detail.contains(
            "PR #7: the agent finished without changing the PR head; the next refresh re-evaluates the row"
        ));
    }

    #[test]
    fn local_only_head_note_reaches_the_finished_row_detail() {
        let fixture = Fixture::new();
        let mut handoff = handoff(&fixture, CiAutofixMode::Auto, &fixture.pr.head_ref_oid);
        handoff.config.pr_queue.agent_command = "git -c user.name=Agent -c user.email=agent@example.invalid -c commit.gpgsign=false commit --allow-empty -m agent".into();

        consider(&handoff, false);

        let row = handoff.db.list_pr_queue().unwrap().remove(0);
        assert_eq!(row.status, PrqStatus::BlockedCi.as_str());
        let detail = row.detail.as_deref().unwrap();
        assert!(detail.contains("CI autofix finished (exit ok)"));
        assert!(detail.contains(
            "PR #7: the agent committed locally but the PR head did not move; nothing was pushed"
        ));
    }

    #[test]
    fn foreign_push_head_note_reaches_finish_detail() {
        let fixture = Fixture::new();
        let admitted = fixture.pr.head_ref_oid.clone();
        let foreign = "f".repeat(40);
        let note = crate::pr_head_outcome::run_warning(
            &fixture.loc,
            7,
            &admitted,
            Some(&foreign),
            true,
            &thegn_core::config_pr_queue::PrQueueConfig::default(),
        )
        .unwrap();
        let detail = autofix_finish_detail(
            &crate::agent_run::AgentRunVerdict::Exited(Some(0)),
            Some(&note),
        );

        assert!(detail.contains("CI autofix finished (exit ok)"));
        assert!(detail.contains("PR #7: someone else pushed to the PR while the agent ran"));
    }

    #[test]
    fn reattest_failure_never_claims_or_spends() {
        let fixture = Fixture::new();
        let handoff = handoff(&fixture, CiAutofixMode::Auto, &fixture.pr.head_ref_oid);
        assert!(
            fixture
                .loc
                .git_out(&["checkout", "-b", "moved-checkout"])
                .is_none()
        );
        assert_eq!(
            fixture
                .loc
                .git_out(&["rev-parse", "--abbrev-ref", "HEAD"])
                .as_deref(),
            Some("moved-checkout")
        );

        consider(&handoff, false);

        assert!(!handoff.sentinel.exists());
        assert_eq!(handoff.db.list_pr_queue().unwrap()[0].agent_attempts, 0);
        assert!(
            handoff
                .db
                .claim_ci_autofix(&handoff.entry.candidate())
                .unwrap(),
            "reattest refusal must happen before the dedupe claim"
        );
    }

    #[test]
    fn repeated_refusal_keeps_one_unread_notification_for_candidate() {
        let fixture = Fixture::new();
        let handoff = handoff(&fixture, CiAutofixMode::Auto, "stale-head");

        consider(&handoff, false);
        consider(&handoff, false);

        let notifications = handoff.db.get_unread_notifications().unwrap();
        assert_eq!(notifications.len(), 1);
        assert_eq!(
            notifications[0].source_ref,
            format!(
                "ci-autofix:{}:{}:{}:{}",
                handoff.entry.worktree,
                handoff.entry.run_id,
                handoff.entry.job_id,
                handoff.entry.head_sha
            )
        );
        assert_eq!(
            notifications[0].kind,
            thegn_core::notification::NotificationKind::PrQueueNeedsHuman
        );
    }

    #[test]
    fn no_pr_context_groups_failed_jobs_by_run_and_supersedes_older_runs() {
        let fixture = Fixture::new();
        let handoff = handoff(&fixture, CiAutofixMode::Auto, &fixture.pr.head_ref_oid);
        let row = handoff.db.list_pr_queue().unwrap().remove(0);
        handoff.db.remove_pr_entry(&row.key).unwrap();

        for (job_id, job_name) in [("job-1", "linux"), ("job-2", "macos"), ("job-3", "windows")] {
            let mut entry = handoff.entry.clone();
            entry.job_id = job_id.into();
            entry.job_name = job_name.into();
            consider_candidate_with_forge(&handoff.config, &handoff.db, &entry, false, None);
        }

        let notifications = handoff.db.get_all_notifications(usize::MAX).unwrap();
        assert_eq!(notifications.len(), 1);
        assert_eq!(
            notifications[0].kind,
            thegn_core::notification::NotificationKind::CiRunFailed
        );
        assert_eq!(
            notifications[0].source_ref,
            format!("ci-run:{}:run", handoff.entry.worktree)
        );
        for job_name in ["linux", "macos", "windows"] {
            assert!(notifications[0].message.contains(job_name));
        }

        let mut next_run = handoff.entry.clone();
        next_run.run_id = "next-run".into();
        next_run.job_id = "job-4".into();
        next_run.job_name = "nightly".into();
        consider_candidate_with_forge(&handoff.config, &handoff.db, &next_run, false, None);

        let all = handoff.db.get_all_notifications(usize::MAX).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(
            all.iter().filter(|notification| !notification.read).count(),
            1
        );
        assert!(all.iter().any(|notification| {
            notification.source_ref == format!("ci-run:{}:run", handoff.entry.worktree)
                && notification.read
        }));
        assert!(all.iter().any(|notification| {
            notification.source_ref == format!("ci-run:{}:next-run", handoff.entry.worktree)
                && !notification.read
        }));
    }

    #[test]
    fn no_pr_context_ignores_a_late_numeric_older_run() {
        let db = thegn_core::db::Db::open_memory().unwrap();
        let mut newer = CiLogEntry {
            worktree: "/wt/ci".into(),
            run_id: "200".into(),
            job_name: "linux".into(),
            ..Default::default()
        };
        notify_ci_run_failed(&db, &newer);

        newer.run_id = "100".into();
        newer.job_name = "macos".into();
        notify_ci_run_failed(&db, &newer);

        let notifications = db.get_all_notifications(usize::MAX).unwrap();
        assert_eq!(notifications.len(), 1);
        assert_eq!(notifications[0].source_ref, "ci-run:/wt/ci:200");
        assert!(!notifications[0].read);
        assert!(!notifications[0].message.contains("macos"));
    }

    #[test]
    // Positive control: legacy notification references still preserve provider context.
    fn notification_candidate_keeps_the_provider_context() {
        let notification = thegn_core::notification::Notification {
            id: 1,
            kind: thegn_core::notification::NotificationKind::PrQueueNeedsHuman,
            source_ref: "ci-autofix:/wt/repo:run-7:job-3:abc123:github:github.com/o/r".into(),
            message: "needs review".into(),
            created_at_ms: 1,
            read: false,
            worktree_path: "/wt/repo".into(),
        };
        let candidate = candidate_from_notification(&notification).unwrap();
        assert_eq!(candidate.head_sha, "abc123");
        assert_eq!(candidate.context, "github:github.com/o/r");
        let legacy = thegn_core::notification::Notification {
            source_ref: "ci-autofix:/wt/repo:run-7:job-3:abc123".into(),
            ..notification
        };
        assert_eq!(candidate_from_notification(&legacy).unwrap().context, "");
    }

    #[test]
    fn authorize_refuses_a_log_cached_for_another_provider_context() {
        let fixture = Fixture::new();
        let mut handoff = handoff(&fixture, CiAutofixMode::Auto, &fixture.pr.head_ref_oid);
        handoff.entry.context = "github:github.com/acme/other".into();

        authorize_entry(
            &handoff.config,
            &handoff.db,
            &handoff.entry,
            Some("github:github.com/acme/repo"),
            Some(&handoff.forge),
        );

        assert!(!handoff.sentinel.exists());
        assert_eq!(handoff.db.list_pr_queue().unwrap()[0].agent_attempts, 0);
        assert!(
            handoff
                .db
                .claim_ci_autofix(&handoff.entry.candidate())
                .unwrap()
        );
        let notifications = handoff.db.get_unread_notifications().unwrap();
        assert_eq!(notifications.len(), 1);
        assert!(
            notifications[0]
                .message
                .contains("another provider context")
        );
        assert_eq!(
            notifications[0].source_ref,
            format!(
                "ci-autofix:{}:{}:{}:{}:{}",
                handoff.entry.worktree,
                handoff.entry.run_id,
                handoff.entry.job_id,
                handoff.entry.head_sha,
                handoff.entry.context
            )
        );
    }

    #[test]
    fn authorize_off_mode_stays_silent_for_a_context_mismatch() {
        let fixture = Fixture::new();
        let mut handoff = handoff(&fixture, CiAutofixMode::Off, &fixture.pr.head_ref_oid);
        handoff.entry.context = "github:github.com/acme/other".into();

        authorize_entry(
            &handoff.config,
            &handoff.db,
            &handoff.entry,
            Some("github:github.com/acme/repo"),
            Some(&handoff.forge),
        );

        assert!(handoff.db.get_unread_notifications().unwrap().is_empty());
        assert!(!handoff.sentinel.exists());
        assert_eq!(handoff.db.list_pr_queue().unwrap()[0].agent_attempts, 0);
    }

    #[test]
    fn authorize_remote_provider_wrapper_refuses_known_context_until_refreshed() {
        let fixture = Fixture::new();
        let mut handoff = handoff(&fixture, CiAutofixMode::Auto, &fixture.pr.head_ref_oid);
        handoff.entry.context = "github:github.com/acme/other".into();
        handoff.db.put_ci_log(&handoff.entry).unwrap();
        let provider_location = serde_json::json!({
            "control_prefix": ["sprite", "exec", "--"],
            "path": "/workspace"
        })
        .to_string();
        handoff
            .db
            .set_worktree_location(&handoff.entry.worktree, &provider_location)
            .unwrap();

        authorize(&handoff.config, &handoff.db, &handoff.entry.candidate());

        assert!(!handoff.sentinel.exists());
        assert_eq!(handoff.db.list_pr_queue().unwrap()[0].agent_attempts, 0);
        let notifications = handoff.db.get_unread_notifications().unwrap();
        assert_eq!(notifications.len(), 1);
        assert!(
            notifications[0]
                .message
                .contains("another provider context")
        );
    }

    #[test]
    fn authorize_context_refusal_does_not_replace_the_existing_candidate_notification() {
        let fixture = Fixture::new();
        let mut handoff = handoff(&fixture, CiAutofixMode::Suggest, &fixture.pr.head_ref_oid);
        handoff.entry.context = "github:github.com/acme/other".into();
        consider_candidate_with_forge(
            &handoff.config,
            &handoff.db,
            &handoff.entry,
            false,
            Some(&handoff.forge),
        );

        authorize_entry(
            &handoff.config,
            &handoff.db,
            &handoff.entry,
            Some("github:github.com/acme/repo"),
            Some(&handoff.forge),
        );

        let notifications = handoff.db.get_unread_notifications().unwrap();
        assert_eq!(notifications.len(), 1);
        assert!(
            notifications[0]
                .message
                .contains("ready for a PR-agent handoff")
        );
        assert!(!handoff.sentinel.exists());
        assert_eq!(handoff.db.list_pr_queue().unwrap()[0].agent_attempts, 0);
    }

    #[test]
    fn authorize_refuses_a_known_log_context_when_the_current_context_is_unknown() {
        let fixture = Fixture::new();
        let mut handoff = handoff(&fixture, CiAutofixMode::Auto, &fixture.pr.head_ref_oid);
        handoff.entry.context = "github:github.com/acme/other".into();

        authorize_entry(
            &handoff.config,
            &handoff.db,
            &handoff.entry,
            None,
            Some(&handoff.forge),
        );

        assert!(!handoff.sentinel.exists());
        assert_eq!(handoff.db.list_pr_queue().unwrap()[0].agent_attempts, 0);
        assert!(
            handoff
                .db
                .claim_ci_autofix(&handoff.entry.candidate())
                .unwrap()
        );
        let notifications = handoff.db.get_unread_notifications().unwrap();
        assert_eq!(notifications.len(), 1);
        assert!(
            notifications[0]
                .message
                .contains("another provider context")
        );
    }

    #[test]
    fn authorize_refuses_an_unknown_log_context_when_the_current_context_is_known() {
        let fixture = Fixture::new();
        let handoff = handoff(&fixture, CiAutofixMode::Auto, &fixture.pr.head_ref_oid);

        authorize_entry(
            &handoff.config,
            &handoff.db,
            &handoff.entry,
            Some("github:github.com/acme/repo"),
            Some(&handoff.forge),
        );

        assert!(!handoff.sentinel.exists());
        assert_eq!(handoff.db.list_pr_queue().unwrap()[0].agent_attempts, 0);
        assert!(
            handoff
                .db
                .claim_ci_autofix(&handoff.entry.candidate())
                .unwrap()
        );
        let notifications = handoff.db.get_unread_notifications().unwrap();
        assert_eq!(notifications.len(), 1);
        assert!(
            notifications[0]
                .message
                .contains("another provider context")
        );
    }

    #[test]
    fn authorize_dispatches_when_the_log_context_matches() {
        let fixture = Fixture::new();
        let mut handoff = handoff(&fixture, CiAutofixMode::Suggest, &fixture.pr.head_ref_oid);
        handoff.entry.context = "github:github.com/acme/repo".into();

        authorize_entry(
            &handoff.config,
            &handoff.db,
            &handoff.entry,
            Some("github:github.com/acme/repo"),
            Some(&handoff.forge),
        );

        assert!(handoff.sentinel.exists());
        assert_eq!(handoff.db.list_pr_queue().unwrap()[0].agent_attempts, 1);
    }

    #[test]
    fn authorize_dispatches_when_both_contexts_are_unknown() {
        let fixture = Fixture::new();
        let handoff = handoff(&fixture, CiAutofixMode::Auto, &fixture.pr.head_ref_oid);

        authorize_entry(
            &handoff.config,
            &handoff.db,
            &handoff.entry,
            None,
            Some(&handoff.forge),
        );

        assert!(handoff.sentinel.exists());
        assert_eq!(handoff.db.list_pr_queue().unwrap()[0].agent_attempts, 1);
    }

    #[test]
    fn candidate_ref_id_round_trips_through_the_notification_parser() {
        let candidate = thegn_core::ci_log::CiLogCandidate {
            worktree: "/wt/repo".into(),
            run_id: "run-7".into(),
            job_id: "job-3".into(),
            head_sha: "abc123".into(),
            context: "github:github.com/o/r".into(),
            attempt: None,
        };
        let mut notification = thegn_core::notification::Notification {
            id: 1,
            kind: thegn_core::notification::NotificationKind::PrQueueNeedsHuman,
            source_ref: candidate_ref_id(&candidate),
            message: "m".into(),
            created_at_ms: 1,
            read: false,
            worktree_path: "/wt/repo".into(),
        };
        assert_eq!(
            candidate_from_notification(&notification).unwrap(),
            candidate
        );

        let legacy = thegn_core::ci_log::CiLogCandidate {
            worktree: "/wt/repo".into(),
            run_id: "run-7".into(),
            job_id: "job-3".into(),
            head_sha: "abc123".into(),
            context: String::new(),
            attempt: None,
        };
        notification.source_ref = candidate_ref_id(&legacy);
        assert_eq!(candidate_from_notification(&notification).unwrap(), legacy);
    }

    #[test]
    fn candidate_ref_id_round_trips_the_run_attempt() {
        let candidates = [
            thegn_core::ci_log::CiLogCandidate {
                worktree: "/wt/repo".into(),
                run_id: "run-7".into(),
                job_id: "job-3".into(),
                head_sha: "abc123".into(),
                context: "github:github.com/o/r".into(),
                attempt: Some(3),
            },
            thegn_core::ci_log::CiLogCandidate {
                worktree: "/wt/repo".into(),
                run_id: "run-7".into(),
                job_id: "job-3".into(),
                head_sha: "abc123".into(),
                context: String::new(),
                attempt: Some(3),
            },
            thegn_core::ci_log::CiLogCandidate {
                worktree: "/wt/repo".into(),
                run_id: "run-7".into(),
                job_id: "job-3".into(),
                head_sha: "abc123".into(),
                context: "github:github.com/o/r".into(),
                attempt: None,
            },
        ];
        let expected = [
            "ci-autofix:/wt/repo:run-7:job-3:abc123:@3:github:github.com/o/r",
            "ci-autofix:/wt/repo:run-7:job-3:abc123:@3",
            "ci-autofix:/wt/repo:run-7:job-3:abc123:github:github.com/o/r",
        ];
        for (candidate, expected_ref) in candidates.iter().zip(expected) {
            let notification = thegn_core::notification::Notification {
                id: 1,
                kind: thegn_core::notification::NotificationKind::PrQueueNeedsHuman,
                source_ref: candidate_ref_id(candidate),
                message: "m".into(),
                created_at_ms: 1,
                read: false,
                worktree_path: "/wt/repo".into(),
            };
            assert_eq!(notification.source_ref, expected_ref);
            assert_eq!(
                candidate_from_notification(&notification).unwrap(),
                *candidate
            );
        }
    }

    #[test]
    fn candidate_from_notification_rejects_a_malformed_attempt() {
        for source_ref in [
            "ci-autofix:/wt/repo:run-7:job-3:abc123:@x:github:github.com/o/r",
            "ci-autofix:/wt/repo:run-7:job-3:abc123:@:ctx",
            "ci-autofix:/wt/repo:run-7:job-3:abc123:@-1",
        ] {
            let notification = thegn_core::notification::Notification {
                id: 1,
                kind: thegn_core::notification::NotificationKind::PrQueueNeedsHuman,
                source_ref: source_ref.into(),
                message: "m".into(),
                created_at_ms: 1,
                read: false,
                worktree_path: "/wt/repo".into(),
            };
            assert!(candidate_from_notification(&notification).is_none());
        }
    }

    #[test]
    fn failed_attempt_spend_prevents_ci_agent_spawn() {
        let fixture = Fixture::new();
        let handoff = handoff(&fixture, CiAutofixMode::Auto, &fixture.pr.head_ref_oid);
        let conn = rusqlite::Connection::open(&handoff.db_path).unwrap();
        conn.execute_batch(
            "CREATE TRIGGER deny_attempts BEFORE UPDATE OF agent_attempts ON pr_queue BEGIN SELECT RAISE(ABORT, 'denied'); END;",
        )
        .unwrap();

        consider(&handoff, false);

        let row = handoff.db.list_pr_queue().unwrap().remove(0);
        assert!(!handoff.sentinel.exists());
        assert_eq!(row.agent_attempts, 0);
        assert_ne!(row.status, PrqStatus::AgentRunning.as_str());
        assert!(
            handoff
                .db
                .get_unread_notifications()
                .unwrap()
                .iter()
                .any(|notification| notification.message.contains("held")
                    && notification
                        .message
                        .contains("could not record the dispatch"))
        );
        assert!(
            handoff
                .db
                .claim_ci_autofix(&handoff.entry.candidate())
                .unwrap()
        );
    }

    #[test]
    fn failed_running_stamp_spends_nothing_and_leaves_candidate_unclaimed() {
        let fixture = Fixture::new();
        let handoff = handoff(&fixture, CiAutofixMode::Auto, &fixture.pr.head_ref_oid);
        let conn = rusqlite::Connection::open(&handoff.db_path).unwrap();
        conn.execute_batch(
            "CREATE TRIGGER deny_status BEFORE UPDATE OF status ON pr_queue BEGIN SELECT RAISE(ABORT, 'denied'); END;",
        )
        .unwrap();

        consider(&handoff, false);

        let row = handoff.db.list_pr_queue().unwrap().remove(0);
        assert!(!handoff.sentinel.exists());
        assert_eq!(row.agent_attempts, 0);
        assert_ne!(row.status, PrqStatus::AgentRunning.as_str());
        assert!(
            handoff
                .db
                .get_unread_notifications()
                .unwrap()
                .iter()
                .any(|notification| notification.message.contains("held")
                    && notification
                        .message
                        .contains("could not record the dispatch"))
        );
        assert!(
            handoff
                .db
                .claim_ci_autofix(&handoff.entry.candidate())
                .unwrap()
        );
    }

    #[test]
    fn live_lease_fences_concurrent_status_writes_until_release() {
        let fixture = Fixture::new();
        let path = fixture.dir.path().to_str().unwrap();
        let db = thegn_core::db::Db::open_at(&fixture.dir.path().join("ci.db")).unwrap();
        db.enqueue_pr(path, 7, Some(path), "fixture", "main", "github")
            .unwrap();
        let row = db.list_pr_queue().unwrap().remove(0);
        let request = thegn_core::pr_queue::PrAgentAdmit {
            key: &row.key,
            expected_attempts: 0,
            expected_checkout_gen: row.checkout_gen,
            max_attempts: 3,
            head: "initial-head",
            blocker: "ci",
            detail: "dispatch",
            lease_until_ms: crate::pr_agent_lease::lease_until_ms(),
            owner_hint: Some("test"),
            ci_candidate: None,
        };
        let guard = crate::pr_agent_lease::PrAgentGuard::admit(&db, &request)
            .unwrap()
            .unwrap();
        db.update_pr_status(
            &row.key,
            PrqStatus::Merged.as_str(),
            Some("ci"),
            None,
            Some("newer-head"),
        )
        .unwrap();

        let during_lease = db.list_pr_queue().unwrap().remove(0);
        assert_eq!(during_lease.status, PrqStatus::AgentRunning.as_str());
        assert_eq!(during_lease.last_head_oid.as_deref(), Some("initial-head"));

        assert!(
            guard
                .finish(&db, Some((PrqStatus::BlockedCi.as_str(), "finished")))
                .unwrap()
        );

        let current = db.list_pr_queue().unwrap().remove(0);
        assert_eq!(current.status, PrqStatus::BlockedCi.as_str());
        assert_eq!(current.last_head_oid.as_deref(), Some("initial-head"));

        let request = thegn_core::pr_queue::PrAgentAdmit {
            key: &row.key,
            expected_attempts: current.agent_attempts,
            expected_checkout_gen: row.checkout_gen,
            max_attempts: 3,
            head: "admitted-head",
            blocker: "ci",
            detail: "dispatch",
            lease_until_ms: crate::pr_agent_lease::lease_until_ms(),
            owner_hint: Some("test"),
            ci_candidate: None,
        };
        let guard = crate::pr_agent_lease::PrAgentGuard::admit(&db, &request)
            .unwrap()
            .unwrap();
        db.update_pr_status(
            &row.key,
            PrqStatus::AgentRunning.as_str(),
            Some("ci"),
            None,
            Some("newer-head"),
        )
        .unwrap();
        let during_lease = db.list_pr_queue().unwrap().remove(0);
        assert_eq!(during_lease.status, PrqStatus::AgentRunning.as_str());
        assert_eq!(during_lease.last_head_oid.as_deref(), Some("admitted-head"));
        assert!(
            guard
                .finish(&db, Some((PrqStatus::BlockedCi.as_str(), "finished")))
                .unwrap()
        );

        let current = db.list_pr_queue().unwrap().remove(0);
        assert_eq!(current.status, PrqStatus::BlockedCi.as_str());
        assert_eq!(current.last_head_oid.as_deref(), Some("admitted-head"));
    }

    #[test]
    fn unbound_or_mismatched_pr_identity_never_reaches_ci_dispatch() {
        let cases = [
            ("unbound", false, false, "identity"),
            ("project-swap", true, false, "identity"),
            ("forge-mismatch", true, true, "forge"),
        ];
        for (case, bound, forge_mismatch, expected_message) in cases {
            for human_authorized in [false, true] {
                let fixture = Fixture::new();
                let path = fixture.dir.path().to_str().unwrap();
                let db = thegn_core::db::Db::open_at(&fixture.dir.path().join("ci.db")).unwrap();
                if bound {
                    let url = if case == "project-swap" {
                        "https://github.com/other/project/pull/7"
                    } else {
                        &fixture.pr.url
                    };
                    db.enqueue_bound_pr(
                        path,
                        7,
                        Some(path),
                        "fixture",
                        "main",
                        if forge_mismatch { "gitlab" } else { "github" },
                        url,
                    )
                    .unwrap();
                } else {
                    db.enqueue_pr(path, 7, Some(path), "fixture", "main", "github")
                        .unwrap();
                }
                let sentinel = fixture.dir.path().join("agent-must-not-run");
                let mut cfg = Config::default();
                cfg.ci.autofix.mode = CiAutofixMode::Auto;
                cfg.pr_queue.enabled = true;
                cfg.pr_queue.own_prs_only = true;
                cfg.pr_queue.agent_command = format!("touch {}", sentinel.display());
                let mut forge = fixture.forge(vec![fixture.proof.clone()]);
                forge.pr.status_check_rollup = vec![
                    serde_json::from_value(
                        serde_json::json!({"name":"test", "conclusion":"FAILURE"}),
                    )
                    .unwrap(),
                ];
                let entry = CiLogEntry {
                    worktree: path.into(),
                    head_sha: fixture.pr.head_ref_oid.clone(),
                    text: "fixture failure".into(),
                    run_id: "run".into(),
                    job_id: "job".into(),
                    ..Default::default()
                };

                consider_candidate_with_forge(&cfg, &db, &entry, human_authorized, Some(&forge));

                assert_eq!(forge.proof_calls.load(Ordering::SeqCst), 0, "{case}");
                assert!(!sentinel.exists(), "{case}");
                let row = db.list_pr_queue().unwrap().remove(0);
                assert_eq!(row.agent_attempts, 0, "{case}");
                assert_eq!(row.status, PrqStatus::NeedsHuman.as_str(), "{case}");
                assert!(db.claim_ci_autofix(&entry.candidate()).unwrap(), "{case}");
                assert!(
                    db.get_unread_notifications().unwrap().iter().any(|n| {
                        n.message.contains("held") && n.message.contains(expected_message)
                    }),
                    "missing {expected_message} hold for {case}, human={human_authorized}"
                );
            }
        }
    }

    #[test]
    fn foreign_author_never_claims_or_spends_a_ci_attempt() {
        for mismatched_number in [false, true] {
            let fixture = Fixture::new();
            let path = fixture.dir.path().to_str().unwrap();
            let db = thegn_core::db::Db::open_at(&fixture.dir.path().join("ci.db")).unwrap();
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
            let sentinel = fixture.dir.path().join("agent-must-not-run");
            let mut cfg = Config::default();
            cfg.ci.autofix.mode = CiAutofixMode::Auto;
            cfg.pr_queue.enabled = true;
            cfg.pr_queue.agent_command = format!("touch {}", sentinel.display());
            cfg.pr_queue.own_prs_only = true;
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
            let entry = CiLogEntry {
                worktree: path.into(),
                head_sha: fixture.pr.head_ref_oid.clone(),
                text: "fixture failure".into(),
                run_id: "run".into(),
                job_id: "job".into(),
                ..Default::default()
            };
            consider_candidate_with_forge(&cfg, &db, &entry, false, Some(&forge));
            assert_eq!(
                forge.proof_calls.load(Ordering::SeqCst),
                usize::from(!mismatched_number)
            );
            assert!(!sentinel.exists());
            assert_eq!(db.list_pr_queue().unwrap()[0].agent_attempts, 0);
            assert!(
                db.claim_ci_autofix(&entry.candidate()).unwrap(),
                "denial must not consume the candidate claim"
            );
            assert!(db.get_unread_notifications().unwrap().iter().any(|n| {
                n.message.contains(if mismatched_number {
                    "identity"
                } else {
                    "own-PR automation held"
                })
            }));
        }
    }

    #[test]
    fn every_shared_policy_hold_blocks_automatic_and_human_ci_handoffs() {
        let cases = [
            ("needs_human", false, false, "needs_human"),
            ("closed", false, false, "closed"),
            ("merged", false, false, "merged"),
            ("agent_running", false, false, "agent_running"),
            ("merging", false, false, "merging"),
            ("blocked_ci", true, false, "CI is not in [pr_queue] watch"),
            ("blocked_ci", false, true, "someone else pushed"),
        ];
        for (status, exclude_ci, foreign_push, reason) in cases {
            for mode in [CiAutofixMode::Auto, CiAutofixMode::Suggest] {
                for human_authorized in [false, true] {
                    let fixture = Fixture::new();
                    let path = fixture.dir.path().to_str().unwrap();
                    let previous_head = if foreign_push {
                        #[expect(
                            clippy::disallowed_methods,
                            reason = "private synchronous git setup inside the temporary fixture"
                        )]
                        let output = fixture
                            .loc
                            .git_command(&[
                                "-c",
                                "user.name=Fixture",
                                "-c",
                                "user.email=fixture@example.invalid",
                                "-c",
                                "commit.gpgsign=false",
                                "commit",
                                "--allow-empty",
                                "-m",
                                "teammate",
                            ])
                            .output()
                            .unwrap();
                        assert!(output.status.success());
                        let previous = fixture.loc.git_out(&["rev-parse", "HEAD"]).unwrap();
                        #[expect(
                            clippy::disallowed_methods,
                            reason = "restore the temporary fixture checkout to the fetched PR head"
                        )]
                        let reset = fixture
                            .loc
                            .git_command(&["reset", "--hard", &fixture.pr.head_ref_oid])
                            .output()
                            .unwrap();
                        assert!(reset.status.success());
                        Some(previous)
                    } else {
                        None
                    };
                    let db =
                        thegn_core::db::Db::open_at(&fixture.dir.path().join("ci.db")).unwrap();
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
                        status,
                        Some("ci"),
                        None,
                        previous_head
                            .as_deref()
                            .or(Some(fixture.pr.head_ref_oid.as_str())),
                    )
                    .unwrap();

                    let sentinel = fixture.dir.path().join("agent-must-not-run");
                    let mut cfg = Config::default();
                    cfg.ci.autofix.mode = mode;
                    cfg.pr_queue.enabled = true;
                    cfg.pr_queue.agent_command = format!("touch {}", sentinel.display());
                    if exclude_ci {
                        cfg.pr_queue.watch =
                            vec![thegn_core::config_pr_queue::PrWatchKind::Conflict];
                    }
                    let mut forge = fixture.forge(vec![fixture.proof.clone()]);
                    forge.pr.status_check_rollup = vec![
                        serde_json::from_value(
                            serde_json::json!({"name":"test", "conclusion":"FAILURE"}),
                        )
                        .unwrap(),
                    ];
                    let entry = CiLogEntry {
                        worktree: path.into(),
                        head_sha: fixture.pr.head_ref_oid.clone(),
                        text: "fixture failure".into(),
                        run_id: "run".into(),
                        job_id: "job".into(),
                        ..Default::default()
                    };

                    consider_candidate_with_forge(
                        &cfg,
                        &db,
                        &entry,
                        human_authorized,
                        Some(&forge),
                    );

                    assert!(!sentinel.exists(), "{status}, human={human_authorized}");
                    assert_eq!(db.list_pr_queue().unwrap()[0].agent_attempts, 0);
                    assert!(db.claim_ci_autofix(&entry.candidate()).unwrap());
                    assert_eq!(forge.proof_calls.load(Ordering::SeqCst), 0);
                    assert!(
                        db.get_unread_notifications()
                            .unwrap()
                            .iter()
                            .any(|n| { n.message.contains(reason) }),
                        "missing reason `{reason}` for {status}, mode={mode:?}, human={human_authorized}"
                    );
                }
            }
        }
    }

    #[test]
    fn missing_history_cells_dispatch_like_a_current_head_and_unknown_heads_hold() {
        let cells = [
            ("NULL last head", None),
            ("empty last head", Some("empty")),
            ("current last head", Some("current")),
            ("unknown older last head", Some("unknown")),
        ];
        let mut failures = Vec::new();

        for (cell, recorded) in cells {
            for mode in [CiAutofixMode::Auto, CiAutofixMode::Suggest] {
                for human_authorized in [false, true] {
                    let fixture = Fixture::new();
                    let handoff = handoff(&fixture, mode, &fixture.pr.head_ref_oid);

                    if let Some(recorded) = recorded {
                        let row = handoff.db.list_pr_queue().unwrap().remove(0);
                        let unknown_head = "0".repeat(40);
                        let head = match recorded {
                            "empty" => "",
                            "current" => fixture.pr.head_ref_oid.as_str(),
                            "unknown" => unknown_head.as_str(),
                            _ => unreachable!("test case has a known recorded-head marker"),
                        };
                        handoff
                            .db
                            .update_pr_status(&row.key, &row.status, None, None, Some(head))
                            .unwrap();
                    }

                    consider(&handoff, human_authorized);

                    let held = cell == "unknown older last head";
                    let dispatched = !held && (mode == CiAutofixMode::Auto || human_authorized);
                    let expected_attempts = u32::from(dispatched);
                    let ran = handoff.sentinel.exists();
                    if ran != dispatched {
                        failures.push(format!(
                            "{cell}, mode={mode:?}, human={human_authorized}: ran={ran}, expected={dispatched}"
                        ));
                    }

                    let attempts = handoff.db.list_pr_queue().unwrap()[0].agent_attempts;
                    if attempts != expected_attempts {
                        failures.push(format!(
                            "{cell}, mode={mode:?}, human={human_authorized}: attempts={attempts}, expected={expected_attempts}"
                        ));
                    }

                    let claimable = handoff
                        .db
                        .claim_ci_autofix(&handoff.entry.candidate())
                        .unwrap();
                    let expected_claimable = !dispatched;
                    if claimable != expected_claimable {
                        failures.push(format!(
                            "{cell}, mode={mode:?}, human={human_authorized}: claimable={claimable}, expected={}",
                            expected_claimable
                        ));
                    }

                    let expected_notice = if held {
                        Some("someone else pushed")
                    } else if mode == CiAutofixMode::Suggest && !human_authorized {
                        Some("ready for a PR-agent handoff")
                    } else {
                        None
                    };
                    if let Some(expected_notice) = expected_notice {
                        let notified = handoff
                            .db
                            .get_unread_notifications()
                            .unwrap()
                            .iter()
                            .any(|notification| notification.message.contains(expected_notice));
                        if !notified {
                            failures.push(format!(
                                "{cell}, mode={mode:?}, human={human_authorized}: missing notification `{expected_notice}`"
                            ));
                        }
                    }
                }
            }
        }

        assert!(
            failures.is_empty(),
            "missing-history policy cells failed:\n{}",
            failures.join("\n")
        );
    }

    fn enqueue_pr(db: &thegn_core::db::Db, path: &str, number: u64, url: &str) {
        db.enqueue_bound_pr(path, number, Some(path), "fixture", "main", "github", url)
            .unwrap();
    }

    #[test]
    fn queue_row_pick_prefers_the_live_row_in_either_order() {
        let fixture = Fixture::new();
        let path = fixture.dir.path().to_str().unwrap();
        let db = thegn_core::db::Db::open_at(&fixture.dir.path().join("picker.db")).unwrap();
        enqueue_pr(&db, path, 7, &fixture.pr.url);
        enqueue_pr(&db, path, 9, &fixture.pr.url.replace("/pull/7", "/pull/9"));
        let rows = db.list_pr_queue().unwrap();
        let merged = rows.iter().find(|row| row.number == 9).unwrap();
        db.update_pr_status(&merged.key, "merged", None, None, None)
            .unwrap();
        let rows = db.list_pr_queue().unwrap();
        let mut reversed = rows.clone();
        reversed.reverse();

        for rows in [rows, reversed] {
            match queue_row_for_worktree(rows, path) {
                QueueRowPick::One(row) => assert_eq!(row.number, 7),
                pick => panic!("expected the live PR #7, got {pick:?}"),
            }
        }
    }

    #[test]
    fn queue_row_pick_holds_two_live_rows() {
        let fixture = Fixture::new();
        let path = fixture.dir.path().to_str().unwrap();
        let db = thegn_core::db::Db::open_at(&fixture.dir.path().join("picker.db")).unwrap();
        enqueue_pr(&db, path, 7, &fixture.pr.url);
        enqueue_pr(&db, path, 9, &fixture.pr.url.replace("/pull/7", "/pull/9"));

        assert_eq!(
            queue_row_for_worktree(db.list_pr_queue().unwrap(), path),
            QueueRowPick::Ambiguous(vec![7, 9])
        );
    }

    #[test]
    fn queue_row_pick_returns_a_lone_settled_row_and_ignores_other_worktrees() {
        let fixture = Fixture::new();
        let path = fixture.dir.path().to_str().unwrap();
        let db = thegn_core::db::Db::open_at(&fixture.dir.path().join("picker.db")).unwrap();
        enqueue_pr(&db, path, 9, &fixture.pr.url.replace("/pull/7", "/pull/9"));
        let row = db.list_pr_queue().unwrap().remove(0);
        db.update_pr_status(&row.key, "closed", None, None, None)
            .unwrap();
        let rows = db.list_pr_queue().unwrap();

        match queue_row_for_worktree(rows.clone(), path) {
            QueueRowPick::One(row) => assert_eq!(row.number, 9),
            pick => panic!("expected settled PR #9, got {pick:?}"),
        }
        assert_eq!(
            queue_row_for_worktree(rows, "/another/worktree"),
            QueueRowPick::None
        );
    }

    #[test]
    fn two_live_queue_rows_hold_ci_autofix_without_claiming_or_spending() {
        let fixture = Fixture::new();
        let handoff = handoff(&fixture, CiAutofixMode::Auto, &fixture.pr.head_ref_oid);
        let path = handoff.entry.worktree.as_str();
        enqueue_pr(
            &handoff.db,
            path,
            9,
            &fixture.pr.url.replace("/pull/7", "/pull/9"),
        );

        consider(&handoff, false);

        assert!(!handoff.sentinel.exists());
        assert!(
            handoff
                .db
                .list_pr_queue()
                .unwrap()
                .iter()
                .all(|row| row.agent_attempts == 0)
        );
        assert!(
            handoff
                .db
                .claim_ci_autofix(&handoff.entry.candidate())
                .unwrap()
        );
        assert!(
            handoff
                .db
                .get_unread_notifications()
                .unwrap()
                .iter()
                .any(|notification| notification.message.contains("share this worktree"))
        );
    }

    #[test]
    fn unsupported_platform_holds_automatic_and_human_ci_handoffs_without_claiming() {
        for human_authorized in [false, true] {
            let fixture = Fixture::new();
            let path = fixture.dir.path().to_str().unwrap();
            let db =
                thegn_core::db::Db::open_at(&fixture.dir.path().join("unsupported.db")).unwrap();
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
            let sentinel = fixture.dir.path().join("agent-must-not-run");
            let mut config = Config::default();
            config.ci.autofix.mode = CiAutofixMode::Auto;
            config.pr_queue.enabled = true;
            config.pr_queue.own_prs_only = false;
            config.pr_queue.agent_command = format!("touch {}", sentinel.display());
            let mut forge = fixture.forge(vec![fixture.proof.clone()]);
            forge.pr.status_check_rollup = vec![
                serde_json::from_value(serde_json::json!({"name":"test", "conclusion":"FAILURE"}))
                    .unwrap(),
            ];
            let entry = CiLogEntry {
                worktree: path.into(),
                head_sha: fixture.pr.head_ref_oid.clone(),
                text: "fixture failure".into(),
                run_id: "run".into(),
                job_id: "job".into(),
                ..Default::default()
            };
            let unsupported = crate::agent_run::force_headless_unsupported();
            consider_candidate_with_forge(&config, &db, &entry, human_authorized, Some(&forge));
            drop(unsupported);

            assert!(db.claim_ci_autofix(&entry.candidate()).unwrap());
            assert_eq!(db.list_pr_queue().unwrap()[0].agent_attempts, 0);
            assert!(!sentinel.exists());
            assert!(
                db.get_unread_notifications()
                    .unwrap()
                    .iter()
                    .any(|notification| notification.message.contains("THE-235"))
            );
        }
    }

    fn handoff_with_prompt_capture(
        fixture: &Fixture,
        head_sha: &str,
    ) -> (Handoff, std::path::PathBuf) {
        let mut handoff = handoff(fixture, CiAutofixMode::Auto, head_sha);
        let prompt_path = fixture.dir.path().join("prompt");
        handoff.config.pr_queue.agent_command = format!(
            "printf '%s' \"$THEGN_TASK_PROMPT\" > {}",
            prompt_path.display()
        );
        let begin = format!("-----{} PRIVATE KEY-----", "BEGIN");
        let end = format!("-----{} PRIVATE KEY-----", "END");
        handoff.entry.text = format!("{begin}\nSYNTHETIC_PEM_BODY\n{end}\ntoken=SYNTHETIC_TOKEN\n");
        (handoff, prompt_path)
    }

    #[test]
    fn ci_prompt_carries_only_redacted_log_text() {
        let fixture = Fixture::new();
        let (handoff, prompt_path) =
            handoff_with_prompt_capture(&fixture, &fixture.pr.head_ref_oid);

        consider(&handoff, false);

        let prompt = std::fs::read_to_string(prompt_path).unwrap();
        assert!(prompt.contains(thegn_core::ci_log::REDACTION_MARKER));
        assert!(!prompt.contains("SYNTHETIC_PEM_BODY"));
        assert!(!prompt.contains("SYNTHETIC_TOKEN"));
    }

    #[test]
    fn ci_prompt_log_evidence_is_a_window_around_the_first_failure() {
        let fixture = Fixture::new();
        let (mut handoff, prompt_path) =
            handoff_with_prompt_capture(&fixture, &fixture.pr.head_ref_oid);
        let mut log = (0..3_000)
            .map(|line| format!("ok step {line}\n"))
            .collect::<String>();
        log.push_str("error: SYNTHETIC_MID failure\n");
        log.push_str(
            &(3_000..6_000)
                .map(|line| format!("ok step {line}\n"))
                .collect::<String>(),
        );
        handoff.entry.text = log;

        consider(&handoff, false);

        let prompt = std::fs::read_to_string(prompt_path).unwrap();
        assert!(prompt.contains("SYNTHETIC_MID"));
        assert!(prompt.contains("ok step 2980\n"));
    }

    #[test]
    fn invalid_prompt_holds_without_claim_spawn_or_spend() {
        let fixture = Fixture::new();
        let (mut handoff, prompt_path) =
            handoff_with_prompt_capture(&fixture, &fixture.pr.head_ref_oid);
        handoff.config.pr_queue.prompts.ci_failure = "{nope}".into();

        consider(&handoff, false);

        assert!(!prompt_path.exists());
        assert!(!handoff.sentinel.exists());
        assert_eq!(handoff.db.list_pr_queue().unwrap()[0].agent_attempts, 0);
        assert!(
            handoff
                .db
                .claim_ci_autofix(&handoff.entry.candidate())
                .unwrap()
        );
        assert!(
            handoff
                .db
                .get_unread_notifications()
                .unwrap()
                .iter()
                .any(|notification| {
                    notification
                        .message
                        .contains("was not dispatched: prompt is invalid")
                })
        );
    }

    #[test]
    fn ci_autofix_is_refused_while_another_pr_agent_lease_is_held() {
        let fixture = Fixture::new();
        let handoff = handoff(&fixture, CiAutofixMode::Auto, &fixture.pr.head_ref_oid);
        let row = handoff.db.list_pr_queue().unwrap().remove(0);
        let request = thegn_core::pr_queue::PrAgentAdmit {
            key: &row.key,
            expected_attempts: 0,
            expected_checkout_gen: row.checkout_gen,
            max_attempts: 3,
            head: &handoff.entry.head_sha,
            blocker: "ci",
            detail: "PR driver",
            lease_until_ms: crate::pr_agent_lease::lease_until_ms(),
            owner_hint: Some("PR driver"),
            ci_candidate: None,
        };
        assert!(matches!(
            handoff.db.admit_pr_agent(&request).unwrap(),
            thegn_core::pr_queue::PrAgentAdmission::Admitted(_)
        ));
        handoff
            .db
            .update_pr_status(&row.key, PrqStatus::BlockedCi.as_str(), None, None, None)
            .unwrap();
        // The live-lease fence blocks this refresh write.
        assert_eq!(
            handoff.db.list_pr_queue().unwrap()[0].status,
            PrqStatus::AgentRunning.as_str()
        );

        consider(&handoff, false);

        let current = handoff.db.list_pr_queue().unwrap().remove(0);
        assert!(!handoff.sentinel.exists());
        assert_eq!(current.agent_attempts, 1);
        assert!(
            handoff
                .db
                .claim_ci_autofix(&handoff.entry.candidate())
                .unwrap()
        );
        assert!(
            handoff
                .db
                .get_unread_notifications()
                .unwrap()
                .iter()
                .any(|notification| {
                    notification.message.contains("held:")
                        && notification.message.contains("agent_running")
                })
        );
    }

    #[test]
    fn isolation_refusal_holds_without_claim_spawn_or_spend() {
        let fixture = Fixture::new();
        let (mut handoff, prompt_path) =
            handoff_with_prompt_capture(&fixture, &fixture.pr.head_ref_oid);
        handoff.config.pr_queue.agent_sandbox = true;
        std::fs::write(
            fixture.dir.path().join(".thegn.toml"),
            "[sandbox\nnetwork = \"none\"\n",
        )
        .unwrap();

        consider(&handoff, false);

        assert!(!prompt_path.exists());
        assert!(!handoff.sentinel.exists());
        assert_eq!(handoff.db.list_pr_queue().unwrap()[0].agent_attempts, 0);
        assert!(
            handoff
                .db
                .claim_ci_autofix(&handoff.entry.candidate())
                .unwrap()
        );
        assert!(
            handoff
                .db
                .get_unread_notifications()
                .unwrap()
                .iter()
                .any(|notification| {
                    notification.message.contains("held:")
                        && notification.message.contains(".thegn.toml")
                })
        );
    }

    #[test]
    fn expired_agent_running_row_is_reconciled_before_autofix() {
        let fixture = Fixture::new();
        let handoff = handoff(&fixture, CiAutofixMode::Auto, &fixture.pr.head_ref_oid);
        let row = handoff.db.list_pr_queue().unwrap().remove(0);
        let request = thegn_core::pr_queue::PrAgentAdmit {
            key: &row.key,
            expected_attempts: row.agent_attempts,
            expected_checkout_gen: row.checkout_gen,
            max_attempts: 3,
            head: &handoff.entry.head_sha,
            blocker: "ci",
            detail: "previous CI dispatch",
            lease_until_ms: crate::pr_agent_lease::lease_until_ms(),
            owner_hint: Some("dead worker"),
            ci_candidate: None,
        };
        assert!(matches!(
            handoff.db.admit_pr_agent(&request).unwrap(),
            thegn_core::pr_queue::PrAgentAdmission::Admitted(_)
        ));
        rusqlite::Connection::open(&handoff.db_path)
            .unwrap()
            .execute(
                "UPDATE pr_queue SET agent_claim_lease_until_ms=0 WHERE key=?1",
                [&row.key],
            )
            .unwrap();

        consider(&handoff, false);

        let notifications = handoff.db.get_unread_notifications().unwrap();
        let row_after = handoff
            .db
            .list_pr_queue()
            .unwrap()
            .into_iter()
            .find(|current| current.key == row.key)
            .unwrap();
        let new_lease = handoff.db.pr_agent_lease_view(&row.key).unwrap();
        assert!(
            new_lease.is_none_or(|view| view.generation > 1)
                || row_after.status != PrqStatus::AgentRunning.as_str(),
            "row={row_after:?}; notifications={notifications:?}"
        );
        assert!(
            notifications.iter().all(|notification| {
                !notification
                    .message
                    .contains("PR queue row is `agent_running`")
            }),
            "notifications={notifications:?}"
        );
        if row_after.agent_attempts > 1 {
            assert_eq!(
                row_after.agent_attempts, 2,
                "row={row_after:?}; notifications={notifications:?}"
            );
        }
    }

    #[test]
    fn finish_detail_has_no_not_started_wording() {
        for verdict in [
            crate::agent_run::AgentRunVerdict::Exited(Some(1)),
            crate::agent_run::AgentRunVerdict::Exited(None),
            crate::agent_run::AgentRunVerdict::Interrupted("timed out".into()),
        ] {
            assert!(!autofix_finish_detail(&verdict, None).contains("did not start"));
        }
    }

    #[test]
    fn finish_detail_keeps_exit_wording_for_runs_that_started() {
        assert_eq!(
            autofix_finish_detail(&crate::agent_run::AgentRunVerdict::Exited(Some(0)), None),
            "CI autofix finished (exit ok); awaiting the next PR refresh"
        );
        for verdict in [
            crate::agent_run::AgentRunVerdict::Exited(Some(1)),
            crate::agent_run::AgentRunVerdict::Exited(None),
            crate::agent_run::AgentRunVerdict::Interrupted("timed out".into()),
        ] {
            assert_eq!(
                autofix_finish_detail(&verdict, None),
                "CI autofix finished (exit failed); awaiting the next PR refresh"
            );
        }
    }

    #[test]
    fn finish_detail_carries_the_head_note() {
        let note = "PR #7: the agent finished without changing the PR head; the next refresh re-evaluates the row";
        assert_eq!(
            autofix_finish_detail(
                &crate::agent_run::AgentRunVerdict::Exited(Some(0)),
                Some(note)
            ),
            "CI autofix finished (exit ok); PR #7: the agent finished without changing the PR head; the next refresh re-evaluates the row; awaiting the next PR refresh"
        );
        assert_eq!(
            autofix_finish_detail(&crate::agent_run::AgentRunVerdict::Exited(Some(0)), None),
            "CI autofix finished (exit ok); awaiting the next PR refresh"
        );
    }

    #[test]
    fn not_started_agent_is_reported_instead_of_a_failed_exit() {
        let fixture = Fixture::new();
        let mut handoff = handoff(&fixture, CiAutofixMode::Auto, &fixture.pr.head_ref_oid);
        handoff.config.pr_queue.agent_command = format!(
            "touch {} {{prompt}} {}",
            handoff.sentinel.display(),
            "x".repeat(101 * 1024)
        );

        consider(&handoff, false);

        assert!(!handoff.sentinel.exists());
        let row = handoff.db.list_pr_queue().unwrap().remove(0);
        assert_eq!(row.agent_attempts, 0);
        assert_eq!(row.status, PrqStatus::NeedsHuman.as_str());
        assert!(row.detail.as_deref().is_some_and(|detail| {
            detail.contains("agent did not start:") && detail.contains("no attempt spent")
        }));
        assert!(
            handoff
                .db
                .get_unread_notifications()
                .unwrap()
                .iter()
                .any(|notification| notification.message.contains("did not start:"))
        );
        assert!(
            !handoff
                .db
                .claim_ci_autofix(&handoff.entry.candidate())
                .unwrap()
        );
    }
}
