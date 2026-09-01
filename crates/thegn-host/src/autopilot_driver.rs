//! Off-loop issue-autopilot supervisor.
//!
//! The driver runs on bounded blocking workers. Tracker refresh only claims
//! matching issues and schedules the actual work, so one slow agent cannot
//! hold the refresh worker hostage.

use std::path::Path;

use thegn_core::agent_task::{TaskKind, TaskVars, default_prompt, render_prompt};
use thegn_core::autopilot::{AutopilotIssueKey, AutopilotState, bounded_reason, matches_issue};
use thegn_core::config::{AutopilotConfig, Config};
use thegn_core::db::Db;
use thegn_core::forge::PrRef;
use thegn_core::forge::model::CreateOpts;
use thegn_core::issue::{AgentDispatchStatus, Issue, IssuePatch, NewDispatch};
use thegn_core::remote::GitLoc;
use thegn_core::store::{AutopilotStore, NotificationStore, WorkspaceStore, WorktreeAuxStore};

/// Run the pickup/implement/PR path for one successful provider refresh.
/// `from_assignee_me` is provenance from the authenticated provider filter;
/// it is deliberately not inferred from display names in an issue payload.
pub(crate) fn pickup(
    cfg: &Config,
    repo_root: &Path,
    cwd: &Path,
    account: &str,
    provider: &str,
    issues: &[Issue],
    from_assignee_me: bool,
) {
    let policy = cfg.repo_autopilot(repo_root);
    if !policy.enabled {
        return;
    }
    let Ok(db) = Db::open() else { return };
    for issue in issues {
        if !matches_issue(
            issue,
            &policy.trigger_label,
            policy.pickup_status,
            from_assignee_me,
        ) {
            continue;
        }
        let key = AutopilotIssueKey::new(provider, account, issue.id.clone());
        let now = thegn_core::util::now_ms();
        let claim = match db.claim_autopilot(
            &key,
            &repo_root.to_string_lossy(),
            policy.max_concurrent,
            policy.max_attempts,
            now,
        ) {
            Ok(thegn_core::store::ClaimOutcome::Claimed(run)) => run,
            Ok(_) => continue,
            Err(e) => {
                tracing::warn!(target: "thegn::autopilot", error = %e, "autopilot claim failed");
                continue;
            }
        };
        let cfg = cfg.clone();
        let policy = policy.clone();
        let repo_root = repo_root.to_path_buf();
        let cwd = cwd.to_path_buf();
        let account = account.to_owned();
        let issue = issue.clone();
        let run_id = claim.id;
        crate::sched::spawn_bg(move || {
            let Ok(db) = Db::open() else {
                tracing::warn!(
                    target: "thegn::autopilot",
                    run_id,
                    "autopilot worker could not open the durable database"
                );
                return;
            };
            drive_claim(
                &db, &cfg, &policy, &repo_root, &cwd, &account, &issue, run_id,
            );
        });
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "the worker receives the complete claimed issue context"
)]
fn drive_claim(
    db: &Db,
    cfg: &Config,
    policy: &AutopilotConfig,
    repo_root: &Path,
    cwd: &Path,
    account: &str,
    issue: &Issue,
    run_id: i64,
) {
    let fail = |reason: &str| mark_needs_human(db, run_id, AutopilotState::Claimed, None, reason);

    let branch_seed =
        thegn_core::issue::issue_branch_seed(issue.branch_hint.as_deref(), &issue.number);
    let branch = thegn_core::worktree::dedupe(
        &branch_seed,
        &thegn_core::worktree::BranchSet::load(repo_root),
    );
    let base = thegn_core::worktree::resolve_base(repo_root, cfg);
    if thegn_core::util::git_out(repo_root, &["rev-parse", "--verify", "--quiet", &base]).is_none()
    {
        fail("configured base branch has no commit");
        return;
    }
    let worktree = thegn_core::worktree::worktree_path(repo_root, &branch, cfg);
    if let Err(e) = thegn_core::worktree::add_checked(repo_root, &branch, &base, &worktree, cfg) {
        fail(&format!("worktree creation failed: {e}"));
        return;
    }
    let wt = worktree.to_string_lossy().into_owned();
    let root_s = repo_root.to_string_lossy().into_owned();
    let tab =
        thegn_core::repo::branch_tab(&thegn_core::repo::repo_slug_with(db, repo_root), &branch);
    if let Err(e) = db.put_worktree(&tab, &root_s, &wt, &branch, None, None) {
        fail(&format!("worktree registration failed: {e}"));
        return;
    }
    if let Err(e) = db.link_issue(&wt, &issue.id) {
        fail(&format!("issue linkage failed: {e}"));
        return;
    }
    match db.set_autopilot_worktree(run_id, &wt, &branch, &base, thegn_core::util::now_ms()) {
        Ok(true) => {}
        Ok(false) => {
            fail("autopilot worktree journal row disappeared");
            return;
        }
        Err(e) => {
            fail(&format!("autopilot worktree journal update failed: {e}"));
            return;
        }
    }

    let agent = policy.agent.trim();
    let role = if agent.is_empty() {
        if policy.agent_command.trim().is_empty() {
            fail("no autopilot agent or agent_command configured");
            return;
        }
        "autopilot-command"
    } else {
        agent
    };
    let dispatch_id = match db.put_agent_dispatch(NewDispatch {
        issue_id: &issue.id,
        worktree_path: &wt,
        agent_name: role,
        stage: Some("autopilot"),
        parent_id: None,
        session_id: None,
        artifact_path: None,
        chunk_path: None,
    }) {
        Ok(id) => id,
        Err(e) => {
            fail(&format!("dispatch record failed: {e}"));
            return;
        }
    };
    match db.attach_autopilot_dispatch(run_id, dispatch_id, thegn_core::util::now_ms()) {
        Ok(true) => {}
        Ok(false) => {
            mark_needs_human(
                db,
                run_id,
                AutopilotState::Claimed,
                Some(dispatch_id),
                "autopilot dispatch journal row disappeared",
            );
            return;
        }
        Err(e) => {
            mark_needs_human(
                db,
                run_id,
                AutopilotState::Claimed,
                Some(dispatch_id),
                &format!("autopilot dispatch journal update failed: {e}"),
            );
            return;
        }
    }

    // This edge is best-effort and never releases the durable claim.  A
    // provider outage must leave the run visible for a human to inspect.
    let tracker_cfg = tracker_config_for_account(cfg, repo_root, account);
    let router = thegn_svc::issue::IssueRouter::from_config_at(&tracker_cfg, Some(cwd));
    if router.is_configured()
        && let Ok(rt) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
    {
        let patch = IssuePatch {
            status: Some(thegn_core::issue::IssueStatus::InProgress),
            ..Default::default()
        };
        if let Err(e) = rt.block_on(router.update_issue(&issue.id, &patch)) {
            mark_needs_human(
                db,
                run_id,
                AutopilotState::Claimed,
                Some(dispatch_id),
                &format!("issue status update failed: {e}"),
            );
            return;
        }
    }

    match db.transition_autopilot(
        run_id,
        AutopilotState::Claimed,
        AutopilotState::Working,
        Some("worker started"),
        None,
        thegn_core::util::now_ms(),
    ) {
        Ok(true) => {}
        Ok(false) => {
            mark_needs_human(
                db,
                run_id,
                AutopilotState::Claimed,
                Some(dispatch_id),
                "claim changed before worker started",
            );
            return;
        }
        Err(e) => {
            mark_needs_human(
                db,
                run_id,
                AutopilotState::Claimed,
                Some(dispatch_id),
                &format!("worker start journal update failed: {e}"),
            );
            return;
        }
    }
    let vars = TaskVars::new()
        .set("issue_number", &issue.number)
        .set("issue_title", &issue.title)
        .set("issue_body", issue.body.as_deref().unwrap_or_default())
        .set("issue_url", &issue.url)
        .set("branch", &branch)
        .set("worktree", &wt);
    let prompt = match render_prompt(default_prompt(TaskKind::Issue), &vars) {
        Ok(p) => p,
        Err(e) => {
            mark_worker_failed(
                db,
                run_id,
                dispatch_id,
                &format!("issue prompt failed: {e}"),
            );
            return;
        }
    };
    let template = if !policy.agent_command.trim().is_empty() {
        policy.agent_command.clone()
    } else {
        match thegn_core::agent_task::effective_agent(cfg, agent, None)
            .and_then(|e| e.headless_template())
        {
            Ok(t) => t,
            Err(e) => {
                mark_worker_failed(
                    db,
                    run_id,
                    dispatch_id,
                    &format!("agent resolution failed: {e}"),
                );
                return;
            }
        }
    };
    let sandbox = match sealed_autopilot_sandbox(cfg, repo_root, &worktree) {
        Ok(spec) => spec,
        Err(reason) => {
            mark_worker_failed(db, run_id, dispatch_id, &reason);
            return;
        }
    };
    let ran = crate::agent_run::run(&crate::agent_run::AgentTaskRun {
        kind: TaskKind::Issue,
        worktree: &wt,
        prompt: &prompt,
        command_template: &template,
        vars: &vars,
        timeout_secs: policy.agent_timeout_secs,
        sandbox: Some(sandbox),
        credential_free: true,
    });
    if let Err(e) = db.update_dispatch_status(
        dispatch_id,
        if ran {
            AgentDispatchStatus::Done
        } else {
            AgentDispatchStatus::Failed
        },
    ) {
        mark_worker_failed(
            db,
            run_id,
            dispatch_id,
            &format!("worker result journal update failed: {e}"),
        );
        return;
    }

    let loc = GitLoc::for_worktree(&worktree);
    let current = crate::git_handle::get()
        .current_branch(&loc)
        .unwrap_or_default();
    let clean = crate::git_handle::get()
        .status(&loc)
        .map(|s| s.is_empty())
        .unwrap_or(false);
    let ahead = thegn_core::util::git_out(
        &worktree,
        &["rev-list", "--count", &format!("{base}..HEAD")],
    )
    .and_then(|n| n.parse::<u64>().ok())
    .unwrap_or(0);
    if current != branch || !clean || ahead == 0 {
        mark_worker_failed(
            db,
            run_id,
            dispatch_id,
            &worker_failure(&current, &branch, clean, ahead),
        );
        return;
    }

    let git = thegn_svc::git::CliGit;
    if let Err(e) = thegn_svc::git::BranchOps::push_set_upstream(&git, &loc, "origin", &branch) {
        mark_worker_failed(db, run_id, dispatch_id, &format!("push failed: {e}"));
        return;
    }
    let forges = crate::forge_handle::get();
    let forge = forges.for_loc(&loc);
    let body = format!(
        "Implemented tracker issue `{}` ({})\n\n{}",
        issue.number, issue.url, issue.title
    );
    let opts = CreateOpts {
        title: Some(format!("{}: {}", issue.number, issue.title)),
        body: Some(body),
        base: Some(base.clone()),
        draft: matches!(policy.open_as, thegn_core::config::AutopilotOpenAs::Draft),
        web: false,
        fill: false,
    };
    let url = match forge.create_pr(&loc, &opts) {
        Ok(url) => url,
        Err(e) => {
            mark_worker_failed(
                db,
                run_id,
                dispatch_id,
                &format!("pull request creation failed: {}", e.describe()),
            );
            return;
        }
    };
    let pr = match forge.pr_status(&loc, PrRef::Current) {
        Ok(pr) if pr.number > 0 => pr,
        Ok(_) => {
            mark_worker_failed(
                db,
                run_id,
                dispatch_id,
                "created pull request has no number",
            );
            return;
        }
        Err(e) => {
            mark_worker_failed(
                db,
                run_id,
                dispatch_id,
                &format!("created pull request readback failed: {}", e.describe()),
            );
            return;
        }
    };
    match db.set_autopilot_pr(
        run_id,
        pr.number,
        &pr.head_ref_name,
        &url,
        thegn_core::util::now_ms(),
    ) {
        Ok(true) => {}
        Ok(false) => {
            mark_worker_failed(
                db,
                run_id,
                dispatch_id,
                "autopilot PR journal row disappeared",
            );
            return;
        }
        Err(e) => {
            mark_worker_failed(
                db,
                run_id,
                dispatch_id,
                &format!("autopilot PR journal update failed: {e}"),
            );
            return;
        }
    }
    match db.transition_autopilot(
        run_id,
        AutopilotState::Working,
        AutopilotState::PrOpened,
        Some("pull request opened"),
        Some(pr.number),
        thegn_core::util::now_ms(),
    ) {
        Ok(true) => {}
        Ok(false) => {
            mark_worker_failed(
                db,
                run_id,
                dispatch_id,
                "worker result changed before PR transition",
            );
            return;
        }
        Err(e) => {
            mark_worker_failed(
                db,
                run_id,
                dispatch_id,
                &format!("PR transition journal update failed: {e}"),
            );
            return;
        }
    }
    if let Err(e) = db.update_dispatch_status(dispatch_id, AgentDispatchStatus::PrOpen) {
        mark_needs_human(
            db,
            run_id,
            AutopilotState::PrOpened,
            Some(dispatch_id),
            &format!("PR dispatch status update failed: {e}"),
        );
        return;
    }
    if cfg.repo_pr_queue(repo_root).enabled {
        if let Err(e) = db.enqueue_pr(
            &root_s,
            pr.number,
            Some(&wt),
            &pr.head_ref_name,
            &pr.base_ref_name,
            forge.id(),
        ) {
            mark_needs_human(
                db,
                run_id,
                AutopilotState::PrOpened,
                Some(dispatch_id),
                &format!("PR queue insert failed: {e}"),
            );
            return;
        }
        match db.transition_autopilot(
            run_id,
            AutopilotState::PrOpened,
            AutopilotState::Shepherding,
            Some("queued for PR shepherding"),
            Some(pr.number),
            thegn_core::util::now_ms(),
        ) {
            Ok(true) => {}
            Ok(false) => mark_needs_human(
                db,
                run_id,
                AutopilotState::PrOpened,
                Some(dispatch_id),
                "autopilot shepherding journal row disappeared",
            ),
            Err(e) => mark_needs_human(
                db,
                run_id,
                AutopilotState::PrOpened,
                Some(dispatch_id),
                &format!("autopilot shepherding journal update failed: {e}"),
            ),
        }
    }
}

