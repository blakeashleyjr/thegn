//! Off-loop issue-autopilot supervisor.
//!
//! This module is intentionally a synchronous driver.  The tracker refresh and
//! PR queue already run on background/blocking workers; the driver is called
//! from those workers and therefore may use the existing blocking tracker,
//! git, forge, and process seams without ever touching the compositor loop.

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
        drive_claim(&db, cfg, &policy, repo_root, cwd, issue, claim.id);
    }
}

fn drive_claim(
    db: &Db,
    cfg: &Config,
    policy: &AutopilotConfig,
    repo_root: &Path,
    cwd: &Path,
    issue: &Issue,
    run_id: i64,
) {
    let fail = |reason: &str| {
        let reason = bounded_reason(Some(reason)).unwrap_or_default();
        let _ = db.transition_autopilot(
            run_id,
            AutopilotState::Claimed,
            AutopilotState::NeedsHuman,
            Some(&reason),
            None,
            thegn_core::util::now_ms(),
        );
    };

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
    let _ = db.link_issue(&wt, &issue.id);
    let _ = db.set_autopilot_worktree(run_id, &wt, &branch, &base, thegn_core::util::now_ms());

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
    let _ = db.attach_autopilot_dispatch(run_id, dispatch_id, thegn_core::util::now_ms());

    // This edge is best-effort and never releases the durable claim.  A
    // provider outage must leave the run visible for a human to inspect.
    let tracker_cfg = cfg.repo_issues(Some(repo_root));
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
            let _ = db.transition_autopilot(
                run_id,
                AutopilotState::Claimed,
                AutopilotState::NeedsHuman,
                Some(&format!("issue status update failed: {e}")),
                None,
                thegn_core::util::now_ms(),
            );
            let _ = db.update_dispatch_status(dispatch_id, AgentDispatchStatus::WaitingHuman);
            return;
        }
    }

    if !db
        .transition_autopilot(
            run_id,
            AutopilotState::Claimed,
            AutopilotState::Working,
            Some("worker started"),
            None,
            thegn_core::util::now_ms(),
        )
        .unwrap_or(false)
    {
        fail("claim changed before worker started");
        return;
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
    let ran = crate::agent_run::run(&crate::agent_run::AgentTaskRun {
        kind: TaskKind::Issue,
        worktree: &wt,
        prompt: &prompt,
        command_template: &template,
        vars: &vars,
        timeout_secs: policy.agent_timeout_secs,
        sandbox: None,
    });
    let _ = db.update_dispatch_status(
        dispatch_id,
        if ran {
            AgentDispatchStatus::Done
        } else {
            AgentDispatchStatus::Failed
        },
    );

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
    let _ = db.set_autopilot_pr(
        run_id,
        pr.number,
        &pr.head_ref_name,
        &url,
        thegn_core::util::now_ms(),
    );
    if !db
        .transition_autopilot(
            run_id,
            AutopilotState::Working,
            AutopilotState::PrOpened,
            Some("pull request opened"),
            Some(pr.number),
            thegn_core::util::now_ms(),
        )
        .unwrap_or(false)
    {
        mark_worker_failed(
            db,
            run_id,
            dispatch_id,
            "worker result changed before PR transition",
        );
        return;
    }
    let _ = db.update_dispatch_status(dispatch_id, AgentDispatchStatus::PrOpen);
    if cfg.repo_pr_queue(repo_root).enabled {
        if let Err(e) = db.enqueue_pr(
            &root_s,
            pr.number,
            Some(&wt),
            &pr.head_ref_name,
            &pr.base_ref_name,
            forge.id(),
        ) {
            let _ = db.transition_autopilot(
                run_id,
                AutopilotState::PrOpened,
                AutopilotState::NeedsHuman,
                Some(&format!("PR queue insert failed: {e}")),
                Some(pr.number),
                thegn_core::util::now_ms(),
            );
            return;
        }
        let _ = db.transition_autopilot(
            run_id,
            AutopilotState::PrOpened,
            AutopilotState::Shepherding,
            Some("queued for PR shepherding"),
            Some(pr.number),
            thegn_core::util::now_ms(),
        );
    }
}

fn worker_failure(current: &str, expected: &str, clean: bool, ahead: u64) -> String {
    format!(
        "worker result rejected: branch={current:?} expected={expected:?}, clean={clean}, commits_ahead={ahead}"
    )
}

fn mark_worker_failed(db: &Db, run_id: i64, dispatch_id: i64, reason: &str) {
    let _ = db.update_dispatch_status(dispatch_id, AgentDispatchStatus::WaitingHuman);
    let _ = db.transition_autopilot(
        run_id,
        AutopilotState::Working,
        AutopilotState::NeedsHuman,
        Some(reason),
        None,
        thegn_core::util::now_ms(),
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
        let router = thegn_svc::issue::IssueRouter::from_config_at(&cfg.issues, Some(repo_root));
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
                let _ = db.transition_autopilot(
                    run.id,
                    run.state,
                    AutopilotState::NeedsHuman,
                    Some(&format!("done sync failed: {e}")),
                    Some(number),
                    thegn_core::util::now_ms(),
                );
                return;
            }
        }
    }
    let next = match run.state {
        AutopilotState::PrOpened | AutopilotState::Shepherding => AutopilotState::Done,
        _ => return,
    };
    let _ = db.transition_autopilot(
        run.id,
        run.state,
        next,
        Some("PR queue observed merged"),
        Some(number),
        thegn_core::util::now_ms(),
    );
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
