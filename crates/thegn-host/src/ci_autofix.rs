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
    let ref_id = format!(
        "ci-autofix:{}:{}:{}:{}",
        candidate.worktree, candidate.run_id, candidate.job_id, candidate.head_sha
    );
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
                    let reread = db.list_pr_queue().and_then(|rows| {
                        rows.into_iter()
                            .find(|row| row.key == item.key)
                            .ok_or_else(|| anyhow::anyhow!("PR queue row disappeared"))
                    });
                    match reread {
                        Ok(row) => row,
                        Err(_) => {
                            notify(format!(
                                "CI autofix for PR #{} held: could not re-read the PR queue row after reconciling an expired agent lease",
                                item.number
                            ));
                            return;
                        }
                    }
                }
                Ok(None) => item,
                Err(error) => {
                    notify(format!(
                        "CI autofix for PR #{} held: could not reconcile the expired PR-agent lease ({error}); agent dispatch refused",
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
    let log_text = thegn_core::ci_log::redact(&entry.text);
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
    let exited_zero = crate::agent_run::run(&crate::agent_run::AgentTaskRun {
        kind: thegn_core::agent_task::TaskKind::PrCiFailure,
        worktree,
        prompt: &prompt,
        command_template: template,
        vars: &vars,
        timeout_secs: pq.agent_timeout_secs,
        sandbox,
        credential_free: true,
    });
    let own = crate::pr_agent_lease::observe_own_head(&loc, &entry.head_sha, || {
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
    });
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
    let outcome = if exited_zero {
        "exit ok"
    } else {
        "exit failed"
    };
    let finished = format!("CI autofix finished ({outcome}); awaiting the next PR refresh");
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pr_authorship::tests::Fixture;
    use std::sync::atomic::Ordering;

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
            ("agent_running", false, false, "agent_running"),
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
}