/// Keep writes routed to the same named tracker account that supplied the
/// issue. `IssueRouter::update_issue` selects the first backend for a provider
/// when several accounts share it, so an unfiltered router could update a
/// different account's issue with the same provider/key.
fn tracker_config_for_account(
    cfg: &Config,
    repo_root: &Path,
    account: &str,
) -> thegn_core::config::IssuesConfig {
    let mut tracker_cfg = cfg.repo_issues(Some(repo_root));
    if !tracker_cfg.issue_accounts.is_empty() {
        tracker_cfg
            .issue_accounts
            .retain(|entry| entry.name == account);
    }
    tracker_cfg
}

/// Resolve the issue worker's isolation boundary independently of the normal
/// interactive sandbox posture. Autopilot receives untrusted tracker text, so
/// it must fail closed when no real sandbox backend is available and must not
/// inherit configured credentials, home mounts, caches, or network access.
fn sealed_autopilot_sandbox(
    cfg: &Config,
    repo_root: &Path,
    worktree: &Path,
) -> Result<thegn_core::sandbox::SandboxSpec, String> {
    let mut sandbox = cfg.repo_sandbox(repo_root);
    sandbox.profile = thegn_core::config::SandboxProfile::Sealed;
    sandbox.file_access = thegn_core::config::FileAccess::Worktree;
    sandbox.mounts.clear();
    sandbox.volumes.clear();
    sandbox.env_passthrough.clear();
    sandbox.auto_caches = false;
    sandbox.inject_devshell = false;
    sandbox.devshell.clear();
    sandbox.nix_daemon = false;
    sandbox.devenv = false;
    sandbox.prepare.clear();
    sandbox.init_script.clear();
    sandbox.warm_direnv = thegn_core::config::WarmDirenv::Off;
    sandbox.ports.clear();
    sandbox.gpu = None;
    sandbox.compose = None;
    sandbox.network_allow.clear();
    sandbox.network_block.clear();
    sandbox.vpn = thegn_core::config::VpnConfig::default();

    let loc = GitLoc::for_worktree(worktree);
    let name = format!(
        "autopilot-{}",
        thegn_core::util::slugify(&worktree.to_string_lossy())
    );
    let mut spec = thegn_core::sandbox::resolve_scoped(
        &sandbox,
        &loc,
        &name,
        thegn_core::config::SandboxProfile::Sealed,
    )
    .ok_or_else(|| "no usable sandbox backend for autopilot worker".to_string())?;
    // The resolver normally forwards configured env values. Keep only the
    // worktree mounts it generated and explicitly suppress image-provided
    // credential-shaped names as well.
    spec.env.clear();
    spec.env_overrides.clear();
    spec.env_block.extend(
        [
            "GH_TOKEN",
            "GITHUB_TOKEN",
            "SSH_AUTH_SOCK",
            "AWS_ACCESS_KEY_ID",
            "AWS_SECRET_ACCESS_KEY",
            "ANTHROPIC_API_KEY",
            "OPENAI_API_KEY",
        ]
        .into_iter()
        .map(str::to_string),
    );
    spec.network = thegn_core::config::Network::None;
    Ok(spec)
}

