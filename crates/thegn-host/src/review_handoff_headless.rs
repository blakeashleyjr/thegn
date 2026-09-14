//! Blocking preparation and admission for the interactive headless fallback.
//!
//! The caller captures the selected view and checkout before queuing this work.
//! Only the worker reads DB/git/provider state or resolves a sandbox. The shared
//! proof is rechecked before launch; it does not freeze subsequent credentials
//! used by the historical login-shell runner (THE-541/THE-233).

use std::path::PathBuf;

use thegn_core::agent_task::TaskKind;
use thegn_core::config::{Config, PrQueueConfig};
use thegn_core::db::Db;
use thegn_core::forge::{Forge, PrRef};
use thegn_core::remote::GitLoc;
use thegn_core::review::PrReviewSnapshot;
use thegn_core::sandbox::SandboxSpec;

use crate::agent_run::{AgentDispatch, AgentTaskRun};
use crate::pr_authorship;

pub(super) struct Request {
    pub worktree: PathBuf,
    pub snapshot: PrReviewSnapshot,
    pub command: String,
    pub title: String,
    pub url: String,
    pub base: String,
    pub feedback: String,
}

pub(super) fn run(cfg: &Config, queue: &PrQueueConfig, request: &Request) -> Result<(), String> {
    let prepare = || match crate::agent_run::agent_floor_gate(
        cfg,
        &GitLoc::worktree_cache_key(&request.worktree),
        queue.agent_sandbox,
        queue.agent_isolation_floor,
        queue.agent_on_floor_miss,
    ) {
        AgentDispatch::Run(spec) => Ok(spec),
        AgentDispatch::RunDegraded(spec, warning) => {
            thegn_core::msg::warn(&warning);
            Ok(spec)
        }
        AgentDispatch::InfraHold(reason) => Err(format!("review handoff blocked: {reason}")),
    };
    if !queue.own_prs_only {
        // Explicit broader policy keeps the existing path, including no new
        // database or provider prerequisites.
        return execute(queue, request, None, prepare, crate::agent_run::run);
    }
    let db = Db::open().map_err(|_| "review handoff held: workspace DB unavailable")?;
    let loc = GitLoc::Local(request.worktree.clone());
    // Before resolving a forge (which may inspect git), prove this local
    // runner is the persisted location. Unknown and nonlocal routes hold.
    pr_authorship::local_execution_location(&db, &loc)?;
    let forges = crate::forge_handle::get();
    let forge = forges.for_loc(&loc);
    execute(
        queue,
        request,
        Some((&db, forge)),
        prepare,
        crate::agent_run::run,
    )
}

/// The production admission/launch path; injection keeps regression tests on
/// this exact ordering without starting agents or touching real credentials.
fn execute(
    queue: &PrQueueConfig,
    request: &Request,
    authority: Option<(&Db, &dyn Forge)>,
    prepare: impl FnOnce() -> Result<Option<SandboxSpec>, String>,
    launch: impl FnOnce(&AgentTaskRun<'_>) -> bool,
) -> Result<(), String> {
    let worktree = GitLoc::worktree_cache_key(&request.worktree);
    let loc = GitLoc::Local(request.worktree.clone());
    let admitted = if queue.own_prs_only {
        let (db, forge) = authority.ok_or(pr_authorship::HELD)?;
        if !request.worktree.is_absolute()
            || request.snapshot.worktree_key != worktree
            || request.snapshot.pr_number == 0
            || request.snapshot.head_oid.is_empty()
        {
            return Err(pr_authorship::HELD.into());
        }
        pr_authorship::local_execution_location(db, &loc)?;
        let pr = forge
            .pr_status(&loc, PrRef::Number(request.snapshot.pr_number))
            .map_err(|_| pr_authorship::HELD)?;
        if pr.number != request.snapshot.pr_number
            || pr.head_ref_oid != request.snapshot.head_oid
            || pr.head_ref_name != request.snapshot.branch
        {
            return Err(pr_authorship::HELD.into());
        }
        let permit = pr_authorship::acquire(
            true,
            db,
            forge,
            &loc,
            forge.id(),
            request.snapshot.pr_number,
            &pr,
        )?
        .ok_or(pr_authorship::HELD)?;
        if !permit.matches_pr_url(&request.url, request.snapshot.pr_number) {
            return Err(pr_authorship::HELD.into());
        }
        Some((db, forge, permit, pr))
    } else {
        None
    };

    // Authorship precedes sandbox resolution, which can have external effects.
    let sandbox = prepare()?;
    let (base, title, url) = admitted
        .as_ref()
        .map(|(_, _, _, pr)| {
            (
                pr.base_ref_name.as_str(),
                pr.title.as_str(),
                pr.url.as_str(),
            )
        })
        .unwrap_or((&request.base, &request.title, &request.url));
    let vars = super::vars(
        &request.snapshot,
        base,
        title,
        url,
        &worktree,
        request.feedback.clone(),
    );
    let prompt =
        thegn_core::agent_task::render_prompt(queue.prompts.resolve(TaskKind::PrReview), &vars)
            .map_err(|_| "review handoff held: configured review prompt is invalid")?;
    if let Some((db, forge, permit, pr)) = &admitted {
        pr_authorship::revalidate(Some(permit), db, *forge, &loc, pr, false)?;
    }
    if launch(&AgentTaskRun {
        kind: TaskKind::PrReview,
        worktree: &worktree,
        prompt: &prompt,
        command_template: &request.command,
        vars: &vars,
        timeout_secs: queue.agent_timeout_secs,
        sandbox,
        credential_free: false,
    }) {
        Ok(())
    } else {
        Err("PR review agent handoff failed".into())
    }
}

#[cfg(test)]
#[path = "review_handoff_headless_tests.rs"]
mod tests;
