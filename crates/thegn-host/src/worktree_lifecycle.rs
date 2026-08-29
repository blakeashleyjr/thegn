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
static REFRESH_TX: OnceLock<tokio::sync::mpsc::UnboundedSender<crate::hydrate::RefreshKind>> =
    OnceLock::new();
static COMPLETIONS: OnceLock<Mutex<Vec<LifecycleCompletion>>> = OnceLock::new();

/// Completion delivered by an off-loop lifecycle worker. The loop owns all
/// session/model/DB reconciliation; workers only perform hooks and disk work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LifecycleCompletion {
    WorktreeDelete {
        group_name: String,
        path: String,
        success: bool,
        message: String,
    },
    WorkspaceDelete {
        repo_path: String,
        slug: String,
        path: String,
        success: bool,
        message: String,
    },
    WorkspaceDeleteFinished {
        repo_path: String,
        slug: String,
        failed_paths: Vec<String>,
    },
}

fn completions() -> &'static Mutex<Vec<LifecycleCompletion>> {
    COMPLETIONS.get_or_init(|| Mutex::new(Vec::new()))
}

/// Install the event loop's existing refresh channel for lifecycle workers.
pub fn install_refresh(tx: tokio::sync::mpsc::UnboundedSender<crate::hydrate::RefreshKind>) {
    let _ = REFRESH_TX.set(tx);
}

pub fn take_completions() -> Vec<LifecycleCompletion> {
    std::mem::take(&mut *completions().lock().expect("lifecycle mutex poisoned"))
}

/// Apply worker outcomes on the compositor loop. No hook, git, or filesystem
/// work occurs here; this only reconciles the in-memory session and cache after
/// a worker has proven that its requested removal completed.
pub fn apply_completions(
    cfg: &Config,
    session: &mut crate::session::Session,
    panes: &mut crate::panes::Panes,
    model: &mut crate::chrome::FrameModel,
    sb: &mut crate::run::SidebarState,
    waker: &termwiz::terminal::TerminalWaker,
) -> bool {
    let mut changed = false;
    let mut workspace_failures: std::collections::HashMap<(String, String), Vec<String>> =
        std::collections::HashMap::new();
    for completion in take_completions() {
        match completion {
            LifecycleCompletion::WorktreeDelete {
                group_name,
                path,
                success,
                message,
            } => {
                if success {
                    if let Some(gi) = session
                        .worktrees
                        .iter()
                        .position(|g| g.name == group_name || g.path == path)
                    {
                        let group = session.worktrees[gi].clone();
                        if let Ok(db) = Db::open() {
                            crate::run::forget_worktree_group(&db, &session.id, &group);
                            let _ = session.persist(&db, &session.id, crate::run::now_secs());
                        }
                        for tab in &group.tabs {
                            for id in tab.center.pane_ids() {
                                panes.table.remove(&id);
                            }
                        }
                        session.switch_to(gi);
                        session.close_active_group();
                        session_end_once(cfg, Path::new(&path), Some(waker.clone()));
                        model.status = format!("Deleted worktree {path}");
                        changed = true;
                    }
                } else {
                    model.status = format!("Worktree delete failed for {path}: {message}");
                    changed = true;
                }
            }
            LifecycleCompletion::WorkspaceDelete {
                repo_path,
                slug,
                path,
                success,
                message,
            } => {
                let key = (repo_path.clone(), slug.clone());
                if success {
                    if let Some(gi) = session.worktrees.iter().position(|g| g.path == path) {
                        let group = session.worktrees[gi].clone();
                        if let Ok(db) = Db::open() {
                            crate::run::forget_worktree_group(&db, &session.id, &group);
                            let _ = session.persist(&db, &session.id, crate::run::now_secs());
                        }
                        for tab in &group.tabs {
                            for id in tab.center.pane_ids() {
                                panes.table.remove(&id);
                            }
                        }
                        session.switch_to(gi);
                        session.close_active_group();
                        session_end_once(cfg, Path::new(&path), Some(waker.clone()));
                    }
                } else {
                    workspace_failures
                        .entry(key)
                        .or_default()
                        .push(format!("{path}: {message}"));
                }
                changed = true;
            }
            LifecycleCompletion::WorkspaceDeleteFinished {
                repo_path,
                slug,
                failed_paths,
            } => {
                if failed_paths.is_empty() {
                    if let Ok(db) = Db::open() {
                        crate::handlers::workspace_remove::remove_workspace_with_db(
                            session,
                            panes,
                            Some(&db),
                            &repo_path,
                            &slug,
                        );
                        if session.id == repo_path {
                            crate::handlers::workspace_remove::land_after_workspace_removed(
                                session,
                                Some(&db),
                            );
                        }
                    }
                    crate::handlers::workspace_remove::forget_workspace_in_model(
                        model, &slug, &repo_path,
                    );
                    model.status = format!("Removed workspace '{slug}'");
                } else {
                    let details = workspace_failures
                        .remove(&(repo_path.clone(), slug.clone()))
                        .unwrap_or_default();
                    model.status = format!(
                        "Workspace '{slug}' kept; failed worktrees: {}",
                        if details.is_empty() {
                            failed_paths.join(", ")
                        } else {
                            details.join("; ")
                        }
                    );
                }
                changed = true;
            }
        }
    }
    if changed {
        crate::run::persist_session_layout(session, panes);
        crate::run::refresh_tab_model(model, session, sb);
        sb.focus_active_row(model);
    }
    changed
}

