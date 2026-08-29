//! Shared worktree create/destroy/session lifecycle orchestration.
//!
//! This module resolves the typed core policy and delegates process execution
//! to [`crate::hook_run`]. Physical git/provider operations remain at their
//! existing call sites; this seam owns hook ordering and failure semantics.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::{Condvar, Mutex, OnceLock};
use thegn_core::config::Config;
use thegn_core::config_resolve::Approvals;
use thegn_core::db::Db;
use thegn_core::hooks::{HookContext, HookEvent, HookExecutionMode, ResolvedHooks};
use thegn_core::store::{RepoTrustStore, WorkspaceStore};

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

/// Deliver a typed background result to the compositor and pulse its existing
/// wake source. The producer is off-loop; a closed channel only means the
/// compositor has already gone away.
pub(crate) fn send_refresh(
    kind: crate::hydrate::RefreshKind,
    waker: Option<termwiz::terminal::TerminalWaker>,
) {
    if let Some(tx) = REFRESH_TX.get() {
        let _ = tx.send(kind); // best-effort: the consumer may be gone
    }
    if let Some(waker) = waker {
        let _ = waker.wake(); // best-effort: the compositor may be shutting down
    }
}

pub fn take_completions() -> Vec<LifecycleCompletion> {
    std::mem::take(&mut *completions().lock().expect("lifecycle mutex poisoned"))
}

/// Apply worker outcomes on the compositor loop. No hook, git, or filesystem
/// work occurs here; this only reconciles in-memory session, pane, model, and
/// focus state after a worker has proven that its requested removal completed.
pub fn apply_completions(
    session: &mut crate::session::Session,
    panes: &mut crate::panes::Panes,
    model: &mut crate::chrome::FrameModel,
    sb: &mut crate::run::SidebarState,
) -> bool {
    apply_completions_from(take_completions(), session, panes, model, sb)
}

/// Apply a supplied batch of worker outcomes on the compositor loop. Keeping
/// the batch-taking wrapper above separate makes this seam testable without
/// racing the process-global completion queue.
fn apply_completions_from(
    completions: Vec<LifecycleCompletion>,
    session: &mut crate::session::Session,
    panes: &mut crate::panes::Panes,
    model: &mut crate::chrome::FrameModel,
    sb: &mut crate::run::SidebarState,
) -> bool {
    let mut changed = false;
    let mut workspace_failures: std::collections::HashMap<(String, String), Vec<String>> =
        std::collections::HashMap::new();
    for completion in completions {
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
                        for tab in &group.tabs {
                            for id in tab.center.pane_ids() {
                                panes.table.remove(&id);
                            }
                        }
                        session.switch_to(gi);
                        session.close_active_group();
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
                        for tab in &group.tabs {
                            for id in tab.center.pane_ids() {
                                panes.table.remove(&id);
                            }
                        }
                        session.switch_to(gi);
                        session.close_active_group();
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
                    crate::handlers::workspace_remove::remove_workspace_in_memory(
                        session, panes, &slug,
                    );
                    if session.id == repo_path {
                        crate::handlers::workspace_remove::land_after_workspace_removed(
                            session, None,
                        );
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
    dispatch_error: Option<String>,
}

impl LifecycleReport {
    pub fn blocked(&self) -> bool {
        self.blocked_failure
    }

    pub fn message(&self) -> String {
        self.dispatch_error.clone().unwrap_or_else(|| {
            self.results
                .iter()
                .find(|result| !result.succeeded())
                .map(crate::hook_run::HookRunResult::summary)
                .unwrap_or_else(|| format!("{} hooks completed", self.event.as_str()))
        })
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
        } else if result.log_error.is_some() {
            report_log_failure(&context, result);
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
        dispatch_error: None,
    }
}

fn spawn_failure_report(event: HookEvent, error: std::io::Error) -> LifecycleReport {
    LifecycleReport {
        event,
        results: Vec::new(),
        pending: Vec::new(),
        blocked_failure: true,
        dispatch_error: Some(format!(
            "failed to schedule {} lifecycle worker: {error}",
            event.as_str()
        )),
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
) -> Result<(), LifecycleReport> {
    let policy = resolve(cfg, repo_root, db);
    let waits_for_pane = policy
        .entries(HookEvent::PostCreate)
        .iter()
        .any(|spec| spec.wait);
    if waits_for_pane {
        let report = run_event_with_db(
            cfg,
            repo_root,
            worktree,
            branch,
            workspace,
            HookEvent::PostCreate,
            HookExecutionMode::User,
            db,
        );
        if report.blocked() {
            Err(report)
        } else {
            Ok(())
        }
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
        )
        .map_err(|error| spawn_failure_report(HookEvent::PostCreate, error))
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
) -> std::io::Result<()> {
    spawn_event_with_completion(
        cfg, repo_root, worktree, branch, workspace, event, mode, waker, None,
    )
}

#[allow(clippy::too_many_arguments)]
fn spawn_event_with_completion(
    cfg: Config,
    repo_root: PathBuf,
    worktree: PathBuf,
    branch: String,
    workspace: String,
    event: HookEvent,
    mode: HookExecutionMode,
    waker: Option<termwiz::terminal::TerminalWaker>,
    completion: Option<SessionEndGuard>,
) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name(format!("thegn-resolve-hook-{}", event.as_str()))
        .spawn(move || {
            // Holding this guard for the entire worker makes destructive
            // teardown wait for all hook/report/waker work. It also releases
            // the in-flight claim if the worker unwinds.
            let _completion = completion;
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
                    } else if result.log_error.is_some() {
                        report_log_failure(&context, result);
                    }
                }
            }
            // Hook completion is a model change even when no lifecycle
            // transaction completion is queued. Keep the compositor's
            // existing refresh funnel in sync with the completion pulse.
            if let Some(tx) = REFRESH_TX.get() {
                let _ = tx.send(crate::hydrate::RefreshKind::Model);
            }
            if let Some(waker) = waker {
                let _ = waker.wake();
            }
        })
        // Dropping a JoinHandle detaches the worker. Errors remain owned by the
        // caller instead of being mistaken for a successfully scheduled event.
        .map(drop)
}