fn mark_needs_human(
    db: &Db,
    run_id: i64,
    expected: AutopilotState,
    dispatch_id: Option<i64>,
    reason: &str,
) {
    if let Some(dispatch_id) = dispatch_id
        && let Err(e) = db.update_dispatch_status(dispatch_id, AgentDispatchStatus::WaitingHuman)
    {
        tracing::warn!(
            target: "thegn::autopilot",
            run_id,
            dispatch_id,
            error = %e,
            "failed to record autopilot dispatch needs-human status"
        );
    }
    let reason = bounded_reason(Some(reason)).unwrap_or_else(|| "autopilot failure".to_string());
    match db.transition_autopilot(
        run_id,
        expected,
        AutopilotState::NeedsHuman,
        Some(&reason),
        None,
        thegn_core::util::now_ms(),
    ) {
        Ok(true) => {}
        Ok(false) => tracing::warn!(
            target: "thegn::autopilot",
            run_id,
            ?expected,
            "autopilot failure could not claim the expected journal state"
        ),
        Err(e) => tracing::warn!(
            target: "thegn::autopilot",
            run_id,
            ?expected,
            error = %e,
            "failed to record autopilot needs-human state"
        ),
    }
}

fn worker_failure(current: &str, expected: &str, clean: bool, ahead: u64) -> String {
    format!(
        "worker result rejected: branch={current:?} expected={expected:?}, clean={clean}, commits_ahead={ahead}"
    )
}

