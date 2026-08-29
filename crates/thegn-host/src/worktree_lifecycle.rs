//! Shared worktree create/destroy/session lifecycle orchestration.
//!
//! This module resolves the typed core policy and delegates process execution
//! to [`crate::hook_run`]. Physical git/provider operations remain at their
//! existing call sites; this seam owns hook ordering and failure semantics.

use std::path::{Path, PathBuf};
use thegn_core::config::Config;
use thegn_core::config_resolve::Approvals;
use thegn_core::db::Db;
use thegn_core::hooks::{HookContext, HookEvent, HookExecutionMode, ResolvedHooks};
use thegn_core::store::RepoTrustStore;

/// Result of an event, including the first blocking failure if any.
#[derive(Debug, Clone)]
pub struct LifecycleReport {
    pub event: HookEvent,
    pub results: Vec<crate::hook_run::HookRunResult>,
    pub pending: Vec<thegn_core::config_resolve::GatedRequest>,
    blocked_failure: bool,
}

impl LifecycleReport {
    pub fn blocked(&self) -> bool {
        self.blocked_failure
    }

    pub fn message(&self) -> String {
        self.results
            .iter()
            .find(|result| !result.succeeded())
            .map(crate::hook_run::HookRunResult::summary)
            .unwrap_or_else(|| format!("{} hooks completed", self.event.as_str()))
    }
}

/// Resolve hooks using the DB's persisted trust approvals. Repo hooks remain
/// absent while pending; the caller can surface `pending` through its normal
/// trust/notification UI.
pub fn resolve(cfg: &Config, repo_root: &Path, db: Option<&Db>) -> ResolvedHooks {
    let approvals = db
        .and_then(|db| db.repo_trust_approved(&repo_root.to_string_lossy()).ok())
        .map(Approvals::from_canonical)
        .unwrap_or_else(Approvals::deny_all);
    thegn_core::hooks::resolve_for_repo(cfg, repo_root, &approvals)
}

/// Build the event environment and execute its hooks synchronously. This is
/// intended for CLI calls and existing background workers; compositor callers
/// should use [`spawn_event`].
pub fn run_event(
    cfg: &Config,
    repo_root: &Path,
    worktree: &Path,
    branch: &str,
    workspace: &str,
    event: HookEvent,
    mode: HookExecutionMode,
) -> LifecycleReport {
    let db = Db::open().ok();
    run_event_with_db(
        cfg,
        repo_root,
        worktree,
        branch,
        workspace,
        event,
        mode,
        db.as_ref(),
    )
}

pub fn run_event_with_db(
    cfg: &Config,
    repo_root: &Path,
    worktree: &Path,
    branch: &str,
    workspace: &str,
    event: HookEvent,
    mode: HookExecutionMode,
    db: Option<&Db>,
) -> LifecycleReport {
    let policy = resolve(cfg, repo_root, db);
    let specs = policy.entries(event).to_vec();
    let context = context(event, repo_root, worktree, branch, workspace);
    let cwd = cwd(event, repo_root, worktree);
    let results = crate::hook_run::run_all(&specs, &context, &cwd, mode);
    let blocked_failure = results
        .iter()
        .zip(specs.iter())
        .any(|(result, spec)| !result.succeeded() && spec.blocks_failure(mode));
    if !policy.pending.is_empty() {
        thegn_core::msg::warn(&format!(
            "{} repo lifecycle hook request(s) await trust approval",
            policy.pending.len()
        ));
    }
    LifecycleReport {
        event,
        results,
        pending: policy.pending,
        blocked_failure,
    }
}

/// Schedule a non-blocking lifecycle event. The worker owns all process and
/// filesystem work and pulses the existing terminal wake source on completion.
pub fn spawn_event(
    cfg: Config,
    repo_root: PathBuf,
    worktree: PathBuf,
    branch: String,
    workspace: String,
    event: HookEvent,
    mode: HookExecutionMode,
    waker: Option<termwiz::terminal::TerminalWaker>,
) -> Option<std::thread::JoinHandle<Vec<crate::hook_run::HookRunResult>>> {
    let db = Db::open().ok();
    let policy = resolve(&cfg, &repo_root, db.as_ref());
    let specs = policy.entries(event).to_vec();
    if specs.is_empty() {
        return None;
    }
    let context = context(event, &repo_root, &worktree, &branch, &workspace);
    let cwd = cwd(event, &repo_root, &worktree);
    Some(crate::hook_run::spawn_all(specs, context, cwd, mode, waker))
}

fn context(
    event: HookEvent,
    repo_root: &Path,
    worktree: &Path,
    branch: &str,
    workspace: &str,
) -> HookContext {
    HookContext {
        event,
        repo_root: repo_root.to_string_lossy().into_owned(),
        worktree: worktree.to_string_lossy().into_owned(),
        branch: branch.to_string(),
        workspace: workspace.to_string(),
    }
}

fn cwd(event: HookEvent, repo_root: &Path, worktree: &Path) -> PathBuf {
    match event {
        HookEvent::PreCreate | HookEvent::PostDestroy => repo_root.to_path_buf(),
        HookEvent::PostCreate
        | HookEvent::PreDestroy
        | HookEvent::SessionStart
        | HookEvent::SessionEnd => worktree.to_path_buf(),
    }
}

/// The standard user-facing blocking policy for an event. Kept here so all
/// physical removal paths can use the same explicit force/unattended switch.
pub const fn mode_for_user(force: bool, unattended: bool) -> HookExecutionMode {
    if unattended {
        HookExecutionMode::Unattended
    } else if force {
        HookExecutionMode::Force
    } else {
        HookExecutionMode::User
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cwd_contract_uses_repo_for_create_before_and_destroy_after() {
        assert_eq!(
            cwd(HookEvent::PreCreate, Path::new("r"), Path::new("w")),
            Path::new("r")
        );
        assert_eq!(
            cwd(HookEvent::PostDestroy, Path::new("r"), Path::new("w")),
            Path::new("r")
        );
        assert_eq!(
            cwd(HookEvent::PreDestroy, Path::new("r"), Path::new("w")),
            Path::new("w")
        );
    }

    #[test]
    fn force_and_unattended_modes_are_distinct() {
        assert_eq!(mode_for_user(false, false), HookExecutionMode::User);
        assert_eq!(mode_for_user(true, false), HookExecutionMode::Force);
        assert_eq!(mode_for_user(false, true), HookExecutionMode::Unattended);
    }
}
