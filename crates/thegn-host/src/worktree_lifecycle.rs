//! Shared worktree create/destroy/session lifecycle orchestration.
//!
//! This module resolves the typed core policy and delegates process execution
//! to [`crate::hook_run`]. Physical git/provider operations remain at their
//! existing call sites; this seam owns hook ordering and failure semantics.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::{Mutex, OnceLock};
use thegn_core::config::Config;
use thegn_core::config_resolve::Approvals;
use thegn_core::db::Db;
use thegn_core::hooks::{HookContext, HookEvent, HookExecutionMode, ResolvedHooks};
use thegn_core::store::RepoTrustStore;

static NOTIFY_STATE: OnceLock<Arc<crate::notify::NotifyState>> = OnceLock::new();

/// Install the compositor's durable notification funnel for background hook
/// completions. Headless callers still receive `thegn_core::msg` output.
pub fn install_notify_state(state: Arc<crate::notify::NotifyState>) {
    let _ = NOTIFY_STATE.set(state);
}

/// Result of an event, including the first blocking failure if any.
#[derive(Debug, Clone)]
pub struct LifecycleReport {
    pub event: HookEvent,
    pub results: Vec<crate::hook_run::HookRunResult>,
    #[allow(dead_code)]
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
    let (repo_hooks, repo_prepare) = thegn_core::config::load_repo_hooks(repo_root)
        .unwrap_or_else(|| (thegn_core::config::HooksConfig::default(), Vec::new()));
    let workspace = cfg
        .workspace
        .get(&thegn_core::config::workspace_slug(repo_root));
    thegn_core::hooks::resolve(
        &cfg.hooks,
        workspace.map(|workspace| &workspace.hooks),
        Some(&repo_hooks),
        &cfg.sandbox.prepare,
        &repo_prepare,
        &approvals,
    )
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

#[allow(clippy::too_many_arguments)]
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
    for result in &results {
        if !result.succeeded() {
            thegn_core::msg::warn(&format!("{}: {}", event.as_str(), result.summary()));
            report_failure(&context, result);
        }
    }
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

/// Run post-create synchronously only when an entry explicitly requests
/// `wait=true`; otherwise leave the real worktree registered and schedule the
/// event without delaying the first pane.
#[allow(clippy::too_many_arguments)]
pub fn schedule_post_create(
    cfg: &Config,
    repo_root: &Path,
    worktree: &Path,
    branch: &str,
    workspace: &str,
    db: Option<&Db>,
    waker: Option<termwiz::terminal::TerminalWaker>,
) {
    let policy = resolve(cfg, repo_root, db);
    let waits_for_pane = policy
        .entries(HookEvent::PostCreate)
        .iter()
        .any(|spec| spec.wait);
    if waits_for_pane {
        let _ = run_event_with_db(
            cfg,
            repo_root,
            worktree,
            branch,
            workspace,
            HookEvent::PostCreate,
            HookExecutionMode::User,
            db,
        );
    } else {
        spawn_event(
            cfg.clone(),
            repo_root.to_path_buf(),
            worktree.to_path_buf(),
            branch.to_string(),
            workspace.to_string(),
            HookEvent::PostCreate,
            HookExecutionMode::User,
            waker,
        );
    }
}

/// Schedule a non-blocking lifecycle event. The worker owns all process and
/// filesystem work and pulses the existing terminal wake source on completion.
#[allow(clippy::too_many_arguments)]
pub fn spawn_event(
    cfg: Config,
    repo_root: PathBuf,
    worktree: PathBuf,
    branch: String,
    workspace: String,
    event: HookEvent,
    mode: HookExecutionMode,
    waker: Option<termwiz::terminal::TerminalWaker>,
) {
    let handle = std::thread::Builder::new()
        .name(format!("thegn-resolve-hook-{}", event.as_str()))
        .spawn(move || {
            crate::platform::qos::set_self(crate::platform::qos::Qos::Utility);
            let resolved_root = if matches!(event, HookEvent::SessionStart | HookEvent::SessionEnd)
            {
                thegn_core::repo::main_worktree(&worktree).unwrap_or(repo_root)
            } else {
                repo_root
            };
            let db = Db::open().ok();
            let policy = resolve(&cfg, &resolved_root, db.as_ref());
            let specs = policy.entries(event).to_vec();
            if !policy.pending.is_empty() {
                thegn_core::msg::warn(&format!(
                    "{} repo lifecycle hook request(s) await trust approval",
                    policy.pending.len()
                ));
            }
            let actual_branch = if branch.is_empty() {
                thegn_core::util::git_out(
                    &worktree,
                    &["symbolic-ref", "--quiet", "--short", "HEAD"],
                )
                .unwrap_or_default()
            } else {
                branch
            };
            let actual_workspace = if workspace.is_empty() {
                thegn_core::repo::repo_slug(&resolved_root)
            } else {
                workspace
            };
            if !specs.is_empty() {
                let context = context(
                    event,
                    &resolved_root,
                    &worktree,
                    &actual_branch,
                    &actual_workspace,
                );
                let cwd = cwd(event, &resolved_root, &worktree);
                let results = crate::hook_run::run_all(&specs, &context, &cwd, mode);
                for result in &results {
                    if !result.succeeded() {
                        thegn_core::msg::warn(&format!("{}: {}", event.as_str(), result.summary()));
                        report_failure(&context, result);
                    }
                }
            }
            if let Some(waker) = waker {
                let _ = waker.wake();
            }
        });
    if let Ok(handle) = handle {
        // Detached UI/daemon work reports failures from inside the worker and
        // pulses the compositor after completion.
        std::mem::forget(handle);
    }
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

/// Force-clean a speculative wizard worktree. Rollback must not leak a
/// checkout because a hook failed while the user was cancelling or renaming.
pub fn rollback_remove(cfg: &Config, repo_root: &Path, worktree: &Path, branch: &str) {
    let workspace = thegn_core::repo::repo_slug(repo_root);
    let existed = worktree.exists();
    let pre = run_event(
        cfg,
        repo_root,
        worktree,
        branch,
        &workspace,
        HookEvent::PreDestroy,
        HookExecutionMode::Force,
    );
    if pre.results.iter().any(|result| !result.succeeded()) {
        thegn_core::msg::warn(&format!("wizard rollback: {}", pre.message()));
    }
    thegn_core::worktree::remove(repo_root, worktree, branch, true);
    if existed && !worktree.exists() {
        let post = run_event(
            cfg,
            repo_root,
            worktree,
            branch,
            &workspace,
            HookEvent::PostDestroy,
            HookExecutionMode::Force,
        );
        if post.results.iter().any(|result| !result.succeeded()) {
            thegn_core::msg::warn(&format!("wizard rollback: {}", post.message()));
        }
    }
}

static SESSION_LATCHES: OnceLock<Mutex<HashSet<(String, String)>>> = OnceLock::new();

fn session_latches() -> &'static Mutex<HashSet<(String, String)>> {
    SESSION_LATCHES.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Schedule `session_start` once for the current host session and worktree.
/// The latch is process-local and intentionally not persisted in SQLite.
pub fn session_start_once(
    cfg: &Config,
    worktree: &Path,
    waker: Option<termwiz::terminal::TerminalWaker>,
) {
    let key = (
        worktree.to_string_lossy().into_owned(),
        std::process::id().to_string(),
    );
    if !session_latches().lock().unwrap().insert(key) {
        return;
    }
    if worktree.as_os_str().is_empty() {
        return;
    }
    spawn_session_event(
        cfg.clone(),
        worktree.to_path_buf(),
        HookEvent::SessionStart,
        waker,
    );
}

/// Schedule `session_end` once for the current host session and worktree.
pub fn session_end_once(
    cfg: &Config,
    worktree: &Path,
    waker: Option<termwiz::terminal::TerminalWaker>,
) {
    let key = (
        worktree.to_string_lossy().into_owned(),
        std::process::id().to_string(),
    );
    if !session_latches().lock().unwrap().remove(&key) {
        return;
    }
    spawn_session_event(
        cfg.clone(),
        worktree.to_path_buf(),
        HookEvent::SessionEnd,
        waker,
    );
}

fn spawn_session_event(
    cfg: Config,
    worktree: PathBuf,
    event: HookEvent,
    waker: Option<termwiz::terminal::TerminalWaker>,
) {
    // `spawn_event` performs trust/config resolution on its worker and derives
    // the real repo root from this worktree path.
    spawn_event(
        cfg,
        worktree.clone(),
        worktree,
        String::new(),
        String::new(),
        event,
        HookExecutionMode::Unattended,
        waker,
    );
}

fn report_failure(context: &HookContext, result: &crate::hook_run::HookRunResult) {
    let Some(state) = NOTIFY_STATE.get() else {
        return;
    };
    let Ok(db) = Db::open() else {
        return;
    };
    let tail = result.failure_tail();
    let message = if tail.is_empty() {
        format!("{}: {}", context.event.as_str(), result.summary())
    } else {
        format!(
            "{}: {}\noutput tail:\n{}",
            context.event.as_str(),
            result.summary(),
            tail
        )
    };
    let (decision, _) = crate::notify::record(
        &db,
        state,
        "hook_failed",
        context.event.as_str(),
        &message,
        &context.worktree,
    );
    state.emit_sound(&decision);
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