fn mark_worker_failed(db: &Db, run_id: i64, dispatch_id: i64, reason: &str) {
    mark_needs_human(
        db,
        run_id,
        AutopilotState::Working,
        Some(dispatch_id),
        reason,
    );
}

/// Close an autopilot run only after the PR queue has observed a real `merged`
/// row transition.  The caller is already on a blocking worker.
pub(crate) fn on_pr_merged(cfg: &Config, repo_root: &Path, number: u64) {
    let Ok(db) = Db::open() else { return };
    let Ok(Some(run)) = db.find_autopilot_by_pr(&repo_root.to_string_lossy(), number) else {
        return;
    };
    let policy = cfg.repo_autopilot(repo_root);
    if policy.done_on_merge {
        let tracker_cfg = tracker_config_for_account(cfg, repo_root, &run.key.account);
        let router = thegn_svc::issue::IssueRouter::from_config_at(&tracker_cfg, Some(repo_root));
        if router.is_configured()
            && let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
        {
            let patch = IssuePatch {
                status: Some(thegn_core::issue::IssueStatus::Done),
                ..Default::default()
            };
            if let Err(e) = rt.block_on(router.update_issue(&run.key.issue_id, &patch)) {
                mark_needs_human(
                    &db,
                    run.id,
                    run.state,
                    run.dispatch_id,
                    &format!("done sync failed: {e}"),
                );
                return;
            }
        }
    }
    let next = match run.state {
        AutopilotState::PrOpened | AutopilotState::Shepherding => AutopilotState::Done,
        _ => return,
    };
    match db.transition_autopilot(
        run.id,
        run.state,
        next,
        Some("PR queue observed merged"),
        Some(number),
        thegn_core::util::now_ms(),
    ) {
        Ok(true) => {}
        Ok(false) => tracing::warn!(
            target: "thegn::autopilot",
            run_id = run.id,
            number,
            "autopilot merge completion found no expected journal state"
        ),
        Err(e) => tracing::warn!(
            target: "thegn::autopilot",
            run_id = run.id,
            number,
            error = %e,
            "failed to record autopilot merge completion"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::worker_failure;

    #[test]
    fn worker_validation_reason_is_bounded_and_actionable() {
        let text = worker_failure("HEAD", "feat", false, 0);
        assert!(text.contains("expected"));
        assert!(text.contains("commits_ahead=0"));
    }
}