/// Run one user/unattended destroy transaction off-loop and report its result.
/// The caller does not prune the live group until the completion says the disk
/// operation really succeeded.
pub fn spawn_worktree_destroy(
    worktree: PathBuf,
    group_name: String,
    session_id: String,
    keep_files: bool,
    mode: HookExecutionMode,
    waker: Option<termwiz::terminal::TerminalWaker>,
) -> std::io::Result<()> {
    let claimed_path = worktree.clone();
    let spawned = std::thread::Builder::new()
        .name("thegn-worktree-destroy".into())
        .spawn(move || {
            crate::platform::qos::set_self(crate::platform::qos::Qos::Utility);
            let cfg = Config::load_layered(&thegn_core::config::ProcessEnv, &[], None);
            let db = Db::open().ok();
            let (success, message) = thegn_core::repo::main_worktree(&worktree)
                .map(|repo_root| {
                    let branch = thegn_core::util::git_out(
                        &worktree,
                        &["symbolic-ref", "--quiet", "--short", "HEAD"],
                    )
                    .unwrap_or_default();
                    let workspace = thegn_core::repo::repo_slug(&repo_root);
                    destroy_one(
                        &cfg,
                        &repo_root,
                        &worktree,
                        &branch,
                        &workspace,
                        keep_files,
                        false,
                        mode,
                        db.as_ref(),
                    )
                })
                .unwrap_or_else(|| {
                    (
                        false,
                        format!("could not resolve repository for {}", worktree.display()),
                    )
                });
            if success && let Some(db) = db.as_ref() {
                crate::handlers::workspace_remove::forget_worktree_path_in_db(
                    db,
                    &session_id,
                    &worktree.to_string_lossy(),
                );
            }
            release_destroy_path(&worktree);
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
    release_destroy_claim_on_spawn_failure(&claimed_path, spawned.map(drop))
}

/// Run each workspace path as an independent transaction. A failed path is
/// reported without preventing its siblings from completing.
pub fn spawn_workspace_destroy(
    cfg: Config,
    repo_root: PathBuf,
    slug: String,
    session_id: String,
    paths: Vec<String>,
    waker: Option<termwiz::terminal::TerminalWaker>,
) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("thegn-workspace-destroy".into())
        .spawn(move || {
            crate::platform::qos::set_self(crate::platform::qos::Qos::Utility);
            let db = Db::open().ok();
            let mut candidates = paths;
            if let Some(db) = db.as_ref() {
                candidates.extend(crate::handlers::workspace_remove::workspace_worktree_dirs(
                    db,
                    &repo_root.to_string_lossy(),
                ));
            }
            candidates.sort();
            candidates.dedup();
            let had_candidates = !candidates.is_empty();
            let paths: Vec<String> = candidates
                .into_iter()
                .filter(|path| try_claim_destroy_path(Path::new(path)))
                .collect();
            if had_candidates && paths.is_empty() {
                complete(
                    LifecycleCompletion::WorkspaceDeleteFinished {
                        repo_path: repo_root.to_string_lossy().into_owned(),
                        slug,
                        failed_paths: vec!["all worktrees are already being deleted".into()],
                    },
                    waker,
                );
                return;
            }
            let mut failed_paths = Vec::new();
            for path in paths {
                let branch = thegn_core::util::git_out(
                    Path::new(&path),
                    &["symbolic-ref", "--quiet", "--short", "HEAD"],
                )
                .unwrap_or_default();
                let (success, message) = destroy_one(
                    &cfg,
                    &repo_root,
                    Path::new(&path),
                    &branch,
                    &slug,
                    false,
                    false,
                    HookExecutionMode::Force,
                    db.as_ref(),
                );
                if !success {
                    failed_paths.push(format!("{path}: {message}"));
                } else if let Some(db) = db.as_ref() {
                    crate::handlers::workspace_remove::forget_worktree_path_in_db(
                        db,
                        &session_id,
                        &path,
                    );
                }
                release_destroy_path(Path::new(&path));
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
            if failed_paths.is_empty()
                && let Some(db) = db.as_ref()
            {
                crate::handlers::workspace_remove::forget_workspace_in_db(
                    db,
                    &session_id,
                    &repo_root.to_string_lossy(),
                    &slug,
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
        })
        .map(drop)
}

fn release_destroy_claim_on_spawn_failure(
    path: &Path,
    spawned: std::io::Result<()>,
) -> std::io::Result<()> {
    if spawned.is_err() {
        release_destroy_path(path);
    }
    spawned
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn destroy_one(
    cfg: &Config,
    repo_root: &Path,
    worktree: &Path,
    branch: &str,
    workspace: &str,
    keep_files: bool,
    delete_branch: bool,
    mode: HookExecutionMode,
    db: Option<&Db>,
) -> (bool, String) {
    // The session boundary is live while `worktree` is still a valid cwd. It
    // must precede both the vetoing pre-hook and all teardown that can remove
    // the directory. The latch makes this once-only and warn-only.
    end_session_before_destroy(cfg, repo_root, worktree, branch, workspace, db);

    let pre = run_event_with_db(
        cfg,
        repo_root,
        worktree,
        branch,
        workspace,
        HookEvent::PreDestroy,
        mode,
        db,
    );
    if pre.blocked() {
        return (false, pre.message());
    }

    if let Err(error) = teardown_runtime(cfg, repo_root, worktree, db) {
        return (false, error);
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
    } else if thegn_core::worktree::remove(
        repo_root,
        worktree,
        if delete_branch { branch } else { "" },
        delete_branch,
    ) {
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

    let post = run_event_with_db(
        cfg,
        repo_root,
        worktree,
        branch,
        workspace,
        HookEvent::PostDestroy,
        mode,
        db,
    );
    if post.results.iter().any(|result| !result.succeeded()) {
        // Post-destroy is warn-only, so physical success remains success.
        return (true, post.message());
    }
    (true, "removed".into())
}

/// Tear down every runtime resource that was attached to the worktree while
/// its DB row and selected environment are still available. These operations
/// are deliberately off-loop because provider and projection teardown may do
/// network, mount, or container work.
fn teardown_runtime(
    cfg: &Config,
    repo_root: &Path,
    worktree: &Path,
    db: Option<&Db>,
) -> Result<(), String> {
    let selected = db
        .and_then(|db| db.effective_env(&worktree.to_string_lossy(), &repo_root.to_string_lossy()));
    let loc = thegn_core::remote::GitLoc::for_worktree(worktree);
    let env = cfg.resolve_env(repo_root, &loc, worktree, selected.as_deref());
    let path = worktree.to_string_lossy().into_owned();
    let mut failures = Vec::new();

    if !env.placement.is_local()
        && let Err(error) =
            crate::agent_teardown::destroy_provider_sandbox_with(cfg, &path, &env.name)
    {
        failures.push(format!("provider sandbox {}: {error}", env.name));
    }
    crate::agent::deregister_vpn(&path);
    crate::agent::deproject(&path);
    crate::agent::deprovision_sync(&path);
    crate::agent::checkpoint_on_close(&path);
    crate::bridge_sup::disconnect_path(&path);
    thegn_core::sandbox::teardown_by_path(&path);
    crate::placement_flow::release(&path);

    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("; "))
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
pub fn rollback_remove(
    cfg: &Config,
    repo_root: &Path,
    worktree: &Path,
    branch: &str,
) -> Result<(), String> {
    rollback_remove_with_branch_created(cfg, repo_root, worktree, branch, true)
}

/// Roll back a failed `git worktree add` without deleting a branch that was
/// already present before that add attempt. `branch_created` comes from the
/// core add operation while its git mutation lock was held.
pub fn rollback_remove_with_branch_created(
    cfg: &Config,
    repo_root: &Path,
    worktree: &Path,
    branch: &str,
    branch_created: bool,
) -> Result<(), String> {
    let workspace = thegn_core::repo::repo_slug(repo_root);
    let db = Db::open().ok();
    let (success, message) = if worktree.exists() {
        destroy_one(
            cfg,
            repo_root,
            worktree,
            branch,
            &workspace,
            false,
            branch_created,
            HookExecutionMode::Force,
            db.as_ref(),
        )
    } else {
        // `git worktree add -b` can create the branch before failing to create
        // the checkout. There is no directory for `destroy_one` to remove in
        // that case, but the speculative branch and worktree metadata still
        // need the same force-cleanup treatment.
        let mut failures = Vec::new();
        if !thegn_core::util::git_ok(repo_root, &["worktree", "prune"]) {
            failures.push("could not prune failed worktree metadata".to_string());
        }
        if branch_created
            && !branch.is_empty()
            && thegn_core::worktree::branch_exists(repo_root, branch)
            && !thegn_core::util::git_ok(repo_root, &["branch", "-D", branch])
        {
            failures.push(format!("could not delete speculative branch {branch}"));
        }
        let post = run_event_with_db(
            cfg,
            repo_root,
            worktree,
            branch,
            &workspace,
            HookEvent::PostDestroy,
            HookExecutionMode::Force,
            db.as_ref(),
        );
        if post.results.iter().any(|result| !result.succeeded()) {
            failures.push(post.message());
        }
        if failures.is_empty() {
            (true, "removed".to_string())
        } else {
            (false, failures.join("; "))
        }
    };
    if success {
        if message != "removed" {
            thegn_core::msg::warn(&format!("wizard rollback: {message}"));
        }
        Ok(())
    } else {
        Err(message)
    }
}

/// Preserve a create failure while reporting any failure from the shared
/// force-cleanup transaction. This keeps the primary diagnostic actionable
/// without allowing a partial `git worktree add` to leak state.
pub fn create_failure_with_rollback(
    primary: impl Into<String>,
    cfg: &Config,
    repo_root: &Path,
    worktree: &Path,
    branch: &str,
) -> String {
    let primary = primary.into();
    match rollback_remove(cfg, repo_root, worktree, branch) {
        Ok(()) => primary,
        Err(cleanup) => format!("{primary}; rollback failed: {cleanup}"),
    }
}

pub fn create_failure_with_add_state(
    primary: impl Into<String>,
    cfg: &Config,
    repo_root: &Path,
    worktree: &Path,
    branch: &str,
    branch_created: bool,
) -> String {
    let primary = primary.into();
    match rollback_remove_with_branch_created(cfg, repo_root, worktree, branch, branch_created) {
        Ok(()) => primary,
        Err(cleanup) => format!("{primary}; rollback failed: {cleanup}"),
    }
}

type SessionKey = (String, String);

#[derive(Default)]
struct SessionRuntime {
    latches: HashSet<SessionKey>,
    ending: HashMap<SessionKey, Vec<Arc<SessionEndInFlight>>>,
}

#[derive(Default)]
struct SessionEndInFlight {
    done: Mutex<bool>,
    completed: Condvar,
}

impl SessionEndInFlight {
    fn finish(&self) {
        *self.done.lock().expect("session end mutex poisoned") = true;
        self.completed.notify_all();
    }

    fn wait(&self) {
        let mut done = self.done.lock().expect("session end mutex poisoned");
        while !*done {
            done = self
                .completed
                .wait(done)
                .expect("session end mutex poisoned");
        }
    }
}

struct SessionEndGuard {
    key: SessionKey,
    in_flight: Arc<SessionEndInFlight>,
}

impl Drop for SessionEndGuard {
    fn drop(&mut self) {
        // Publish completion before removing the discoverable token. A destroy
        // that observed the token can always wait safely; a later destroy knows
        // all work in this event is already finished.
        self.in_flight.finish();
        let mut runtime = session_runtime()
            .lock()
            .expect("session runtime mutex poisoned");
        if let Some(events) = runtime.ending.get_mut(&self.key) {
            events.retain(|event| !Arc::ptr_eq(event, &self.in_flight));
            if events.is_empty() {
                runtime.ending.remove(&self.key);
            }
        }
    }
}

static SESSION_RUNTIME: OnceLock<Mutex<SessionRuntime>> = OnceLock::new();
static DESTROY_CLAIMS: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();

fn session_runtime() -> &'static Mutex<SessionRuntime> {
    SESSION_RUNTIME.get_or_init(|| Mutex::new(SessionRuntime::default()))
}

fn destroy_claims() -> &'static Mutex<HashSet<PathBuf>> {
    DESTROY_CLAIMS.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Atomically claim one physical worktree path for teardown. The same claim is
/// shared by sidebar and workspace deletion so overlapping requests cannot run
/// destroy hooks or git removal twice.
pub(crate) fn try_claim_destroy_path(path: &Path) -> bool {
    destroy_claims()
        .lock()
        .expect("destroy claim mutex poisoned")
        .insert(path.to_path_buf())
}

fn release_destroy_path(path: &Path) {
    destroy_claims()
        .lock()
        .expect("destroy claim mutex poisoned")
        .remove(path);
}

/// Schedule `session_start` once for the current host session and worktree.
/// The latch is process-local and intentionally not persisted in SQLite.
pub fn session_start_once(
    cfg: &Config,
    worktree: &Path,
    waker: Option<termwiz::terminal::TerminalWaker>,
) -> std::io::Result<bool> {
    if worktree.as_os_str().is_empty() {
        return Ok(false);
    }
    let key = session_key(worktree);
    if !session_runtime()
        .lock()
        .expect("session runtime mutex poisoned")
        .latches
        .insert(key.clone())
    {
        return Ok(false);
    }
    let spawned = spawn_session_event(
        cfg.clone(),
        worktree.to_path_buf(),
        HookEvent::SessionStart,
        waker,
        None,
    );
    finish_session_start_spawn(&key, spawned)
}

fn finish_session_start_spawn(
    key: &SessionKey,
    spawned: std::io::Result<()>,
) -> std::io::Result<bool> {
    match spawned {
        Ok(()) => Ok(true),
        Err(error) => {
            session_runtime()
                .lock()
                .expect("session runtime mutex poisoned")
                .latches
                .remove(key);
            Err(error)
        }
    }
}

/// Release a claimed `session_start` when the pane it was preparing could not
/// be spawned. The next retry must be allowed to run the hook again.
pub fn release_session_start(worktree: &Path) {
    let key = session_key(worktree);
    session_runtime()
        .lock()
        .expect("session runtime mutex poisoned")
        .latches
        .remove(&key);
}

/// Schedule `session_end` once for the current host session and worktree.
pub fn session_end_once(
    cfg: &Config,
    worktree: &Path,
    waker: Option<termwiz::terminal::TerminalWaker>,
) -> std::io::Result<bool> {
    let key = session_key(worktree);
    let Some(guard) = claim_session_end(&key) else {
        return Ok(false);
    };
    spawn_session_event(
        cfg.clone(),
        worktree.to_path_buf(),
        HookEvent::SessionEnd,
        waker,
        Some(guard),
    )?;
    Ok(true)
}

fn session_key(worktree: &Path) -> SessionKey {
    (
        worktree.to_string_lossy().into_owned(),
        std::process::id().to_string(),
    )
}

fn claim_session_end(key: &SessionKey) -> Option<SessionEndGuard> {
    let mut runtime = session_runtime()
        .lock()
        .expect("session runtime mutex poisoned");
    if !runtime.latches.remove(key) {
        return None;
    }
    let in_flight = Arc::new(SessionEndInFlight::default());
    runtime
        .ending
        .entry(key.clone())
        .or_default()
        .push(Arc::clone(&in_flight));
    Some(SessionEndGuard {
        key: key.clone(),
        in_flight,
    })
}

fn end_session_before_destroy(
    cfg: &Config,
    repo_root: &Path,
    worktree: &Path,
    branch: &str,
    workspace: &str,
    db: Option<&Db>,
) {
    let key = session_key(worktree);
    let (claimed, in_flight) = {
        let mut runtime = session_runtime()
            .lock()
            .expect("session runtime mutex poisoned");
        let claimed = runtime.latches.remove(&key);
        let in_flight = runtime.ending.get(&key).cloned().unwrap_or_default();
        (claimed, in_flight)
    };
    // Destruction already runs on a lifecycle worker. Waiting here serializes
    // cwd ownership without ever blocking the compositor loop.
    for event in in_flight {
        event.wait();
    }
    if !claimed {
        return;
    }
    let report = run_event_with_db(
        cfg,
        repo_root,
        worktree,
        branch,
        workspace,
        HookEvent::SessionEnd,
        HookExecutionMode::Unattended,
        db,
    );
    if report.results.iter().any(|result| !result.succeeded()) {
        thegn_core::msg::warn(&format!("session_end: {}", report.message()));
    }
}

/// End a worktree session when the pane that just exited was its last live
/// process. The exited pane is still present in the session tree until the
/// drain reconciles it, so callers pass its id explicitly.
pub fn session_end_after_pane_exit(
    cfg: &Config,
    session: &crate::session::Session,
    panes: &crate::panes::Panes,
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
                    .any(|id| id != exited_pane && panes.table.contains_key(&id))
            });
            if !other_live && let Err(error) = session_end_once(cfg, Path::new(&group.path), waker)
            {
                thegn_core::msg::warn(&format!(
                    "session_end lifecycle worker could not start: {error}"
                ));
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
    completion: Option<SessionEndGuard>,
) -> std::io::Result<()> {
    // `spawn_event` performs trust/config resolution on its worker and derives
    // the real repo root from this worktree path.
    spawn_event_with_completion(
        cfg,
        worktree.clone(),
        worktree,
        String::new(),
        String::new(),
        event,
        HookExecutionMode::Unattended,
        waker,
        completion,
    )
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

fn report_log_failure(context: &HookContext, result: &crate::hook_run::HookRunResult) {
    let Some(state) = NOTIFY_STATE.get() else {
        return;
    };
    let Ok(db) = Db::open() else {
        return;
    };
    let message = format!("{}: {}", context.event.as_str(), result.summary());
    let (decision, _) = crate::notify::record(
        &db,
        state,
        "hook_log_failed",
        context.event.as_str(),
        &message,
        &context.worktree,
    );
    state.emit_sound(&decision);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[expect(clippy::disallowed_methods)]
    fn git(dir: &Path, args: &[&str]) {
        let output = thegn_core::util::git_cmd(dir)
            .args(args)
            .output()
            .expect("git should start");
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }

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

    #[test]
    fn duplicate_destroy_requests_share_one_path_claim() {
        let path = std::env::temp_dir().join(format!(
            "tg-lifecycle-destroy-claim-{}-{}",
            std::process::id(),
            thegn_core::util::now()
        ));
        assert!(try_claim_destroy_path(&path));
        assert!(!try_claim_destroy_path(&path));
        release_destroy_path(&path);
        assert!(try_claim_destroy_path(&path));
        release_destroy_path(&path);
    }

    #[test]
    fn failed_pane_spawn_releases_session_start_claim_for_retry() {
        let worktree = std::env::temp_dir().join(format!(
            "tg-lifecycle-session-retry-{}-{}",
            std::process::id(),
            thegn_core::util::now()
        ));
        assert!(session_start_once(&Config::default(), &worktree, None).unwrap());
        release_session_start(&worktree);
        assert!(session_start_once(&Config::default(), &worktree, None).unwrap());
        release_session_start(&worktree);
    }

    #[test]
    fn closing_a_worktree_releases_the_session_latch_for_reopen() {
        let worktree = std::env::temp_dir().join(format!(
            "tg-lifecycle-session-close-{}-{}",
            std::process::id(),
            thegn_core::util::now()
        ));
        assert!(session_start_once(&Config::default(), &worktree, None).unwrap());
        assert!(session_end_once(&Config::default(), &worktree, None).unwrap());
        assert!(session_start_once(&Config::default(), &worktree, None).unwrap());
        release_session_start(&worktree);
    }

    #[test]
    fn lifecycle_worker_spawn_failures_release_claims_and_surface_errors() {
        let worktree = std::env::temp_dir().join(format!(
            "tg-lifecycle-spawn-failure-{}-{}",
            std::process::id(),
            thegn_core::util::now()
        ));
        let key = session_key(&worktree);
        session_runtime()
            .lock()
            .expect("session runtime mutex poisoned")
            .latches
            .insert(key.clone());
        let error =
            finish_session_start_spawn(&key, Err(std::io::Error::other("injected thread failure")))
                .expect_err("the session-start scheduling failure must propagate");
        assert!(error.to_string().contains("injected thread failure"));
        assert!(
            !session_runtime()
                .lock()
                .expect("session runtime mutex poisoned")
                .latches
                .contains(&key)
        );

        assert!(try_claim_destroy_path(&worktree));
        let error = release_destroy_claim_on_spawn_failure(
            &worktree,
            Err(std::io::Error::other("injected destroy failure")),
        )
        .expect_err("the destroy scheduling failure must propagate");
        assert!(error.to_string().contains("injected destroy failure"));
        assert!(try_claim_destroy_path(&worktree));
        release_destroy_path(&worktree);

        session_runtime()
            .lock()
            .expect("session runtime mutex poisoned")
            .latches
            .insert(key.clone());
        let guard = claim_session_end(&key).expect("session end should be claimed");
        drop(guard); // Builder::spawn drops its closure and guard on failure.
        assert!(
            !session_runtime()
                .lock()
                .expect("session runtime mutex poisoned")
                .ending
                .contains_key(&key)
        );

        let report = spawn_failure_report(
            HookEvent::PostCreate,
            std::io::Error::other("injected post-create failure"),
        );
        assert!(report.blocked());
        assert!(report.message().contains("injected post-create failure"));
    }

    #[test]
    fn destroy_waits_for_detached_session_end_before_removing_worktree() {
        let base = tempfile::tempdir().unwrap();
        let state_home = base.path().join("state");
        let _env =
            crate::testenv::EnvVarGuard::set(&[("XDG_STATE_HOME", state_home.to_str().unwrap())]);
        let root = base.path().join("repo");
        let worktree = base.path().join("feature");
        std::fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["config", "user.name", "test"]);
        git(&root, &["config", "user.email", "test@example.com"]);
        git(&root, &["config", "commit.gpgsign", "false"]);
        std::fs::write(root.join("file"), "base\n").unwrap();
        git(&root, &["add", "file"]);
        git(&root, &["commit", "-q", "-m", "base"]);
        git(
            &root,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feature",
                worktree.to_str().unwrap(),
            ],
        );

        let started = base.path().join("session-end-started");
        let release = base.path().join("release-session-end");
        let mut cfg = Config::default();
        cfg.hooks.session_end = vec![thegn_core::hooks::HookEntry::Spec(
            thegn_core::hooks::HookEntrySpec {
                command: format!(
                    "printf started > {}; while [ ! -e {} ]; do sleep 0.01; done",
                    started.display(),
                    release.display()
                ),
                wait: Some(false),
                timeout_secs: Some(5),
                on_failure: Some(thegn_core::hooks::HookFailure::Warn),
            },
        )];
        let key = session_key(&worktree);
        session_runtime()
            .lock()
            .expect("session runtime mutex poisoned")
            .latches
            .insert(key);
        assert!(session_end_once(&cfg, &worktree, None).unwrap());

        let hook_deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !started.exists() && std::time::Instant::now() < hook_deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(started.exists(), "session_end hook did not start");

        let (tx, rx) = std::sync::mpsc::channel();
        let destroy_cfg = cfg.clone();
        let destroy_root = root.clone();
        let destroy_worktree = worktree.clone();
        std::thread::spawn(move || {
            let result = destroy_one(
                &destroy_cfg,
                &destroy_root,
                &destroy_worktree,
                "feature",
                "repo",
                false,
                false,
                HookExecutionMode::Force,
                None,
            );
            tx.send(result).unwrap();
        });
        assert!(
            rx.recv_timeout(std::time::Duration::from_millis(150))
                .is_err(),
            "destroy completed while session_end still owned the cwd"
        );
        assert!(worktree.exists());

        std::fs::write(&release, "release\n").unwrap();
        let (success, message) = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("destroy should resume after session_end completion");
        assert!(success, "destroy failed: {message}");
        assert!(!worktree.exists());
    }

    #[test]
    fn waiting_post_create_failure_blocks_create_pipeline() {
        let state_home = std::env::temp_dir().join(format!(
            "tg-lifecycle-post-wait-state-{}-{}",
            std::process::id(),
            thegn_core::util::now()
        ));
        let _env =
            crate::testenv::EnvVarGuard::set(&[("XDG_STATE_HOME", state_home.to_str().unwrap())]);
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.hooks.post_create = vec![thegn_core::hooks::HookEntry::Spec(
            thegn_core::hooks::HookEntrySpec {
                command: "exit 9".into(),
                wait: Some(true),
                timeout_secs: Some(2),
                on_failure: Some(thegn_core::hooks::HookFailure::Block),
            },
        )];

        let report = schedule_post_create(
            &cfg,
            dir.path(),
            dir.path(),
            "feature",
            "workspace",
            None,
            None,
        )
        .expect_err("a blocking wait hook must stop creation");
        assert!(report.blocked());
        assert!(report.message().contains("hook failed"));
    }

    #[test]
    fn rollback_removes_branch_when_add_left_no_checkout() {
        let state_home = std::env::temp_dir().join(format!(
            "tg-lifecycle-rollback-state-{}",
            std::process::id()
        ));
        let _env =
            crate::testenv::EnvVarGuard::set(&[("XDG_STATE_HOME", state_home.to_str().unwrap())]);
        let root = std::env::temp_dir().join(format!(
            "tg-lifecycle-rollback-repo-{}-{}",
            std::process::id(),
            thegn_core::util::now()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["config", "user.name", "test"]);
        git(&root, &["config", "user.email", "test@example.com"]);
        git(&root, &["config", "commit.gpgsign", "false"]);
        std::fs::write(root.join("file"), "base\n").unwrap();
        git(&root, &["add", "file"]);
        git(&root, &["commit", "-q", "-m", "base"]);
        git(&root, &["branch", "partial", "main"]);

        let missing = root.join("missing");
        let result = rollback_remove(&Config::default(), &root, &missing, "partial");

        assert!(result.is_ok(), "rollback failed: {result:?}");
        assert!(!thegn_core::worktree::branch_exists(&root, "partial"));
    }

    #[test]
    fn add_failure_preserves_primary_error_after_force_cleanup() {
        let state_home =
            std::env::temp_dir().join(format!("tg-lifecycle-add-state-{}", std::process::id()));
        let _env =
            crate::testenv::EnvVarGuard::set(&[("XDG_STATE_HOME", state_home.to_str().unwrap())]);
        let root = std::env::temp_dir().join(format!(
            "tg-lifecycle-add-repo-{}-{}",
            std::process::id(),
            thegn_core::util::now()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["config", "user.name", "test"]);
        git(&root, &["config", "user.email", "test@example.com"]);
        git(&root, &["config", "commit.gpgsign", "false"]);
        std::fs::write(root.join("file"), "base\n").unwrap();
        git(&root, &["add", "file"]);
        git(&root, &["commit", "-q", "-m", "base"]);
        git(&root, &["branch", "partial", "main"]);

        let error = create_failure_with_add_state(
            "git worktree add failed: partial checkout",
            &Config::default(),
            &root,
            &root.join("missing"),
            "partial",
            true,
        );

        assert!(error.contains("git worktree add failed: partial checkout"));
        assert!(!thegn_core::worktree::branch_exists(&root, "partial"));

        git(&root, &["branch", "preexisting", "main"]);
        let result = rollback_remove_with_branch_created(
            &Config::default(),
            &root,
            &root.join("missing-preexisting"),
            "preexisting",
            false,
        );
        assert!(result.is_ok(), "rollback failed: {result:?}");
        assert!(thegn_core::worktree::branch_exists(&root, "preexisting"));
    }

    fn completion_test_session() -> crate::session::Session {
        crate::session::Session {
            id: "repo".into(),
            worktrees: vec![crate::session::WorktreeGroup::new(
                "repo/feature",
                crate::session::GroupKind::Branch,
                "/tmp/repo-feature",
            )],
            active: 0,
        }
    }

    fn apply_test_completion(
        completion: LifecycleCompletion,
    ) -> (crate::session::Session, crate::chrome::FrameModel) {
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let mut panes = crate::panes::Panes::new(tx);
        let mut session = completion_test_session();
        let mut model = crate::chrome::FrameModel::default();
        let mut sb = crate::run::SidebarState::default();
        assert!(apply_completions_from(
            vec![completion],
            &mut session,
            &mut panes,
            &mut model,
            &mut sb,
        ));
        (session, model)
    }

    #[test]
    fn successful_completion_variants_reconcile_without_loop_io() {
        let (session, model) = apply_test_completion(LifecycleCompletion::WorktreeDelete {
            group_name: "repo/feature".into(),
            path: "/tmp/repo-feature".into(),
            success: true,
            message: String::new(),
        });
        assert!(session.worktrees.is_empty());
        assert_eq!(model.status, "Deleted worktree /tmp/repo-feature");

        let (session, _) = apply_test_completion(LifecycleCompletion::WorkspaceDelete {
            repo_path: "/tmp/repo".into(),
            slug: "repo".into(),
            path: "/tmp/repo-feature".into(),
            success: true,
            message: String::new(),
        });
        assert!(session.worktrees.is_empty());

        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let mut panes = crate::panes::Panes::new(tx);
        let mut session = completion_test_session();
        let mut model = crate::chrome::FrameModel {
            sidebar_workspaces: vec![("repo".into(), "repo".into(), "repo".into(), "repo".into())],
            ..Default::default()
        };
        let mut sb = crate::run::SidebarState::default();
        assert!(apply_completions_from(
            vec![LifecycleCompletion::WorkspaceDeleteFinished {
                repo_path: "repo".into(),
                slug: "repo".into(),
                failed_paths: Vec::new(),
            }],
            &mut session,
            &mut panes,
            &mut model,
            &mut sb,
        ));
        assert!(session.worktrees.is_empty());
        assert!(model.sidebar_workspaces.is_empty());
    }
}