fn complete(completion: LifecycleCompletion, waker: Option<termwiz::terminal::TerminalWaker>) {
    completions()
        .lock()
        .expect("lifecycle mutex poisoned")
        .push(completion);
    if let Some(tx) = REFRESH_TX.get() {
        let _ = tx.send(crate::hydrate::RefreshKind::Model);
    }
    if let Some(waker) = waker {
        let _ = waker.wake();
    }
}

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

/// Run one user/unattended destroy transaction off-loop and report its result.
/// The caller does not prune the live group until the completion says the disk
/// operation really succeeded.
pub fn spawn_worktree_destroy(
    cfg: Config,
    repo_root: PathBuf,
    worktree: PathBuf,
    branch: String,
    workspace: String,
    group_name: String,
    keep_files: bool,
    mode: HookExecutionMode,
    waker: Option<termwiz::terminal::TerminalWaker>,
) {
    std::thread::spawn(move || {
        crate::platform::qos::set_self(crate::platform::qos::Qos::Utility);
        let (success, message) = destroy_one(
            &cfg, &repo_root, &worktree, &branch, &workspace, keep_files, mode,
        );
        complete(
            LifecycleCompletion::WorktreeDelete {
                group_name,
                path: worktree.to_string_lossy().into_owned(),
                success,
                message,
            },
            waker,
        );
    });
}

/// Run each workspace path as an independent transaction. A failed path is
/// reported without preventing its siblings from completing.
pub fn spawn_workspace_destroy(
    cfg: Config,
    repo_root: PathBuf,
    slug: String,
    paths: Vec<(String, String)>,
    waker: Option<termwiz::terminal::TerminalWaker>,
) {
    std::thread::spawn(move || {
        crate::platform::qos::set_self(crate::platform::qos::Qos::Utility);
        let mut failed_paths = Vec::new();
        for (path, branch) in paths {
            let (success, message) = destroy_one(
                &cfg,
                &repo_root,
                Path::new(&path),
                &branch,
                &slug,
                false,
                HookExecutionMode::Force,
            );
            if !success {
                failed_paths.push(format!("{path}: {message}"));
            }
            complete(
                LifecycleCompletion::WorkspaceDelete {
                    repo_path: repo_root.to_string_lossy().into_owned(),
                    slug: slug.clone(),
                    path,
                    success,
                    message,
                },
                waker.clone(),
            );
        }
        complete(
            LifecycleCompletion::WorkspaceDeleteFinished {
                repo_path: repo_root.to_string_lossy().into_owned(),
                slug,
                failed_paths,
            },
            waker,
        );
    });
}

fn destroy_one(
    cfg: &Config,
    repo_root: &Path,
    worktree: &Path,
    branch: &str,
    workspace: &str,
    keep_files: bool,
    mode: HookExecutionMode,
) -> (bool, String) {
    let pre = run_event(
        cfg,
        repo_root,
        worktree,
        branch,
        workspace,
        HookEvent::PreDestroy,
        mode,
    );
    if pre.blocked() {
        return (false, pre.message());
    }

    let removed = if keep_files {
        let marker = worktree.join(".git");
        match std::fs::remove_file(&marker) {
            Ok(()) => thegn_core::util::git_ok(repo_root, &["worktree", "prune"]),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                thegn_core::util::git_ok(repo_root, &["worktree", "prune"])
            }
            Err(error) => {
                thegn_core::msg::warn(&format!("could not detach kept worktree: {error}"));
                false
            }
        }
    } else if thegn_core::worktree::remove(repo_root, worktree, "", false) {
        thegn_core::worktree::purge_worktree_files(worktree);
        !worktree.exists()
    } else {
        false
    };
    if !removed {
        return (
            false,
            format!("could not remove worktree at {}", worktree.display()),
        );
    }

    let post = run_event(
        cfg,
        repo_root,
        worktree,
        branch,
        workspace,
        HookEvent::PostDestroy,
        mode,
    );
    if post.results.iter().any(|result| !result.succeeded()) {
        // Post-destroy is warn-only, so physical success remains success.
        return (true, post.message());
    }
    (true, "removed".into())
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

/// End a worktree session when the pane that just exited was its last live
/// process. The exited pane is still present in the session tree until the
/// drain reconciles it, so callers pass its id explicitly.
pub fn session_end_after_pane_exit(
    cfg: &Config,
    session: &crate::session::Session,
    exited_pane: u32,
    waker: Option<termwiz::terminal::TerminalWaker>,
) {
    for group in &session.worktrees {
        if group.path.is_empty() {
            continue;
        }
        let has_exited = group
            .tabs
            .iter()
            .any(|tab| tab.center.pane_ids().contains(&exited_pane));
        if has_exited {
            let other_live = group.tabs.iter().any(|tab| {
                tab.center
                    .pane_ids()
                    .into_iter()
                    .any(|id| id != exited_pane)
            });
            if !other_live {
                session_end_once(cfg, Path::new(&group.path), waker);
            }
            return;
        }
    }
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
