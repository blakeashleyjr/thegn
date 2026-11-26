//! Merge-queue → sidebar-folder lifecycle policy (the I/O half).
//!
//! Executes the pure decision from [`thegn_core::merge_lifecycle`]: file a
//! worktree into a sidebar folder as its branch moves through the queue, or —
//! after a clean land — remove the worktree (and optionally delete its branch).
//! Best-effort throughout: a folder/DB hiccup must never fail a merge (the DB is
//! a cache; git refs are the source of truth).
//!
//! `apply` runs OFF the event loop — the CLI subcommands (`merge add/drain/land`,
//! `integrate`) and the fold's `spawn_blocking` are its callers. When a removed
//! worktree is open as a live tab in a running instance, the in-app fold-result
//! handler reaps the orphaned tab via the typed, off-loop reconciliation result
//! — nothing in `apply` touches the session.

use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use thegn_core::config::MergeQueueConfig;
use thegn_core::db::Db;
use thegn_core::merge_lifecycle::{LifecycleAction, LifecycleEvent, decide};
use thegn_core::store::{WorkspaceStore, WorktreeAuxStore};

#[path = "merge_cleanup.rs"]
mod cleanup;
#[cfg(test)]
pub(crate) use cleanup::TestIsolation;
pub(crate) use cleanup::common as repository_identity;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CleanupRefusal {
    Database,
    Identity,
    ClaimBusy,
    MergeClaimed,
    Unsafe,
    Remote,
    Resources,
    Removal,
}

impl CleanupRefusal {
    pub(crate) fn id(self) -> &'static str {
        match self {
            Self::Database => "database",
            Self::Identity => "identity",
            Self::ClaimBusy => "claim_busy",
            Self::MergeClaimed => "merge_claimed",
            Self::Unsafe => "unsafe",
            Self::Remote => "remote",
            Self::Resources => "resources",
            Self::Removal => "removal",
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Database => "database unavailable",
            Self::Identity => "queue identity changed",
            Self::ClaimBusy => "cleanup already in progress",
            Self::MergeClaimed => "merge claim held",
            Self::Unsafe => "unsafe or unverified checkout",
            Self::Remote => "remote worktree",
            Self::Resources => "attached runtime resources",
            Self::Removal => "removal failed",
        }
    }
}

fn live_merge_claim(view: Option<&thegn_core::store::MergeClaimView>, now_ms: i64) -> bool {
    view.is_some_and(|view| {
        view.phase != "unclaimed" && view.lease_until_ms.is_none_or(|until| until >= now_ms)
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CleanupOutcome {
    Removed {
        branch_deleted: bool,
        queue_removed: bool,
        discarded_build_state: bool,
        discarded_tool_state: bool,
        branch_held: bool,
        bookkeeping_errors: Vec<String>,
    },
    KeptDirty,
    KeptChanged,
    Refused {
        class: CleanupRefusal,
        reason: String,
    },
}

/// One group identity identified by the off-loop vanished-tab probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VanishedGroup {
    pub path: String,
    pub epoch: u64,
}

/// Groups identified by the off-loop vanished-tab probe. The compositor applies
/// this result without re-checking disk or SQLite.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VanishedTabs {
    pub groups: Vec<VanishedGroup>,
}

impl VanishedTabs {
    /// Keep only sampled group generations that still exist in the live session.
    pub(crate) fn still_current(&self, session: &crate::session::Session) -> Vec<VanishedGroup> {
        self.groups
            .iter()
            .filter(|sampled| {
                session
                    .worktrees
                    .iter()
                    .any(|current| current.path == sampled.path && current.epoch == sampled.epoch)
            })
            .cloned()
            .collect()
    }
}

static RECONCILIATION_IN_FLIGHT: AtomicBool = AtomicBool::new(false);
/// Groups (path, epoch) this process already decided to keep; re-probed only
/// when the epoch changes, or when a git-kept group reaches its reprobe deadline.
static SETTLED_GROUPS: OnceLock<Mutex<SettledGroups>> = OnceLock::new();
/// Group path -> (epoch it was settled at, when a git-kept group is probed again).
type SettledGroups = HashMap<String, (u64, Option<std::time::Instant>)>;
const GIT_KEPT_REPROBE: std::time::Duration = std::time::Duration::from_secs(60);

struct ReconciliationCompletionGuard;

impl Drop for ReconciliationCompletionGuard {
    fn drop(&mut self) {
        RECONCILIATION_IN_FLIGHT.store(false, Ordering::Release);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathState {
    Checkout,
    Remnant,
    Missing,
}

/// Branch groups: `.git` present proves a linked checkout; a directory without
/// it is what an interrupted `git worktree remove` leaves.
fn branch_path_state(path: &str) -> PathState {
    if Path::new(path).join(".git").symlink_metadata().is_ok() {
        PathState::Checkout
    } else if Path::new(path).is_dir() {
        PathState::Remnant
    } else {
        PathState::Missing
    }
}

/// Home / terminal groups and workspace roots: a plain directory workspace has
/// no `.git`.
fn dir_path_state(path: &str) -> PathState {
    if Path::new(path).is_dir() {
        PathState::Checkout
    } else {
        PathState::Missing
    }
}

struct SampleInputs {
    groups: Vec<VanishedGroup>,
    branch_paths: HashSet<String>,
    session_root: Option<String>,
}

fn sample_inputs(session: &crate::session::Session) -> SampleInputs {
    let groups = session
        .worktrees
        .iter()
        .filter(|g| !g.path.is_empty())
        .map(|g| VanishedGroup {
            path: g.path.clone(),
            epoch: g.epoch,
        })
        .collect();
    let branch_paths = session
        .worktrees
        .iter()
        .filter(|g| g.kind == crate::session::GroupKind::Branch && !g.path.is_empty())
        .map(|g| g.path.clone())
        .collect();
    let session_root = Path::new(&session.id)
        .is_absolute()
        .then(|| session.id.clone());
    SampleInputs {
        groups,
        branch_paths,
        session_root,
    }
}

fn sample_vanished_tabs(
    groups: &[VanishedGroup],
    path_state: impl Fn(&str) -> PathState,
    load_registry: impl FnOnce() -> anyhow::Result<RegistrySample>,
    mut git_unlisted: impl FnMut(&str, Option<&str>) -> bool,
    settled_groups: &Mutex<HashMap<String, (u64, Option<std::time::Instant>)>>,
    now: std::time::Instant,
) -> anyhow::Result<Vec<String>> {
    let current = groups
        .iter()
        .map(|group| (group.path.as_str(), group.epoch))
        .collect::<std::collections::HashSet<_>>();
    // RECONCILIATION_IN_FLIGHT admits only one sampler, so holding this memo
    // lock across the registry load cannot block another concurrent sampler.
    let mut settled = settled_groups
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    settled.retain(|path, (epoch, _)| current.contains(&(path.as_str(), *epoch)));
    let missing: Vec<(&VanishedGroup, PathState)> = groups
        .iter()
        .filter_map(|group| {
            if matches!(
                settled.get(&group.path),
                Some((epoch, deadline))
                    if *epoch == group.epoch && deadline.is_none_or(|deadline| now < deadline)
            ) {
                return None;
            }
            if !Path::new(&group.path).is_absolute() {
                return None;
            }
            let state = path_state(&group.path);
            (state != PathState::Checkout).then_some((group, state))
        })
        .collect();
    if missing.is_empty() {
        return Ok(Vec::new());
    }
    let registry = load_registry()?;
    let RegistrySample {
        rows,
        workspaces,
        remote,
    } = registry;
    let mut gone = Vec::new();
    let mut unregistered_reaped = 0;
    let mut unregistered_kept = 0;
    let mut first_unregistered_path = None;
    for (group, state) in missing {
        let path = &group.path;
        let is_workspace_root = workspaces
            .iter()
            .any(|workspace| workspace.repo_path == *path);
        match rows.iter().find(|row| row.worktree == *path) {
            Some(row) if !remote.contains(&row.worktree) => match state {
                PathState::Missing => gone.push(path.clone()),
                PathState::Remnant => {
                    if git_unlisted(path, Some(row.repo_root.as_str())) {
                        gone.push(path.clone());
                    } else {
                        settled.insert(path.clone(), (group.epoch, Some(now + GIT_KEPT_REPROBE)));
                    }
                }
                PathState::Checkout => {}
            },
            Some(_) => {
                tracing::debug!(path, "skipping vanished-tab probe for remote worktree");
                settled.insert(path.clone(), (group.epoch, None));
            }
            None if is_workspace_root => {
                if state == PathState::Remnant {
                    settled.insert(path.clone(), (group.epoch, None));
                } else {
                    gone.push(path.clone());
                }
            }
            None => {
                let is_unlisted = git_unlisted(path, None);
                if is_unlisted {
                    unregistered_reaped += 1;
                } else {
                    unregistered_kept += 1;
                }
                if first_unregistered_path.is_none() {
                    first_unregistered_path = Some(path.as_str());
                }
                if is_unlisted {
                    gone.push(path.clone());
                } else {
                    settled.insert(path.clone(), (group.epoch, Some(now + GIT_KEPT_REPROBE)));
                }
            }
        }
    }
    if let Some(first_path) = first_unregistered_path {
        tracing::info!(
            reaped = unregistered_reaped,
            kept = unregistered_kept,
            first_path,
            "reconciled unregistered vanished-tab paths using git"
        );
    }
    Ok(gone)
}

/// THE-2472: the one "is this registry row remote" answer the automatic
/// reapers share. Remote = a stored non-local `location`, OR an effective env
/// (the row's own, else the repo's ambient one) whose placement is ssh, k8s or
/// provider. ssh and k8s rows never store a `location`.
pub(crate) fn registry_row_is_remote(
    db: &Db,
    cfg: &thegn_core::config::Config,
    row: &thegn_core::models::WorktreeRow,
    ambient_cache: &mut HashMap<String, String>,
) -> bool {
    let stored_local = row.location.is_empty() || row.location == "local";
    !stored_local
        || crate::hydrate::row_is_remote_effective(
            db,
            cfg,
            "",
            row.env_name.as_deref(),
            &row.repo_root,
            ambient_cache,
        )
}

/// What the sampler reads from the state DB on the reap branch.
struct RegistrySample {
    rows: Vec<thegn_core::models::WorktreeRow>,
    workspaces: Vec<thegn_core::models::WorkspaceRow>,
    /// Paths of rows `registry_row_is_remote` calls remote.
    remote: HashSet<String>,
}

impl RegistrySample {
    #[cfg(test)]
    fn from_location(
        rows: Vec<thegn_core::models::WorktreeRow>,
        workspaces: Vec<thegn_core::models::WorkspaceRow>,
    ) -> Self {
        let remote = rows
            .iter()
            .filter(|row| !(row.location.is_empty() || row.location == "local"))
            .map(|row| row.worktree.clone())
            .collect();
        Self {
            rows,
            workspaces,
            remote,
        }
    }
}

/// Read the registry and classify every row once (one ambient-env cache).
fn registry_sample(db: &Db, cfg: &thegn_core::config::Config) -> anyhow::Result<RegistrySample> {
    let rows = db.worktrees()?;
    let workspaces = db.workspaces()?;
    let mut ambient_cache = HashMap::new();
    let remote = rows
        .iter()
        .filter(|row| registry_row_is_remote(db, cfg, row, &mut ambient_cache))
        .map(|row| row.worktree.clone())
        .collect();
    Ok(RegistrySample {
        rows,
        workspaces,
        remote,
    })
}

fn git_confirms_unlisted(
    branch_paths: &HashSet<String>,
    session_root: Option<&str>,
    path: &str,
    row_root: Option<&str>,
    cache: &mut HashMap<String, Option<Vec<String>>>,
) -> bool {
    branch_paths.contains(path)
        && !crate::hydrate::row_is_git_listed(
            session_root.or(row_root).unwrap_or_default(),
            path,
            cache,
        )
}

/// Apply the lifecycle policy for one worktree branch in response to `event`.
/// A no-op unless `[merge_queue] organize_folders` is on. Never fails.
pub(crate) fn apply(
    cfg: &MergeQueueConfig,
    db: &Db,
    repo_root: &Path,
    worktree: &str,
    branch: &str,
    event: LifecycleEvent,
) {
    for note in apply_reported(cfg, db, repo_root, worktree, branch, event) {
        note.emit();
    }
}

/// An actual committed outcome, unlike an `UpToDate` or generic lifecycle event.
pub(crate) fn apply_landed(
    cfg: &MergeQueueConfig,
    db: &Db,
    repo_root: &Path,
    worktree: &str,
    branch: &str,
    commit: &str,
) {
    let notes = apply_landed_reported(cfg, db, repo_root, worktree, branch, commit);
    for note in &notes {
        note.emit();
    }
    // A retained worktree must leave a durable trace: this path runs from the
    // CLI/daemon, where the terminal warning above is easily lost.
    if let Some(text) = hold_notice(&notes) {
        tracing::info!(
            target: "thegn::git_audit",
            worktree = %worktree,
            commit = %commit,
            class = "hold",
            "landed cleanup hold recorded"
        );
        let source_ref = format!("merge-cleanup:{commit}:{worktree}");
        if let Err(error) = crate::automation_events::emit_once(
            db,
            thegn_core::notification::NotificationKind::QueueHeld.as_str(),
            &source_ref,
            &text,
            worktree,
        ) {
            tracing::debug!(target: "thegn::merge", %error, "could not record landed-cleanup hold");
        }
    }
}

/// The joined `Warn` note text (one inbox notice per land), `None` without any.
fn hold_notice(notes: &[LifecycleNote]) -> Option<String> {
    let warns: Vec<&str> = notes
        .iter()
        .filter_map(|note| match note {
            LifecycleNote::Warn(text) => Some(text.as_str()),
            LifecycleNote::Info(_) => None,
        })
        .collect();
    if warns.is_empty() {
        None
    } else {
        Some(warns.join("; "))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LifecycleNote {
    Info(String),
    Warn(String),
}

impl LifecycleNote {
    pub(crate) fn text(&self) -> &str {
        match self {
            Self::Info(text) | Self::Warn(text) => text,
        }
    }

    pub(crate) fn emit(&self) {
        match self {
            Self::Info(text) => thegn_core::msg::info(text),
            Self::Warn(text) => thegn_core::msg::warn(text),
        }
    }
}

pub(crate) fn apply_reported(
    cfg: &MergeQueueConfig,
    db: &Db,
    repo_root: &Path,
    worktree: &str,
    branch: &str,
    event: LifecycleEvent,
) -> Vec<LifecycleNote> {
    apply_inner(cfg, db, repo_root, worktree, branch, event, None)
}

pub(crate) fn apply_landed_reported(
    cfg: &MergeQueueConfig,
    db: &Db,
    repo_root: &Path,
    worktree: &str,
    branch: &str,
    commit: &str,
) -> Vec<LifecycleNote> {
    apply_inner(
        cfg,
        db,
        repo_root,
        worktree,
        branch,
        LifecycleEvent::Landed,
        Some(commit),
    )
}

/// The main checkout is a fixed anchor; aliases are compared by canonical path.
fn home_checkout(worktree: &str, repo_root: &Path) -> std::result::Result<bool, String> {
    let worktree_path = Path::new(worktree);
    if worktree_path == repo_root {
        return Ok(true);
    }
    let worktree_canonical = std::fs::canonicalize(worktree_path);
    let root_canonical = std::fs::canonicalize(repo_root);
    match (worktree_canonical, root_canonical) {
        (Ok(worktree), Ok(root)) => Ok(worktree == root),
        (Err(error), _) | (_, Err(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(false)
        }
        (Err(error), _) | (_, Err(error)) => Err(format!(
            "merge lifecycle: worktree identity unreadable for {}: {error}",
            crate::merge_sweep::safe_display(worktree)
        )),
    }
}

fn apply_inner(
    cfg: &MergeQueueConfig,
    db: &Db,
    repo_root: &Path,
    worktree: &str,
    branch: &str,
    event: LifecycleEvent,
    commit: Option<&str>,
) -> Vec<LifecycleNote> {
    let mut notes = Vec::new();
    // The home / main checkout is a fixed anchor — never file or remove it.
    match home_checkout(worktree, repo_root) {
        Ok(true) => return notes,
        Ok(false) => {}
        Err(reason) => {
            notes.push(LifecycleNote::Warn(reason));
            return notes;
        }
    }
    let result = match decide(cfg, event) {
        LifecycleAction::Noop => Ok(()),
        LifecycleAction::FileInto(folder) => file_into(db, repo_root, worktree, branch, &folder),
        LifecycleAction::RemoveWorktree { delete_branch } => {
            let Some(commit) = commit else {
                notes.push(LifecycleNote::Warn(
                    "merge cleanup: retained worktree; no committed outcome supplied".into(),
                ));
                return notes;
            };
            let target = crate::integrate::resolve_target(cfg, repo_root);
            let outcome = remove_landed(
                db,
                repo_root,
                worktree,
                branch,
                &target,
                Some(commit),
                delete_branch,
            );
            match outcome {
                CleanupOutcome::Removed {
                    discarded_build_state,
                    discarded_tool_state,
                    branch_held,
                    bookkeeping_errors,
                    ..
                } => {
                    if discarded_build_state {
                        notes.push(LifecycleNote::Info(format!(
                            "merge cleanup: swept {} (discarded build state)",
                            crate::merge_sweep::safe_display(branch)
                        )));
                    }
                    if discarded_tool_state {
                        notes.push(LifecycleNote::Info(format!(
                            "merge cleanup: swept {} (discarded seeded tool state)",
                            crate::merge_sweep::safe_display(branch)
                        )));
                    }
                    if branch_held {
                        notes.push(LifecycleNote::Info(format!(
                            "merge cleanup: removed {}; branch kept for explicit cleanup",
                            crate::merge_sweep::safe_display(branch)
                        )));
                    }
                    for error in bookkeeping_errors {
                        notes.push(LifecycleNote::Warn(crate::merge_sweep::safe_display(
                            &error,
                        )));
                    }
                }
                CleanupOutcome::KeptDirty => notes.push(LifecycleNote::Warn(format!(
                    "merge cleanup: kept {} — edited since landing",
                    crate::merge_sweep::safe_display(branch)
                ))),
                CleanupOutcome::KeptChanged => notes.push(LifecycleNote::Warn(format!(
                    "merge cleanup: kept {} — changed during cleanup",
                    crate::merge_sweep::safe_display(branch)
                ))),
                CleanupOutcome::Refused { class, reason } => {
                    notes.push(LifecycleNote::Warn(format!(
                        "merge cleanup: kept {} — {}: {}",
                        crate::merge_sweep::safe_display(branch),
                        class.label(),
                        crate::merge_sweep::safe_display(&reason)
                    )))
                }
            }
            Ok(())
        }
        LifecycleAction::Unfile => unfile(cfg, db, repo_root, worktree),
    };
    if let Err(error) = result {
        notes.push(LifecycleNote::Warn(format!(
            "merge lifecycle bookkeeping unavailable: {error}"
        )));
    }
    notes
}

/// Checked execution for the one-shot `thegn land` path. Unlike the queue's
/// best-effort lifecycle, the caller receives every database failure and must
/// either fail before landing or explicitly report degraded post-land state.
pub(crate) fn apply_landed_in_place_checked(
    cfg: &MergeQueueConfig,
    db: &Db,
    repo_root: &Path,
    worktree: &str,
    branch: &str,
) -> Result<()> {
    match home_checkout(worktree, repo_root) {
        Ok(true) => return Ok(()),
        Ok(false) => {}
        Err(reason) => anyhow::bail!(reason),
    }
    match decide(cfg, LifecycleEvent::LandedInPlace) {
        LifecycleAction::Noop => Ok(()),
        LifecycleAction::FileInto(folder) => file_into(db, repo_root, worktree, branch, &folder),
        LifecycleAction::Unfile => unfile(cfg, db, repo_root, worktree),
        LifecycleAction::RemoveWorktree { .. } => {
            anyhow::bail!("land-in-place lifecycle unexpectedly requested worktree removal")
        }
    }
}

/// Un-file a worktree from its lifecycle folder (Merging / Needs attention /
/// Merged) back to the ungrouped repo root — the cleanup a plain dequeue
/// (`merge rm` / `merge clear`) needs, since it neither lands nor fails. Guarded
/// so it only clears membership of a folder the lifecycle itself manages: if the
/// user has since hand-filed the worktree into a folder of their own, it is left
/// alone. Best-effort — sidebar bookkeeping must never fail a queue mutation.
fn unfile(cfg: &MergeQueueConfig, db: &Db, repo_root: &Path, worktree: &str) -> Result<()> {
    // The worktree's current folder, if any. No row / no folder ⇒ nothing to do.
    let Some(fid) = db
        .worktrees()?
        .into_iter()
        .find_map(|w| (w.worktree == worktree).then_some(w.folder_id).flatten())
    else {
        return Ok(());
    };
    // Resolve that folder's name (keyed by the worktree's own workspace string,
    // the same resolution `file_into` uses) and only un-file when it is one the
    // lifecycle manages.
    let recorded = db.repo_root_for(worktree)?;
    let repo_path = workspace_repo_path(db, repo_root, recorded.as_deref())?;
    let name = db
        .folders_for_workspace(&repo_path)?
        .into_iter()
        .find(|f| f.folder_id == fid)
        .map(|f| f.name);
    let is_lifecycle_folder = |n: &str| {
        let n = n.trim();
        !n.is_empty()
            && (n == cfg.queued_folder.trim()
                || n == cfg.failed_folder.trim()
                || n == cfg.merged_folder.trim())
    };
    if name.as_deref().map(is_lifecycle_folder).unwrap_or(false) {
        db.set_worktree_folder(worktree, None)?;
    }
    Ok(())
}

/// The exact `workspaces.repo_path` string the sidebar keys folders by, for the
/// repo whose main checkout is `repo_root` (and whose worktree row records
/// `recorded`). A folder MUST be created under this string or the sidebar —
/// which matches `folders.repo_path == workspaces.repo_path` byte-for-byte —
/// won't render it. Prefer an existing `workspaces` row matched exactly (on the
/// recorded path or `repo_root`) then by canonicalized path; fall back to the
/// recorded worktree path, then `repo_root`. Runs off-loop, so the `canonicalize`
/// stat is fine.
fn workspace_repo_path(db: &Db, repo_root: &Path, recorded: Option<&str>) -> Result<String> {
    let rows = db.workspaces()?;
    let want = std::fs::canonicalize(repo_root).ok(); // best-effort: optional input: a vanished path just fails the match; the caller falls back to the raw root
    if let Some(w) = rows.iter().find(|w| {
        Some(w.repo_path.as_str()) == recorded
            || Path::new(&w.repo_path) == repo_root
            || (want.is_some() && std::fs::canonicalize(&w.repo_path).ok() == want)
    }) {
        return Ok(w.repo_path.clone());
    }
    Ok(recorded
        .map(str::to_owned)
        .unwrap_or_else(|| repo_root.to_string_lossy().into_owned()))
}

/// File `worktree` into the named sidebar folder (find-or-create), scoped to the
/// worktree's own workspace so it lands under the right repo in the tree.
fn file_into(db: &Db, repo_root: &Path, worktree: &str, branch: &str, folder: &str) -> Result<()> {
    // `repo_root_for` doubles as the "is this worktree in the DB cache?" probe:
    // it reads the row's `repo_path`, so `None` means there is no row.
    let recorded = db.repo_root_for(worktree)?;
    // File under the SAME `repo_path` string the sidebar keys folders by. The
    // sidebar renders a folder only when `folders.repo_path == workspaces.repo_path`
    // byte-for-byte (see `hydrate::workspace_list` + `sidebar::build_rows`), so a
    // folder created under a divergent string (a worktree row registered by an
    // external tool, a trailing slash, a symlinked path) yields no header and the
    // filed worktree is orphaned. Resolve to the workspace's own string.
    let repo_path = workspace_repo_path(db, repo_root, recorded.as_deref())?;
    let (workspace_name, workspace_kind) =
        crate::workspace_identity::workspace_identity(&repo_path, Some("repo"));
    let fid = db.ensure_folder(&repo_path, folder, &workspace_name, workspace_kind)?;
    if recorded.is_some() {
        // Row already cached: a narrow update that leaves every other column intact.
        db.set_worktree_folder(worktree, Some(fid))?;
    } else {
        // No cache row — the worktree was created via git / the `wt` CLI, not the
        // in-app wizard/provision path that calls `put_worktree`. A bare
        // `set_worktree_folder` would `UPDATE … WHERE worktree=?` and match zero
        // rows, so the filing would silently vanish. Register the row instead,
        // with the canonical `{slug}/{branch}` tab name the sidebar joins live
        // tabs to (`db_by_tab`), so the folder actually shows.
        let base = {
            let slug = thegn_core::util::slugify(&thegn_core::repo::repo_name_from_path(
                Path::new(&repo_path),
            ));
            if slug.is_empty() {
                "repo".to_string()
            } else {
                slug
            }
        };
        let slug = db.slug_for_repo(&repo_path, &base)?;
        let tab = thegn_core::repo::branch_tab(&slug, branch);
        db.put_worktree(&tab, &repo_path, worktree, branch, None, Some(fid))?;
    }
    Ok(())
}

/// Remove a landed worktree, retaining requested branch cleanup as an explicit
/// hold. Caller-provided names must match the registered checkout and current
/// Git objects before lifecycle side effects or physical removal.
pub(crate) fn remove_landed(
    db: &Db,
    repo_root: &Path,
    worktree: &str,
    branch: &str,
    target: &str,
    landed: Option<&str>,
    delete_branch: bool,
) -> CleanupOutcome {
    remove_landed_unaudited(
        db,
        repo_root,
        worktree,
        branch,
        target,
        landed,
        delete_branch,
    )
}

/// One `thegn::git_audit` line per cleanup attempt: worktree, commit and the
/// outcome class only (no further paths, no free-form refusal text).
fn audit_cleanup_outcome(worktree: &str, landed: Option<&str>, outcome: &CleanupOutcome) {
    let class = match outcome {
        CleanupOutcome::Removed {
            branch_held: true, ..
        } => "removed_branch_held",
        CleanupOutcome::Removed { .. } => "removed",
        CleanupOutcome::KeptDirty => "kept_dirty",
        CleanupOutcome::KeptChanged => "kept_changed",
        CleanupOutcome::Refused { class, .. } => class.id(),
    };
    tracing::info!(
        target: "thegn::git_audit",
        worktree = %worktree,
        commit = %landed.unwrap_or(""),
        class,
        "landed cleanup attempt"
    );
}

fn remove_landed_unaudited(
    db: &Db,
    repo_root: &Path,
    worktree: &str,
    branch: &str,
    target: &str,
    landed: Option<&str>,
    delete_branch: bool,
) -> CleanupOutcome {
    let selected = match db.list_merge_queue() {
        Ok(rows) => rows.into_iter().find(|row| row.worktree == worktree),
        Err(error) => {
            let refused = CleanupOutcome::Refused {
                class: CleanupRefusal::Database,
                reason: format!("queue identity unavailable: {error}"),
            };
            audit_cleanup_outcome(worktree, landed, &refused);
            return refused;
        }
    };
    let Some(selected) = selected.filter(|row| {
        row.status == "landed"
            && row.branch == branch
            && row.target_branch == target
            && landed.is_some()
            && row.result_oid.as_deref() == landed
    }) else {
        let refused = CleanupOutcome::Refused {
            class: CleanupRefusal::Identity,
            reason: "committed outcome does not match a current landed queue row".into(),
        };
        audit_cleanup_outcome(worktree, landed, &refused);
        return refused;
    };
    // A held row already completed its worktree collection (the sweep treats
    // it the same way); only the branch is deliberately retained. Replaying
    // the land is a no-op, not a fresh refusal against the vanished path.
    if thegn_core::merge_sweep::CleanupHold::from_detail(selected.error_detail.as_deref()).is_some()
    {
        let replay = CleanupOutcome::Removed {
            branch_deleted: false,
            queue_removed: false,
            discarded_build_state: false,
            discarded_tool_state: false,
            branch_held: true,
            bookkeeping_errors: Vec::new(),
        };
        audit_cleanup_outcome(worktree, landed, &replay);
        return replay;
    }
    let cfg = crate::config_startup::cleanup_config();
    remove_landed_with_config(
        &cfg,
        db,
        repo_root,
        worktree,
        branch,
        target,
        landed,
        &selected,
        delete_branch,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn remove_landed_with_config(
    cfg: &thegn_core::config::Config,
    db: &Db,
    repo_root: &Path,
    worktree: &str,
    branch: &str,
    target: &str,
    landed: Option<&str>,
    expected_queue: &thegn_core::db::MergeQueueRow,
    delete_branch: bool,
) -> CleanupOutcome {
    let outcome = remove_landed_with_config_unaudited(
        cfg,
        db,
        repo_root,
        worktree,
        branch,
        target,
        landed,
        expected_queue,
        delete_branch,
    );
    audit_cleanup_outcome(worktree, landed, &outcome);
    outcome
}

/// The checkout is already gone: retire the proven landed queue row, keep the
/// branch, and report it as a removal that deleted nothing but the row.
fn retire_row_of_gone_checkout(db: &Db, worktree: &str, landed: Option<&str>) -> CleanupOutcome {
    let mut bookkeeping_errors = Vec::new();
    let queue_removed =
        match db.remove_merge_entry_as(worktree, "landed cleanup: checkout already gone") {
            Ok(_) => true,
            Err(error) => {
                bookkeeping_errors.push(format!("queue row retained for retry: {error}"));
                false
            }
        };
    tracing::info!(
        target: "thegn::git_audit",
        worktree = %worktree,
        commit = %landed.unwrap_or(""),
        class = "already_gone",
        queue_removed,
        "landed cleanup: checkout already gone"
    );
    CleanupOutcome::Removed {
        branch_deleted: false,
        queue_removed,
        discarded_build_state: false,
        discarded_tool_state: false,
        branch_held: false,
        bookkeeping_errors,
    }
}

#[allow(clippy::too_many_arguments)]
fn remove_landed_with_config_unaudited(
    cfg: &thegn_core::config::Config,
    db: &Db,
    repo_root: &Path,
    worktree: &str,
    branch: &str,
    target: &str,
    landed: Option<&str>,
    expected_queue: &thegn_core::db::MergeQueueRow,
    delete_branch: bool,
) -> CleanupOutcome {
    if expected_queue.status != "landed"
        || expected_queue.worktree != worktree
        || expected_queue.branch != branch
        || expected_queue.target_branch != target
        || landed.is_none_or(str::is_empty)
        || expected_queue.result_oid.as_deref() != landed
    {
        return CleanupOutcome::Refused {
            class: CleanupRefusal::Identity,
            reason: "selected landed queue identity is invalid".into(),
        };
    }
    let claim = match db.observe_merge_claim(worktree) {
        Ok(claim) => claim,
        Err(error) => {
            return CleanupOutcome::Refused {
                class: CleanupRefusal::Database,
                reason: format!("merge claim unavailable: {error}"),
            };
        }
    };
    let now = thegn_core::util::now_ms();
    if let Some(view) = claim
        .as_ref()
        .filter(|view| live_merge_claim(Some(*view), now))
    {
        return CleanupOutcome::Refused {
            class: CleanupRefusal::MergeClaimed,
            reason: format!(
                "a drain holds this row (claim generation {}, owner {}); cleanup retries after it is released",
                view.generation,
                crate::merge_sweep::safe_display(view.owner_hint.as_deref().unwrap_or("unnamed"))
            ),
        };
    }
    // Merge reclaim shares the same process as interactive deletion in the TUI.
    // Serialize it with sidebar/workspace destroy workers so hooks, provider
    // teardown, and git removal cannot run twice for one physical path.
    let Some(_destroy_claim) =
        crate::worktree_lifecycle::try_scoped_destroy_path(Path::new(worktree))
    else {
        return CleanupOutcome::Refused {
            class: CleanupRefusal::ClaimBusy,
            reason: "cleanup is already in progress".into(),
        };
    };
    // Crash recovery: git removed the checkout but the queue write never
    // happened. The row was already proven to be this landed commit above, so
    // retire it; the branch is never touched here. A checkout that still
    // exists, or is still registered (even if its directory is missing), keeps
    // the normal path, and an unreadable registry is "unknown", not "gone".
    // A row with no registry record is gone only when the repo's ambient env is
    // local (ssh and k8s rows store no location).
    let row_is_local = (expected_queue.location.is_empty() || expected_queue.location == "local")
        && match db.worktree_record(worktree) {
            Ok(None) => !crate::hydrate::row_is_remote_effective(
                db,
                cfg,
                "",
                None,
                &repo_root.to_string_lossy(),
                &mut HashMap::new(),
            ),
            Ok(Some(row)) => !registry_row_is_remote(db, cfg, &row, &mut HashMap::new()),
            Err(_) => false, // unknown is never "gone"
        };
    if row_is_local
        && !Path::new(worktree).exists()
        && matches!(
            cleanup::path_is_registered(repo_root, Path::new(worktree)),
            Ok(false)
        )
    {
        return retire_row_of_gone_checkout(db, worktree, landed);
    }
    // `worktree::remove` escalates to `git worktree remove --force`, which
    // discards uncommitted work. That is fine for an explicit `wt rm`, but this
    // path is automatic — it fires on a clean land, with no prompt — so a
    // worktree the user still has work in must be left alone. It keeps its
    // lifecycle folder and its branch; the next land (or a manual `wt rm`)
    // cleans it up once the work is committed or dropped.
    let verified = match cleanup::Verified::probe_with_config(
        repo_root, worktree, branch, target, landed, cfg,
    ) {
        Ok(verified) => verified,
        Err(cleanup::Refusal::Dirty) => return CleanupOutcome::KeptDirty,
        Err(error) => {
            return CleanupOutcome::Refused {
                class: CleanupRefusal::Unsafe,
                reason: error.to_string(),
            };
        }
    };
    match db.merge_enqueue_identity(worktree) {
        Ok(Some(recorded)) => {
            if let Err(reason) = crate::merge_driver::recorded_identity_drift(
                &recorded,
                Path::new(worktree),
                repo_root,
            ) {
                return CleanupOutcome::Refused {
                    class: CleanupRefusal::Identity,
                    reason: format!(
                        "worktree identity changed since the branch was queued ({reason}); kept"
                    ),
                };
            }
        }
        Ok(None) => {}
        Err(error) => {
            return CleanupOutcome::Refused {
                class: CleanupRefusal::Database,
                reason: format!("enqueue identity unavailable: {error}"),
            };
        }
    }
    let queue_before = match db.list_merge_queue() {
        Ok(rows) => rows.into_iter().find(|r| r.worktree == worktree),
        Err(error) => {
            return CleanupOutcome::Refused {
                class: CleanupRefusal::Database,
                reason: format!("queue identity unavailable: {error}"),
            };
        }
    };
    if queue_before.as_ref() != Some(expected_queue) {
        return CleanupOutcome::Refused {
            class: CleanupRefusal::Identity,
            reason: "selected landed queue entry changed or was revoked".into(),
        };
    }
    let cache_before = match db.worktree_record(worktree) {
        Ok(row) => row,
        Err(error) => {
            return CleanupOutcome::Refused {
                class: CleanupRefusal::Database,
                reason: format!("worktree cache identity unavailable: {error}"),
            };
        }
    };
    if queue_before.as_ref().is_some_and(|r| {
        r.branch != branch
            || r.target_branch != target
            || (!r.location.is_empty() && r.location != "local")
    }) || cache_before
        .as_ref()
        .is_some_and(|r| r.branch != branch || (!r.location.is_empty() && r.location != "local"))
    {
        return CleanupOutcome::Refused {
            class: CleanupRefusal::Remote,
            reason: "cache identity is remote or disagrees with Git".into(),
        };
    }
    if let Some(row) = &cache_before
        && let Err(error) = verified.verify_cached_repository(Path::new(&row.repo_root))
    {
        return CleanupOutcome::Refused {
            class: CleanupRefusal::Unsafe,
            reason: error.to_string(),
        };
    }
    let resources = match cleanup::LocalResources::settle(cfg, db, repo_root, worktree) {
        Ok(resources) => resources,
        Err(reason) => {
            return CleanupOutcome::Refused {
                class: CleanupRefusal::Resources,
                reason,
            };
        }
    };
    let discarded_build_state = verified.discarded_build_state();
    let discarded_tool_state = verified.discarded_tool_state();
    let changed_during_cleanup = std::cell::Cell::new(false);
    let dirty_during_cleanup = std::cell::Cell::new(false);
    // A worktree that becomes dirty or changes *during* cleanup is kept, exactly
    // as one that was already dirty at admission. Record which, so the sweep
    // reports it in its own category rather than as a generic refusal — a
    // `Refused` row reads as a fault, and this is the normal "you went back to
    // it" outcome.
    let note_refusal = |refusal: cleanup::Refusal| -> String {
        match &refusal {
            cleanup::Refusal::Changed => changed_during_cleanup.set(true),
            cleanup::Refusal::Dirty => dirty_during_cleanup.set(true),
            cleanup::Refusal::ManagedChanged(_) | cleanup::Refusal::UnrecognizedToolState(_) => {}
            cleanup::Refusal::Unsafe(_) => {}
        }
        refusal.to_string()
    };
    let revalidate_verified = || -> Result<(), String> {
        match verified.revalidate() {
            Ok(()) => Ok(()),
            Err(refusal) => Err(note_refusal(refusal)),
        }
    };
    let unchanged_queue = || -> Result<(), String> {
        resources.revalidate(db, repo_root, worktree)?;
        let current = db
            .list_merge_queue()
            .map_err(|e| e.to_string())?
            .into_iter()
            .find(|r| r.worktree == worktree);
        if current != queue_before {
            return Err("merge queue entry changed during cleanup".into());
        }
        let claim = db
            .observe_merge_claim(worktree)
            .map_err(|error| format!("merge claim unavailable: {error}"))?;
        if live_merge_claim(claim.as_ref(), thegn_core::util::now_ms()) {
            return Err("merge claim acquired during cleanup".into());
        }
        let cache_now = db.worktree_record(worktree).map_err(|e| e.to_string())?;
        if cache_now
            .as_ref()
            .map(|r| (&r.repo_root, &r.branch, &r.location, &r.env_name))
            != cache_before
                .as_ref()
                .map(|r| (&r.repo_root, &r.branch, &r.location, &r.env_name))
        {
            return Err("worktree cache identity changed during cleanup".into());
        }
        Ok(())
    };
    // Automatic reclaim is unattended: the shared transaction runs the hook,
    // runtime teardown, removal, and post-hook in order, but a repository-
    // authored failure can never wedge the queue.
    // Display label only: this is the `$THEGN_WORKSPACE` string handed to
    // user cleanup hooks, not an identity anything routes on. It deliberately
    // stays the path-derived basename slug (THE-516 considered resolving the
    // DB slug here and rejected it): `slug_for_repo` is a WRITE transaction,
    // so a transient SQLITE_BUSY from any of the several thegn processes on
    // this box would turn unattended landed-worktree reclaim into a refusal,
    // and the hook's variable would silently change value (repo → repo-2).
    let workspace = thegn_core::util::slugify(&thegn_core::repo::repo_name_from_path(repo_root));
    let (removed, message) = crate::worktree_lifecycle::destroy_one_checked(
        cfg,
        repo_root,
        Path::new(worktree),
        branch,
        &workspace,
        false,
        delete_branch,
        thegn_core::hooks::HookExecutionMode::Unattended,
        Some(db),
        &|| {
            unchanged_queue()?;
            revalidate_verified()
        },
        Some(&|| {
            unchanged_queue()?;
            match verified.remove_checked(&unchanged_queue) {
                Ok(()) => Ok(()),
                Err(refusal) => Err(note_refusal(refusal)),
            }
        }),
        Some(&|| resources.revalidate(db, repo_root, worktree)),
    );
    if !removed {
        if changed_during_cleanup.get() {
            return CleanupOutcome::KeptChanged;
        }
        if dirty_during_cleanup.get() {
            return CleanupOutcome::KeptDirty;
        }
        return CleanupOutcome::Refused {
            class: CleanupRefusal::Removal,
            reason: message,
        };
    }
    if let Err(error) = verified.verify_removed() {
        return CleanupOutcome::Refused {
            class: CleanupRefusal::Removal,
            reason: error.to_string(),
        };
    }
    let mut bookkeeping_errors = Vec::new();
    // THE-596: no-deref/OID checks alone cannot atomically prove direct ref
    // type. Automatic cleanup never mutates a source/victim/target ref.
    let branch_deleted = false;
    // No retry record is discarded for a refused physical cleanup. After real
    // removal, report partial bookkeeping failure rather than claiming tidy DB.
    let queue_removed = match db.transaction(|db| {
        unchanged_queue().map_err(anyhow::Error::msg)?;
        let cache_now = db.worktree_record(worktree)?;
        if cache_now
            .as_ref()
            .map(|r| (&r.repo_root, &r.branch, &r.location))
            != cache_before
                .as_ref()
                .map(|r| (&r.repo_root, &r.branch, &r.location))
        {
            anyhow::bail!("worktree cache identity changed during cleanup");
        }
        if delete_branch && !db.hold_merge_cleanup(expected_queue)? {
            anyhow::bail!("cleanup hold observation changed; retained for manual reconciliation");
        }
        db.del_worktree(worktree)?;
        db.delete_tab_groups_for_worktree_all_sessions(worktree)?;
        if !delete_branch {
            db.remove_merge_entry_as(worktree, "thegn close")?;
            return Ok(true);
        }
        Ok(false)
    }) {
        Ok(removed) => removed,
        Err(error) => {
            bookkeeping_errors.push(format!(
                "{branch}: cleanup bookkeeping retained for retry: {error}"
            ));
            false
        }
    };
    if delete_branch && !queue_removed {
        match db.list_merge_queue() {
            Ok(rows) => {
                if let Some(row) = rows.into_iter().find(|row| row.worktree == worktree)
                    && thegn_core::merge_sweep::CleanupHold::from_detail(
                        row.error_detail.as_deref(),
                    )
                    .is_none()
                    && let Err(error) = db.hold_merge_cleanup(&row)
                {
                    bookkeeping_errors.push(format!(
                        "{branch}: retained branch cleanup hold could not be persisted: {error}"
                    ));
                }
            }
            Err(error) => bookkeeping_errors.push(format!(
                "{branch}: retained branch cleanup hold could not be checked: {error}"
            )),
        }
    }
    if message != "removed" {
        bookkeeping_errors.push(message);
    }
    CleanupOutcome::Removed {
        branch_deleted,
        queue_removed,
        discarded_build_state,
        discarded_tool_state,
        branch_held: delete_branch,
        bookkeeping_errors,
    }
}

/// Probe local session groups for out-of-band removals on a worker. SQLite and
/// filesystem access, including the cache/session writes, stay off the loop.
/// The next refresh carries the typed result back to the compositor.
pub(crate) fn spawn_reconcile_removed_tabs(
    session: &crate::session::Session,
    waker: Option<termwiz::terminal::TerminalWaker>,
) {
    if RECONCILIATION_IN_FLIGHT.swap(true, Ordering::AcqRel) {
        return;
    }
    let inputs = sample_inputs(session);
    let admission = crate::sched::spawn_bg(move || {
        // Install before any worker operation so every exit releases ownership.
        let _completion = ReconciliationCompletionGuard;
        let mut sampled_db = None;
        let settled = SETTLED_GROUPS.get_or_init(|| Mutex::new(HashMap::new()));
        let SampleInputs {
            groups,
            branch_paths,
            session_root,
        } = inputs;
        let mut git_cache = HashMap::new();
        let sample = sample_vanished_tabs(
            &groups,
            |path| {
                if branch_paths.contains(path) {
                    branch_path_state(path)
                } else {
                    dir_path_state(path)
                }
            },
            || {
                let admitted = crate::config_startup::display().ok_or_else(|| {
                    anyhow::anyhow!("no admitted config yet; remote rows cannot be told apart, so nothing is reaped")
                })?;
                let db = Db::open()?;
                let sample = registry_sample(&db, admitted.config())?;
                sampled_db = Some(db);
                Ok(sample)
            },
            |path, row_root| {
                git_confirms_unlisted(
                    &branch_paths,
                    session_root.as_deref(),
                    path,
                    row_root,
                    &mut git_cache,
                )
            },
            settled,
            std::time::Instant::now(),
        );
        let gone = match sample {
            Ok(gone) => gone,
            Err(error) => {
                tracing::warn!(%error, "vanished-tab reconciliation sample unavailable; leaving tabs untouched");
                return;
            }
        };
        if let Some(db) = sampled_db {
            for path in &gone {
                if let Err(error) = db.del_worktree(path) {
                    tracing::warn!(%error, path, "could not clear vanished worktree cache row");
                }
            }
        }
        let vanished: Vec<VanishedGroup> = groups
            .into_iter()
            .filter(|group| gone.iter().any(|path| path == &group.path))
            .collect();
        if !vanished.is_empty() {
            crate::worktree_lifecycle::send_refresh(
                crate::hydrate::RefreshKind::VanishedTabs(Box::new(VanishedTabs {
                    groups: vanished,
                })),
                waker,
            );
        }
    });
    if admission != crate::sched::Admission::Admitted {
        RECONCILIATION_IN_FLIGHT.store(false, Ordering::Release);
        tracing::debug!(
            ?admission,
            "vanished-tab reconciliation deferred by background scheduler"
        );
    }
}

/// Apply an off-loop vanished-tab result. This is intentionally pure with
/// respect to the filesystem and SQLite: it only removes pane/session objects,
/// queues the resulting layout cache write, and restores focus by group name —
/// or, when the active group itself vanished, lands per `landing` (planned by
/// the caller against the pre-removal sidebar rows).
pub(crate) fn apply_vanished_tabs(
    session: &mut crate::session::Session,
    panes: &mut crate::panes::Panes,
    groups: &[VanishedGroup],
    landing: &crate::handlers::worktree_delete::Landing,
) -> bool {
    let gone = vanished_group_indices(session, groups);
    if gone.is_empty() {
        return false;
    }
    // `delete_groups` moves `session.active` onto each group it removes
    // (`switch_to` + `close_active_group`), so a background reap would silently
    // steal focus onto whatever group shifts into the freed index — often an
    // unmaterialized tab, which then paints the idle splash on the next full
    // frame. Re-pin the group the user was on (by name; it survives unless it
    // was itself reaped), mirroring the user-initiated delete
    // (`handlers::worktree_delete`).
    let prior = session.active_group().map(|g| g.name.clone());
    for gi in gone.into_iter().rev() {
        for id in crate::run::prune_vanished_group(session, gi) {
            panes.table.remove(&id);
        }
    }
    match prior.and_then(|name| session.worktrees.iter().position(|g| g.name == name)) {
        Some(idx) => session.switch_to(idx),
        // The active group was reaped: its visual neighbour, not the home row
        // `prune_vanished_group` parked focus on.
        None => landing.land(session),
    }
    let snapshot = session.layout_snapshot(&session.id, crate::run::now_secs());
    crate::db_task::persist(move |db| {
        let _ = crate::session::Session::write_layout(db, &snapshot); // best-effort: cache write: the DB is a cache
    });
    true
}

/// Select only groups named by the worker's completion. Keeping this as a
/// pure seam makes it impossible for loop-side reconciliation to silently
/// reintroduce a disk or SQLite probe.
fn vanished_group_indices(
    session: &crate::session::Session,
    groups: &[VanishedGroup],
) -> Vec<usize> {
    session
        .worktrees
        .iter()
        .enumerate()
        .filter(|(_, g)| {
            groups
                .iter()
                .any(|group| group.path == g.path && group.epoch == g.epoch)
        })
        .map(|(i, _)| i)
        .collect()
}

#[cfg(test)]
#[path = "merge_lifecycle_cleanup_tests.rs"]
mod cleanup_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use thegn_core::config::{Config, EnvConfig, OnLanded, PlacementMode};
    use thegn_core::store::NotificationStore;
    use thegn_core::util;

    #[test]
    fn failed_registry_sample_emits_no_removals_and_releases_worker_ownership() {
        RECONCILIATION_IN_FLIGHT.store(true, Ordering::Release);
        let _completion = ReconciliationCompletionGuard;
        let groups = vec![VanishedGroup {
            path: "/definitely/missing/worktree".to_owned(),
            epoch: 1,
        }];
        let remote = Mutex::new(HashMap::new());
        let sample = sample_vanished_tabs(
            &groups,
            |_| PathState::Missing,
            || anyhow::bail!("injected database open failure"),
            |_, _| false,
            &remote,
            std::time::Instant::now(),
        );
        assert!(sample.is_err());
        assert!(
            sample
                .unwrap_err()
                .to_string()
                .contains("injected database open failure")
        );
        drop(_completion);
        assert!(!RECONCILIATION_IN_FLIGHT.load(Ordering::Acquire));

        RECONCILIATION_IN_FLIGHT.store(true, Ordering::Release);
        let panic = std::panic::catch_unwind(|| {
            let _completion = ReconciliationCompletionGuard;
            panic!("injected worker panic");
        });
        assert!(panic.is_err());
        assert!(!RECONCILIATION_IN_FLIGHT.load(Ordering::Acquire));
    }

    #[test]
    fn opaque_unregistered_path_is_skipped_during_vanished_tab_sample() {
        let groups = vec![VanishedGroup {
            path: "provider:workspace/branch".into(),
            epoch: 1,
        }];
        let remote = Mutex::new(HashMap::new());
        let loads = std::cell::Cell::new(0);
        for _ in 0..2 {
            let sample = sample_vanished_tabs(
                &groups,
                |_| PathState::Missing,
                || {
                    loads.set(loads.get() + 1);
                    Ok(RegistrySample::from_location(Vec::new(), Vec::new()))
                },
                |_, _| false,
                &remote,
                std::time::Instant::now(),
            );
            assert_eq!(sample.unwrap(), Vec::<String>::new());
        }
        assert_eq!(loads.get(), 0);
    }

    #[test]
    fn registered_remote_path_is_not_reported_as_gone() {
        let db = Db::open_memory().unwrap();
        db.put_workspace("/repo", "repo", "git").unwrap();
        db.put_worktree("feature", "/repo", "/remote/feature", "feature", None, None)
            .unwrap();
        db.set_worktree_location("/remote/feature", r#"{"host":"build"}"#)
            .unwrap();
        let groups = vec![VanishedGroup {
            path: "/remote/feature".to_owned(),
            epoch: 1,
        }];
        let remote = Mutex::new(HashMap::new());
        let gone = sample_vanished_tabs(
            &groups,
            |_| PathState::Missing,
            || {
                Ok(RegistrySample::from_location(
                    db.worktrees()?,
                    db.workspaces()?,
                ))
            },
            |_, _| false,
            &remote,
            std::time::Instant::now(),
        )
        .unwrap();
        assert!(gone.is_empty());
    }

    #[test]
    fn ssh_row_with_missing_path_is_kept_and_settled() {
        let db = Db::open_memory().unwrap();
        db.put_workspace("/repo", "repo", "git").unwrap();
        db.put_worktree("repo/a", "/repo", "/gone/a", "a", None, None)
            .unwrap();
        db.set_worktree_env("/gone/a", "build").unwrap();
        let mut cfg = Config::default();
        cfg.env.insert(
            "build".into(),
            EnvConfig {
                placement: PlacementMode::Ssh,
                ..Default::default()
            },
        );
        let sample = registry_sample(&db, &cfg).unwrap();
        let settled = Mutex::new(HashMap::new());
        let gone = sample_vanished_tabs(
            &[VanishedGroup {
                path: "/gone/a".into(),
                epoch: 1,
            }],
            |_| PathState::Missing,
            || Ok(sample),
            |_, _| panic!("remote rows do not need git confirmation"),
            &settled,
            std::time::Instant::now(),
        )
        .unwrap();
        assert!(gone.is_empty());
        assert!(matches!(
            settled.lock().unwrap().get("/gone/a"),
            Some((1, None))
        ));
    }

    #[test]
    fn k8s_row_inheriting_the_ambient_env_is_kept() {
        let db = Db::open_memory().unwrap();
        db.put_workspace("/repo", "repo", "git").unwrap();
        db.put_worktree("repo/a", "/repo", "/gone/a", "a", None, None)
            .unwrap();
        db.set_workspace_env("/repo", "kube").unwrap();
        let mut cfg = Config::default();
        cfg.env.insert(
            "kube".into(),
            EnvConfig {
                placement: PlacementMode::K8s,
                ..Default::default()
            },
        );
        let sample = registry_sample(&db, &cfg).unwrap();
        let settled = Mutex::new(HashMap::new());
        let gone = sample_vanished_tabs(
            &[VanishedGroup {
                path: "/gone/a".into(),
                epoch: 1,
            }],
            |_| PathState::Missing,
            || Ok(sample),
            |_, _| panic!("remote rows do not need git confirmation"),
            &settled,
            std::time::Instant::now(),
        )
        .unwrap();
        assert!(gone.is_empty());
    }

    #[test]
    fn local_row_with_missing_path_is_still_gone() {
        let db = Db::open_memory().unwrap();
        db.put_workspace("/repo", "repo", "git").unwrap();
        db.put_worktree("repo/a", "/repo", "/gone/a", "a", None, None)
            .unwrap();
        db.set_workspace_env("/repo", "host").unwrap();
        let mut cfg = Config::default();
        cfg.env.insert(
            "host".into(),
            EnvConfig {
                placement: PlacementMode::Local,
                ..Default::default()
            },
        );
        let sample = registry_sample(&db, &cfg).unwrap();
        let gone = sample_vanished_tabs(
            &[VanishedGroup {
                path: "/gone/a".into(),
                epoch: 1,
            }],
            |_| PathState::Missing,
            || Ok(sample),
            |_, _| panic!("missing registered rows do not need git confirmation"),
            &Mutex::new(HashMap::new()),
            std::time::Instant::now(),
        )
        .unwrap();
        assert_eq!(gone, ["/gone/a"]);
    }

    #[test]
    fn registry_row_is_remote_reads_location_and_env() {
        let db = Db::open_memory().unwrap();
        for (root, name) in [
            ("/location-repo", "location"),
            ("/local-repo", "local"),
            ("/pinned-repo", "pinned"),
            ("/ambient-repo", "ambient"),
            ("/local-ambient-repo", "local-ambient"),
        ] {
            db.put_workspace(root, name, "git").unwrap();
        }
        db.put_worktree(
            "location",
            "/location-repo",
            "/location",
            "location",
            Some("remote"),
            None,
        )
        .unwrap();
        db.put_worktree(
            "local",
            "/local-repo",
            "/local",
            "local",
            Some("local"),
            None,
        )
        .unwrap();
        db.put_worktree("pinned", "/pinned-repo", "/pinned", "pinned", None, None)
            .unwrap();
        db.set_worktree_env("/pinned", "build").unwrap();
        db.put_worktree(
            "ambient",
            "/ambient-repo",
            "/ambient",
            "ambient",
            None,
            None,
        )
        .unwrap();
        db.set_workspace_env("/ambient-repo", "kube").unwrap();
        db.put_worktree(
            "local-ambient",
            "/local-ambient-repo",
            "/local-ambient",
            "local-ambient",
            None,
            None,
        )
        .unwrap();
        db.set_workspace_env("/local-ambient-repo", "host").unwrap();

        let mut cfg = Config::default();
        for (name, placement) in [
            ("build", PlacementMode::Ssh),
            ("kube", PlacementMode::K8s),
            ("host", PlacementMode::Local),
        ] {
            cfg.env.insert(
                name.into(),
                EnvConfig {
                    placement,
                    ..Default::default()
                },
            );
        }
        let rows = db.worktrees().unwrap();
        let mut ambient_cache = HashMap::new();
        for (path, expected) in [
            ("/location", true),
            ("/local", false),
            ("/pinned", true),
            ("/ambient", true),
            ("/local-ambient", false),
        ] {
            let row = rows.iter().find(|row| row.worktree == path).unwrap();
            assert_eq!(
                registry_row_is_remote(&db, &cfg, row, &mut ambient_cache),
                expected,
                "{path}"
            );
        }
    }

    #[test]
    fn all_present_dirs_never_load_the_registry() {
        let groups = ["/one", "/two", "/three"]
            .into_iter()
            .enumerate()
            .map(|(epoch, path)| VanishedGroup {
                path: path.to_owned(),
                epoch: epoch as u64,
            })
            .collect::<Vec<_>>();
        let remote = Mutex::new(HashMap::new());
        let loads = std::cell::Cell::new(0);
        let sample = sample_vanished_tabs(
            &groups,
            |_| PathState::Checkout,
            || {
                loads.set(loads.get() + 1);
                Ok(RegistrySample::from_location(Vec::new(), Vec::new()))
            },
            |_, _| false,
            &remote,
            std::time::Instant::now(),
        )
        .unwrap();
        assert!(sample.is_empty());
        assert_eq!(loads.get(), 0);
    }

    #[test]
    fn remote_group_is_not_reloaded_every_sample() {
        let db = Db::open_memory().unwrap();
        db.put_workspace("/repo", "repo", "git").unwrap();
        db.put_worktree("feature", "/repo", "/remote/feature", "feature", None, None)
            .unwrap();
        db.set_worktree_location("/remote/feature", r#"{"host":"build"}"#)
            .unwrap();
        let remote = Mutex::new(HashMap::new());
        let loads = std::cell::Cell::new(0);
        let sample_group = |epoch| VanishedGroup {
            path: "/remote/feature".to_owned(),
            epoch,
        };
        let first_group = [sample_group(1)];
        let first = sample_vanished_tabs(
            &first_group,
            |_| PathState::Missing,
            || {
                loads.set(loads.get() + 1);
                Ok(RegistrySample::from_location(
                    db.worktrees()?,
                    db.workspaces()?,
                ))
            },
            |_, _| false,
            &remote,
            std::time::Instant::now(),
        )
        .unwrap();
        assert!(first.is_empty());
        assert_eq!(loads.get(), 1);

        let same_group = [sample_group(1)];
        let second = sample_vanished_tabs(
            &same_group,
            |_| PathState::Missing,
            || {
                loads.set(loads.get() + 1);
                Ok(RegistrySample::from_location(
                    db.worktrees()?,
                    db.workspaces()?,
                ))
            },
            |_, _| false,
            &remote,
            std::time::Instant::now(),
        )
        .unwrap();
        assert!(second.is_empty());
        assert_eq!(loads.get(), 1);

        let new_epoch = [sample_group(2)];
        let third = sample_vanished_tabs(
            &new_epoch,
            |_| PathState::Missing,
            || {
                loads.set(loads.get() + 1);
                Ok(RegistrySample::from_location(
                    db.worktrees()?,
                    db.workspaces()?,
                ))
            },
            |_, _| false,
            &remote,
            std::time::Instant::now(),
        )
        .unwrap();
        assert!(third.is_empty());
        assert_eq!(loads.get(), 2);
    }

    #[test]
    fn home_root_and_gone_registered_local_worktree_are_sampled() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("repo");
        std::fs::create_dir(&home).unwrap();
        let gone_worktree = dir.path().join("gone-worktree");
        let db = Db::open_memory().unwrap();
        db.put_workspace(home.to_str().unwrap(), "repo", "git")
            .unwrap();
        db.put_worktree(
            "gone",
            home.to_str().unwrap(),
            gone_worktree.to_str().unwrap(),
            "gone",
            None,
            None,
        )
        .unwrap();
        let paths = [
            home.to_string_lossy().into_owned(),
            gone_worktree.to_string_lossy().into_owned(),
        ];

        let groups = paths
            .iter()
            .enumerate()
            .map(|(epoch, path)| VanishedGroup {
                path: path.clone(),
                epoch: epoch as u64,
            })
            .collect::<Vec<_>>();
        let remote = Mutex::new(HashMap::new());
        let gone = sample_vanished_tabs(
            &groups,
            dir_path_state,
            || {
                Ok(RegistrySample::from_location(
                    db.worktrees()?,
                    db.workspaces()?,
                ))
            },
            |_, _| false,
            &remote,
            std::time::Instant::now(),
        )
        .unwrap();
        assert_eq!(gone, [gone_worktree.to_string_lossy().into_owned()]);
    }

    #[test]
    fn unregistered_path_does_not_block_gone_registered_local_worktree() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("repo");
        std::fs::create_dir(&home).unwrap();
        let gone_worktree = dir.path().join("gone-worktree");
        let db = Db::open_memory().unwrap();
        db.put_workspace(home.to_str().unwrap(), "repo", "git")
            .unwrap();
        db.put_worktree(
            "gone",
            home.to_str().unwrap(),
            gone_worktree.to_str().unwrap(),
            "gone",
            None,
            None,
        )
        .unwrap();
        let paths = [
            "opaque:workspace/branch".into(),
            gone_worktree.to_string_lossy().into_owned(),
        ];

        let groups = paths
            .iter()
            .enumerate()
            .map(|(epoch, path)| VanishedGroup {
                path: path.clone(),
                epoch: epoch as u64,
            })
            .collect::<Vec<_>>();
        let remote = Mutex::new(HashMap::new());
        let gone = sample_vanished_tabs(
            &groups,
            dir_path_state,
            || {
                Ok(RegistrySample::from_location(
                    db.worktrees()?,
                    db.workspaces()?,
                ))
            },
            |_, _| false,
            &remote,
            std::time::Instant::now(),
        )
        .unwrap();
        assert_eq!(gone, [gone_worktree.to_string_lossy().into_owned()]);
    }

    #[test]
    fn unregistered_group_is_gone_once_git_stops_listing_it() {
        let groups = [VanishedGroup {
            path: "/missing/feature".into(),
            epoch: 1,
        }];
        let settled = Mutex::new(HashMap::new());
        let gone = sample_vanished_tabs(
            &groups,
            |_| PathState::Missing,
            || Ok(RegistrySample::from_location(Vec::new(), Vec::new())),
            |_, _| true,
            &settled,
            std::time::Instant::now(),
        )
        .unwrap();
        assert_eq!(gone, ["/missing/feature"]);
    }

    #[test]
    fn unregistered_group_git_still_lists_is_settled_and_not_reprobed() {
        let groups = [VanishedGroup {
            path: "/listed/feature".into(),
            epoch: 1,
        }];
        let settled = Mutex::new(HashMap::new());
        let loads = std::cell::Cell::new(0);
        let git_calls = std::cell::Cell::new(0);
        let now = std::time::Instant::now();
        let sample = |groups: &[VanishedGroup]| {
            sample_vanished_tabs(
                groups,
                |_| PathState::Missing,
                || {
                    loads.set(loads.get() + 1);
                    Ok(RegistrySample::from_location(Vec::new(), Vec::new()))
                },
                |_, _| {
                    git_calls.set(git_calls.get() + 1);
                    false
                },
                &settled,
                now,
            )
        };
        assert!(sample(&groups).unwrap().is_empty());
        assert!(sample(&groups).unwrap().is_empty());
        assert_eq!(loads.get(), 1);
        assert_eq!(git_calls.get(), 1);

        let new_epoch = [VanishedGroup {
            path: "/listed/feature".into(),
            epoch: 2,
        }];
        assert!(sample(&new_epoch).unwrap().is_empty());
        assert_eq!(loads.get(), 2);
        assert_eq!(git_calls.get(), 2);
    }

    #[test]
    fn git_kept_group_is_reprobed_after_the_deadline() {
        let groups = [VanishedGroup {
            path: "/listed/feature".into(),
            epoch: 1,
        }];
        let settled = Mutex::new(HashMap::new());
        let loads = std::cell::Cell::new(0);
        let git_calls = std::cell::Cell::new(0);
        let now = std::time::Instant::now();
        let sample = |now| {
            sample_vanished_tabs(
                &groups,
                |_| PathState::Missing,
                || {
                    loads.set(loads.get() + 1);
                    Ok(RegistrySample::from_location(Vec::new(), Vec::new()))
                },
                |_, _| {
                    git_calls.set(git_calls.get() + 1);
                    git_calls.get() > 1
                },
                &settled,
                now,
            )
        };

        assert!(sample(now).unwrap().is_empty());
        assert_eq!(loads.get(), 1);
        assert_eq!(git_calls.get(), 1);

        assert_eq!(
            sample(now + std::time::Duration::from_secs(61)).unwrap(),
            ["/listed/feature"]
        );
        assert_eq!(loads.get(), 2);
        assert_eq!(git_calls.get(), 2);
    }

    #[test]
    fn remnant_dir_of_a_registered_row_is_gone_only_when_git_unlisted() {
        let dir = tempfile::tempdir().unwrap();
        let remnant = dir.path().join("feature");
        std::fs::create_dir(&remnant).unwrap();
        std::fs::write(remnant.join("leftover"), "x").unwrap();
        let remnant_s = remnant.to_string_lossy().into_owned();
        let db = Db::open_memory().unwrap();
        db.put_worktree("feature", "/repo", &remnant_s, "feature", None, None)
            .unwrap();
        let settled = Mutex::new(HashMap::new());
        let kept = sample_vanished_tabs(
            &[VanishedGroup {
                path: remnant_s.clone(),
                epoch: 1,
            }],
            |_| PathState::Remnant,
            || {
                Ok(RegistrySample::from_location(
                    db.worktrees()?,
                    db.workspaces()?,
                ))
            },
            |_, _| false,
            &settled,
            std::time::Instant::now(),
        )
        .unwrap();
        assert!(kept.is_empty());

        let gone = sample_vanished_tabs(
            &[VanishedGroup {
                path: remnant_s.clone(),
                epoch: 2,
            }],
            |_| PathState::Remnant,
            || {
                Ok(RegistrySample::from_location(
                    db.worktrees()?,
                    db.workspaces()?,
                ))
            },
            |_, _| true,
            &settled,
            std::time::Instant::now(),
        )
        .unwrap();
        assert_eq!(gone, [remnant_s]);
    }

    #[test]
    fn remnant_dir_still_listed_by_git_is_kept_and_settled() {
        let dir = tempfile::tempdir().unwrap();
        let remnant = dir.path().join("feature");
        std::fs::create_dir(&remnant).unwrap();
        let remnant_s = remnant.to_string_lossy().into_owned();
        let db = Db::open_memory().unwrap();
        db.put_worktree("feature", "/repo", &remnant_s, "feature", None, None)
            .unwrap();
        let settled = Mutex::new(HashMap::new());
        let git_calls = std::cell::Cell::new(0);
        for _ in 0..2 {
            let kept = sample_vanished_tabs(
                &[VanishedGroup {
                    path: remnant_s.clone(),
                    epoch: 1,
                }],
                |_| PathState::Remnant,
                || {
                    Ok(RegistrySample::from_location(
                        db.worktrees()?,
                        db.workspaces()?,
                    ))
                },
                |_, _| {
                    git_calls.set(git_calls.get() + 1);
                    false
                },
                &settled,
                std::time::Instant::now(),
            )
            .unwrap();
            assert!(kept.is_empty());
        }
        assert_eq!(git_calls.get(), 1);
    }

    #[test]
    fn missing_registered_row_is_gone_without_asking_git() {
        let db = Db::open_memory().unwrap();
        db.put_worktree(
            "feature",
            "/repo",
            "/missing/feature",
            "feature",
            None,
            None,
        )
        .unwrap();
        let gone = sample_vanished_tabs(
            &[VanishedGroup {
                path: "/missing/feature".into(),
                epoch: 1,
            }],
            |_| PathState::Missing,
            || {
                Ok(RegistrySample::from_location(
                    db.worktrees()?,
                    db.workspaces()?,
                ))
            },
            |_, _| panic!("missing registered rows do not need git confirmation"),
            &Mutex::new(HashMap::new()),
            std::time::Instant::now(),
        )
        .unwrap();
        assert_eq!(gone, ["/missing/feature"]);
    }

    #[test]
    fn sample_inputs_confine_git_confirmation_to_branch_groups() {
        let session = crate::session::Session {
            id: "default".into(),
            worktrees: vec![
                crate::session::WorktreeGroup::new(
                    "repo/home",
                    crate::session::GroupKind::Home,
                    "/home",
                ),
                crate::session::WorktreeGroup::new(
                    "repo/feature",
                    crate::session::GroupKind::Branch,
                    "/feature",
                ),
                crate::session::WorktreeGroup::new(
                    "terminal",
                    crate::session::GroupKind::Terminal,
                    "/remote",
                ),
            ],
            active: 0,
        };
        let inputs = sample_inputs(&session);
        assert_eq!(inputs.groups.len(), 3);
        assert_eq!(
            inputs.branch_paths,
            ["/feature".to_owned()].into_iter().collect()
        );
        assert_eq!(inputs.session_root, None);
    }

    #[test]
    fn branch_path_state_reads_dot_git_presence() {
        let dir = tempfile::tempdir().unwrap();
        let checkout = dir.path().join("checkout");
        let remnant = dir.path().join("remnant");
        let absent = dir.path().join("absent");
        std::fs::create_dir(&checkout).unwrap();
        std::fs::write(checkout.join(".git"), "gitdir: /tmp/admin\n").unwrap();
        std::fs::create_dir(&remnant).unwrap();
        assert_eq!(
            branch_path_state(checkout.to_str().unwrap()),
            PathState::Checkout
        );
        assert_eq!(
            branch_path_state(remnant.to_str().unwrap()),
            PathState::Remnant
        );
        assert_eq!(
            branch_path_state(absent.to_str().unwrap()),
            PathState::Missing
        );
    }

    #[test]
    fn worktree_removed_with_its_registry_row_out_of_band_is_reaped() {
        let _isolation = TestIsolation::new();
        let db = Db::open_memory().unwrap();
        let (root, feat) = repo_with_feat(&db, "vanished-rowless");
        let root_s = root.to_string_lossy().into_owned();
        let feat_s = feat.to_string_lossy().into_owned();
        git(&root, &["worktree", "remove", "--force", &feat_s]);
        db.del_worktree(&feat_s).unwrap();
        let branch_paths = [feat_s.clone()].into_iter().collect::<HashSet<_>>();
        let mut git_cache = HashMap::new();
        let gone = sample_vanished_tabs(
            &[VanishedGroup {
                path: feat_s.clone(),
                epoch: 1,
            }],
            |path| {
                if branch_paths.contains(path) {
                    branch_path_state(path)
                } else {
                    dir_path_state(path)
                }
            },
            || {
                Ok(RegistrySample::from_location(
                    db.worktrees()?,
                    db.workspaces()?,
                ))
            },
            |path, row_root| {
                git_confirms_unlisted(
                    &branch_paths,
                    Some(root_s.as_str()),
                    path,
                    row_root,
                    &mut git_cache,
                )
            },
            &Mutex::new(HashMap::new()),
            std::time::Instant::now(),
        )
        .unwrap();
        assert_eq!(gone, [feat_s]);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn interrupted_removal_remnant_is_reaped_after_git_prunes_it() {
        let _isolation = TestIsolation::new();
        let db = Db::open_memory().unwrap();
        let (root, feat) = repo_with_feat(&db, "vanished-remnant");
        let root_s = root.to_string_lossy().into_owned();
        let feat_s = feat.to_string_lossy().into_owned();
        git(&root, &["worktree", "remove", "--force", &feat_s]);
        std::fs::create_dir_all(feat.join("crates/x")).unwrap();
        std::fs::write(feat.join("crates/x/leftover"), "x").unwrap();
        git(&root, &["worktree", "prune"]);
        let branch_paths = [feat_s.clone()].into_iter().collect::<HashSet<_>>();
        let mut git_cache = HashMap::new();
        let gone = sample_vanished_tabs(
            &[VanishedGroup {
                path: feat_s.clone(),
                epoch: 1,
            }],
            |path| {
                if branch_paths.contains(path) {
                    branch_path_state(path)
                } else {
                    dir_path_state(path)
                }
            },
            || {
                Ok(RegistrySample::from_location(
                    db.worktrees()?,
                    db.workspaces()?,
                ))
            },
            |path, row_root| {
                git_confirms_unlisted(
                    &branch_paths,
                    Some(root_s.as_str()),
                    path,
                    row_root,
                    &mut git_cache,
                )
            },
            &Mutex::new(HashMap::new()),
            std::time::Instant::now(),
        )
        .unwrap();
        assert_eq!(gone, [feat_s]);
        std::fs::remove_dir_all(&root).unwrap();
        std::fs::remove_dir_all(&feat).unwrap();
    }

    // test code: fixture plumbing, never on the event loop.
    #[expect(clippy::disallowed_methods)]
    fn git(dir: &Path, args: &[&str]) {
        let ok = cleanup::test_git(dir)
            .args(args)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        assert!(ok, "git {} failed in {}", args.join(" "), dir.display());
    }

    /// A repo on `main` with a linked worktree holding branch `feat`. Registers
    /// both the workspace and the feat worktree in `db` so folder filing has a
    /// row to update. Returns (repo_root, feat_worktree_path).
    fn repo_with_feat(db: &Db, tag: &str) -> (PathBuf, PathBuf) {
        // Cleanup refuses noncanonical worktree paths by design; the macOS temp
        // root (`/var/folders`, or `/tmp`, both under `/private`) is a symlink.
        let temp = std::env::temp_dir();
        let temp = std::fs::canonicalize(&temp).unwrap_or(temp);
        let root = temp.join(format!(
            "tg-mlife-{tag}-{}-{}",
            std::process::id(),
            util::now()
        ));
        let feat = root.with_extension("feat");
        let _ = std::fs::remove_dir_all(&root); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
        let _ = std::fs::remove_dir_all(&feat); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
        std::fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["config", "user.name", "t"]);
        git(&root, &["config", "user.email", "t@e"]);
        git(&root, &["config", "commit.gpgsign", "false"]);
        std::fs::write(root.join("base.txt"), "base\n").unwrap();
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-q", "-m", "c0"]);
        git(
            &root,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feat",
                feat.to_str().unwrap(),
                "main",
            ],
        );
        let root_s = root.to_string_lossy().to_string();
        let feat_s = feat.to_string_lossy().to_string();
        db.put_workspace(&root_s, "repo", "git").unwrap();
        db.put_worktree("feat", &root_s, &feat_s, "feat", None, None)
            .unwrap();
        (root, feat)
    }

    fn cfg(on_landed: OnLanded) -> MergeQueueConfig {
        MergeQueueConfig {
            organize_folders: true,
            queued_folder: "Merging".into(),
            on_landed,
            merged_folder: "Merged".into(),
            failed_folder: "Needs attention".into(),
            ..MergeQueueConfig::default()
        }
    }

    #[expect(clippy::disallowed_methods)]
    fn record_private_land(db: &Db, root: &Path, worktree: &str, branch: &str) -> String {
        let output = cleanup::test_git(root)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        assert!(output.status.success());
        let commit = String::from_utf8(output.stdout).unwrap().trim().to_owned();
        db.enqueue_merge(worktree, branch, "main").unwrap();
        db.update_merge_status(worktree, "landed", Some(&commit), None, None)
            .unwrap();
        commit
    }

    #[expect(clippy::disallowed_methods)]
    fn record_private_land_with_identity(
        db: &Db,
        root: &Path,
        worktree: &str,
        branch: &str,
        identity: &thegn_core::store::MergeEnqueueIdentity,
    ) -> String {
        let output = cleanup::test_git(root)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        assert!(output.status.success());
        let commit = String::from_utf8(output.stdout).unwrap().trim().to_owned();
        db.enqueue_merge_candidate_with_identity(
            worktree,
            branch,
            "main",
            &thegn_core::store::MergeCandidate {
                branch_ref: format!("refs/heads/{branch}"),
                candidate_oid: commit.clone(),
            },
            identity,
        )
        .unwrap();
        db.update_merge_status(worktree, "landed", Some(&commit), None, None)
            .unwrap();
        commit
    }

    #[test]
    fn landed_remove_identity_refuses_a_recreated_worktree() {
        let _isolation = TestIsolation::new();
        let db = Db::open_memory().unwrap();
        let (root, feat) = repo_with_feat(&db, "rm-identity-recreated");
        let feat_s = feat.to_string_lossy().to_string();
        let real = crate::merge_ops::local_enqueue_identity(&feat).unwrap();
        let identity = thegn_core::store::MergeEnqueueIdentity {
            common_dir: real.common_dir.clone(),
            admin_dir: "/not/the/original/registration".into(),
        };
        let commit = record_private_land_with_identity(&db, &root, &feat_s, "feat", &identity);
        let before = db.list_merge_queue().unwrap();

        let outcome = remove_landed(&db, &root, &feat_s, "feat", "main", Some(&commit), true);

        assert!(matches!(
            outcome,
            CleanupOutcome::Refused {
                class: CleanupRefusal::Identity,
                ref reason,
            } if reason.contains("registration changed since enqueue")
        ));
        assert!(feat.is_dir(), "worktree dir must be kept");
        assert_eq!(db.list_merge_queue().unwrap(), before);
        assert!(util::git_ok(
            &root,
            &["rev-parse", "--verify", "--quiet", "refs/heads/feat"]
        ));
        std::fs::remove_dir_all(&root).unwrap();
        std::fs::remove_dir_all(&feat).unwrap();
    }

    #[test]
    fn landed_remove_identity_refuses_another_repository() {
        let _isolation = TestIsolation::new();
        let db = Db::open_memory().unwrap();
        let (root, feat) = repo_with_feat(&db, "rm-identity-repository");
        let feat_s = feat.to_string_lossy().to_string();
        let real = crate::merge_ops::local_enqueue_identity(&feat).unwrap();
        let identity = thegn_core::store::MergeEnqueueIdentity {
            common_dir: "/elsewhere/.git".into(),
            admin_dir: real.admin_dir,
        };
        let commit = record_private_land_with_identity(&db, &root, &feat_s, "feat", &identity);

        let outcome = remove_landed(&db, &root, &feat_s, "feat", "main", Some(&commit), true);

        assert!(matches!(
            outcome,
            CleanupOutcome::Refused {
                class: CleanupRefusal::Identity,
                ref reason,
            } if reason.contains("different repository")
        ));
        assert!(feat.is_dir(), "worktree dir must be kept");
        std::fs::remove_dir_all(&root).unwrap();
        std::fs::remove_dir_all(&feat).unwrap();
    }

    #[test]
    fn landed_remove_proceeds_when_the_recorded_identity_matches() {
        let _isolation = TestIsolation::new();
        let db = Db::open_memory().unwrap();
        let (root, feat) = repo_with_feat(&db, "rm-identity-matches");
        let feat_s = feat.to_string_lossy().to_string();
        let identity = crate::merge_ops::local_enqueue_identity(&feat).unwrap();
        let commit = record_private_land_with_identity(&db, &root, &feat_s, "feat", &identity);

        let outcome = remove_landed(&db, &root, &feat_s, "feat", "main", Some(&commit), true);

        assert!(matches!(outcome, CleanupOutcome::Removed { .. }));
        assert!(
            !feat.is_dir(),
            "matching worktree identity should be removed"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// The folder name a worktree is currently filed under, if any.
    fn folder_of(db: &Db, repo: &str, wt: &str) -> Option<String> {
        let fid = db
            .worktrees()
            .ok()?
            .into_iter()
            .find(|w| w.worktree == wt)?
            .folder_id?;
        db.folders_for_workspace(repo)
            .ok()?
            .into_iter()
            .find(|f| f.folder_id == fid)
            .map(|f| f.name)
    }

    #[test]
    fn workspace_repo_path_prefers_registered_workspace_string() {
        let db = Db::open_memory().unwrap();
        db.put_workspace("/repos/app", "app", "git").unwrap();
        // A worktree row whose recorded repo_path diverges (trailing slash) from
        // the workspace's canonical string must still file under the workspace
        // string, so the sidebar's byte-for-byte folder filter matches.
        let got = workspace_repo_path(&db, Path::new("/repos/app"), Some("/repos/app/")).unwrap();
        assert_eq!(got, "/repos/app");
    }

    #[test]
    fn workspace_repo_path_falls_back_when_no_workspace_row() {
        let db = Db::open_memory().unwrap();
        // No workspace registered: fall back to the recorded path, else repo_root.
        assert_eq!(
            workspace_repo_path(&db, Path::new("/repos/none"), Some("/repos/rec")).unwrap(),
            "/repos/rec"
        );
        assert_eq!(
            workspace_repo_path(&db, Path::new("/repos/none"), None).unwrap(),
            "/repos/none"
        );
    }

    #[test]
    fn vanished_indices_use_only_worker_reported_paths() {
        let session = crate::session::Session {
            id: "s".into(),
            worktrees: vec![
                crate::session::WorktreeGroup::new(
                    "repo/home",
                    crate::session::GroupKind::Home,
                    "/repo",
                ),
                crate::session::WorktreeGroup::new(
                    "repo/feature",
                    crate::session::GroupKind::Branch,
                    "/repo-feature",
                ),
            ],
            active: 0,
        };
        assert_eq!(
            vanished_group_indices(
                &session,
                &[VanishedGroup {
                    path: "/repo-feature".into(),
                    epoch: session.worktrees[1].epoch,
                }],
            ),
            vec![1]
        );
        assert!(vanished_group_indices(&session, &[]).is_empty());
    }

    fn vanished_test_panes() -> crate::panes::Panes {
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        crate::panes::Panes::new(tx)
    }

    #[test]
    fn stale_vanish_result_spares_a_group_reopened_at_the_same_path() {
        let sampled = crate::session::WorktreeGroup::new(
            "repo/feature",
            crate::session::GroupKind::Branch,
            "/r/feat",
        );
        let result = VanishedTabs {
            groups: vec![VanishedGroup {
                path: sampled.path.clone(),
                epoch: sampled.epoch,
            }],
        };
        let mut session = crate::session::Session {
            id: "s".into(),
            worktrees: vec![sampled],
            active: 0,
        };
        session.worktrees.clear();
        let replacement = crate::session::WorktreeGroup::new(
            "repo/feature",
            crate::session::GroupKind::Branch,
            "/r/feat",
        );
        let replacement_epoch = replacement.epoch;
        session.worktrees.push(replacement);

        assert!(result.still_current(&session).is_empty());
        assert!(!apply_vanished_tabs(
            &mut session,
            &mut vanished_test_panes(),
            &result.groups,
            &crate::handlers::worktree_delete::Landing::default(),
        ));
        assert_eq!(session.worktrees.len(), 1);
        assert_eq!(session.worktrees[0].epoch, replacement_epoch);
    }

    #[test]
    fn vanish_result_removes_only_the_sampled_epoch() {
        let _isolation = TestIsolation::new();
        let sampled = crate::session::WorktreeGroup::new(
            "repo/feature",
            crate::session::GroupKind::Branch,
            "/r/feat",
        );
        let result = VanishedTabs {
            groups: vec![VanishedGroup {
                path: sampled.path.clone(),
                epoch: sampled.epoch,
            }],
        };
        let replacement = crate::session::WorktreeGroup::new(
            "repo/feature-replacement",
            crate::session::GroupKind::Branch,
            "/r/feat",
        );
        let replacement_epoch = replacement.epoch;
        let mut session = crate::session::Session {
            id: "s".into(),
            worktrees: vec![sampled, replacement],
            active: 0,
        };

        assert!(apply_vanished_tabs(
            &mut session,
            &mut vanished_test_panes(),
            &result.groups,
            &crate::handlers::worktree_delete::Landing::default(),
        ));
        assert_eq!(session.worktrees.len(), 1);
        assert_eq!(session.worktrees[0].epoch, replacement_epoch);
    }

    #[test]
    fn vanish_result_still_removes_the_sampled_group() {
        let _isolation = TestIsolation::new();
        let sampled = crate::session::WorktreeGroup::new(
            "repo/feature",
            crate::session::GroupKind::Branch,
            "/r/feat",
        );
        let result = VanishedTabs {
            groups: vec![VanishedGroup {
                path: sampled.path.clone(),
                epoch: sampled.epoch,
            }],
        };
        let mut session = crate::session::Session {
            id: "s".into(),
            worktrees: vec![sampled],
            active: 0,
        };

        assert!(apply_vanished_tabs(
            &mut session,
            &mut vanished_test_panes(),
            &result.groups,
            &crate::handlers::worktree_delete::Landing::default(),
        ));
        assert!(session.worktrees.is_empty());
    }

    #[test]
    fn enqueue_files_into_merging_folder() {
        let _isolation = TestIsolation::new();
        let db = Db::open_memory().unwrap();
        let (root, feat) = repo_with_feat(&db, "enq");
        let (root_s, feat_s) = (
            root.to_string_lossy().to_string(),
            feat.to_string_lossy().to_string(),
        );
        apply(
            &cfg(OnLanded::Move),
            &db,
            &root,
            &feat_s,
            "feat",
            LifecycleEvent::Enqueued,
        );
        assert_eq!(folder_of(&db, &root_s, &feat_s).as_deref(), Some("Merging"));
        let _ = std::fs::remove_dir_all(&root); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
        let _ = std::fs::remove_dir_all(&feat); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
    }

    #[test]
    fn enqueue_registers_unopened_workspace_and_files_merging_folder() {
        let _isolation = TestIsolation::new();
        let db = Db::open_memory().unwrap();
        let (root, feat) = repo_with_feat(&db, "unopened-workspace");
        let (root_s, feat_s) = (
            root.to_string_lossy().to_string(),
            feat.to_string_lossy().to_string(),
        );
        db.del_workspace(&root_s).unwrap();
        assert!(
            db.workspaces()
                .unwrap()
                .iter()
                .all(|row| row.repo_path != root_s)
        );

        apply(
            &cfg(OnLanded::Move),
            &db,
            &root,
            &feat_s,
            "feat",
            LifecycleEvent::Enqueued,
        );

        assert!(
            db.workspaces()
                .unwrap()
                .iter()
                .any(|row| row.repo_path == root_s)
        );
        assert_eq!(folder_of(&db, &root_s, &feat_s).as_deref(), Some("Merging"));
        let _ = std::fs::remove_dir_all(&root); // best-effort cleanup
        let _ = std::fs::remove_dir_all(&feat); // best-effort cleanup
    }

    #[test]
    fn enqueue_registers_unpersisted_worktree_and_files_it() {
        let _isolation = TestIsolation::new();
        let db = Db::open_memory().unwrap();
        let (root, feat) = repo_with_feat(&db, "unpersist");
        let (root_s, feat_s) = (
            root.to_string_lossy().to_string(),
            feat.to_string_lossy().to_string(),
        );
        // A worktree created via git / the `wt` CLI has no `worktrees` cache row.
        db.del_worktree(&feat_s).unwrap();
        assert!(db.repo_root_for(&feat_s).unwrap().is_none(), "no cache row");

        apply(
            &cfg(OnLanded::Move),
            &db,
            &root,
            &feat_s,
            "feat",
            LifecycleEvent::Enqueued,
        );

        // The row is registered AND filed into Merging — with the canonical
        // `{slug}/{branch}` tab name the sidebar joins live tabs by, so the
        // folder actually renders (a bare `set_worktree_folder` would no-op).
        assert_eq!(folder_of(&db, &root_s, &feat_s).as_deref(), Some("Merging"));
        let row = db
            .worktrees()
            .unwrap()
            .into_iter()
            .find(|w| w.worktree == feat_s)
            .expect("worktree row registered");
        let slug = thegn_core::repo::repo_slug_with(&db, &root);
        assert_eq!(row.tab_name, thegn_core::repo::branch_tab(&slug, "feat"));
        let _ = std::fs::remove_dir_all(&root); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
        let _ = std::fs::remove_dir_all(&feat); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
    }

    #[test]
    fn landed_move_refiles_and_keeps_row() {
        let _isolation = TestIsolation::new();
        let db = Db::open_memory().unwrap();
        let (root, feat) = repo_with_feat(&db, "move");
        let (root_s, feat_s) = (
            root.to_string_lossy().to_string(),
            feat.to_string_lossy().to_string(),
        );
        db.enqueue_merge(&feat_s, "feat", "main").unwrap();
        apply(
            &cfg(OnLanded::Move),
            &db,
            &root,
            &feat_s,
            "feat",
            LifecycleEvent::Landed,
        );
        assert_eq!(folder_of(&db, &root_s, &feat_s).as_deref(), Some("Merged"));
        // The queue row survives a folder move (additive; the ✓ stays in the panel).
        assert!(
            db.list_merge_queue()
                .unwrap()
                .iter()
                .any(|r| r.worktree == feat_s)
        );
        let _ = std::fs::remove_dir_all(&root); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
        let _ = std::fs::remove_dir_all(&feat); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
    }

    #[test]
    fn landed_remove_keeps_branch_and_explicit_queue_hold() {
        let _isolation = TestIsolation::new();
        let db = Db::open_memory().unwrap();
        let (root, feat) = repo_with_feat(&db, "rm");
        let feat_s = feat.to_string_lossy().to_string();
        let commit = record_private_land(&db, &root, &feat_s, "feat");
        apply_landed(&cfg(OnLanded::Remove), &db, &root, &feat_s, "feat", &commit);
        assert!(!feat.is_dir(), "worktree dir removed");
        assert!(
            util::git_ok(
                &root,
                &["rev-parse", "--verify", "--quiet", "refs/heads/feat"]
            ),
            "branch retained pending atomic type proof"
        );
        assert!(db.list_merge_queue().unwrap().iter().any(|r| {
            r.worktree == feat_s
                && thegn_core::merge_sweep::CleanupHold::from_detail(r.error_detail.as_deref())
                    .is_some()
        }));
        let _ = std::fs::remove_dir_all(&root); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
    }

    /// Crash after the worktree was removed but before the queue row moved:
    /// cleanup retires the landed row in one pass, keeps the branch, and a
    /// re-run is a no-op (the row is gone, so it refuses on identity).
    #[test]
    fn crash_after_worktree_removal_before_row_update_converges() {
        let _isolation = TestIsolation::new();
        let db = Db::open_memory().unwrap();
        let (root, feat) = repo_with_feat(&db, "crash-a");
        let feat_s = feat.to_string_lossy().to_string();
        let commit = record_private_land(&db, &root, &feat_s, "feat");
        // The "crash": git removed the checkout, the DB write never happened.
        git(
            &root,
            &["worktree", "remove", "--force", feat.to_str().unwrap()],
        );
        let first = remove_landed(&db, &root, &feat_s, "feat", "main", Some(&commit), true);
        assert!(
            matches!(
                first,
                CleanupOutcome::Removed {
                    queue_removed: true,
                    branch_deleted: false,
                    ..
                }
            ),
            "{first:?}"
        );
        assert!(db.list_merge_queue().unwrap().is_empty(), "row retired");
        assert!(!feat.exists(), "worktree stays gone");
        let second = remove_landed(&db, &root, &feat_s, "feat", "main", Some(&commit), true);
        assert!(
            matches!(
                second,
                CleanupOutcome::Refused {
                    class: CleanupRefusal::Identity,
                    ..
                }
            ),
            "{second:?}"
        );
        assert!(db.list_merge_queue().unwrap().is_empty());
        assert!(
            util::git_ok(
                &root,
                &["rev-parse", "--verify", "--quiet", "refs/heads/feat"]
            ),
            "branch kept"
        );
        let _ = std::fs::remove_dir_all(&root); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
    }

    /// A commit that does not match the recorded landed oid refuses and keeps
    /// the row even when the checkout is gone.
    #[test]
    fn gone_checkout_with_mismatched_commit_keeps_the_row() {
        let _isolation = TestIsolation::new();
        let db = Db::open_memory().unwrap();
        let (root, feat) = repo_with_feat(&db, "crash-d");
        let feat_s = feat.to_string_lossy().to_string();
        let _commit = record_private_land(&db, &root, &feat_s, "feat");
        git(
            &root,
            &["worktree", "remove", "--force", feat.to_str().unwrap()],
        );
        let rows_before = db.list_merge_queue().unwrap();
        let bogus = "0123456789abcdef0123456789abcdef01234567";
        let outcome = remove_landed(&db, &root, &feat_s, "feat", "main", Some(bogus), true);
        assert!(
            matches!(
                outcome,
                CleanupOutcome::Refused {
                    class: CleanupRefusal::Identity,
                    ..
                }
            ),
            "{outcome:?}"
        );
        assert_eq!(db.list_merge_queue().unwrap(), rows_before, "row kept");
        let _ = std::fs::remove_dir_all(&root); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
    }

    /// A remote (ssh/provider) row's path never exists on this host and is
    /// never in the local registry; it must not be mistaken for a gone
    /// checkout and silently retired.
    #[test]
    fn gone_remote_checkout_is_never_retired() {
        let _isolation = TestIsolation::new();
        let db = Db::open_memory().unwrap();
        let (root, feat) = repo_with_feat(&db, "crash-f");
        let feat_s = feat.to_string_lossy().to_string();
        db.set_worktree_location(&feat_s, r#"{"host":"build"}"#)
            .unwrap();
        let commit = record_private_land(&db, &root, &feat_s, "feat");
        git(
            &root,
            &["worktree", "remove", "--force", feat.to_str().unwrap()],
        );
        let rows_before = db.list_merge_queue().unwrap();
        let outcome = remove_landed(&db, &root, &feat_s, "feat", "main", Some(&commit), true);
        assert!(
            !matches!(outcome, CleanupOutcome::Removed { .. }),
            "{outcome:?}"
        );
        assert_eq!(db.list_merge_queue().unwrap(), rows_before, "row kept");
        let _ = std::fs::remove_dir_all(&root); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
    }

    #[test]
    fn crash_recovery_keeps_the_row_of_a_pinned_ssh_worktree() {
        let _isolation = TestIsolation::new();
        let db = Db::open_memory().unwrap();
        let (root, feat) = repo_with_feat(&db, "crash-ssh");
        let feat_s = feat.to_string_lossy().to_string();
        db.set_worktree_env(&feat_s, "build").unwrap();
        let mut cfg = Config::default();
        cfg.env.insert(
            "build".into(),
            EnvConfig {
                placement: PlacementMode::Ssh,
                ..Default::default()
            },
        );
        let commit = record_private_land(&db, &root, &feat_s, "feat");
        git(
            &root,
            &["worktree", "remove", "--force", feat.to_str().unwrap()],
        );
        let rows_before = db.list_merge_queue().unwrap();
        let selected = rows_before
            .iter()
            .find(|row| row.worktree == feat_s)
            .expect("landed queue row")
            .clone();
        let outcome = remove_landed_with_config(
            &cfg,
            &db,
            &root,
            &feat_s,
            "feat",
            "main",
            Some(&commit),
            &selected,
            true,
        );
        assert!(
            !matches!(outcome, CleanupOutcome::Removed { .. }),
            "{outcome:?}"
        );
        assert_eq!(db.list_merge_queue().unwrap(), rows_before, "row kept");
        assert!(util::git_ok(
            &root,
            &["rev-parse", "--verify", "--quiet", "refs/heads/feat"]
        ));
        let _ = std::fs::remove_dir_all(&root); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
    }

    #[test]
    fn crash_recovery_keeps_the_row_of_an_ambient_k8s_repo() {
        let _isolation = TestIsolation::new();
        let db = Db::open_memory().unwrap();
        let (root, feat) = repo_with_feat(&db, "crash-k8s");
        let root_s = root.to_string_lossy().to_string();
        let feat_s = feat.to_string_lossy().to_string();
        db.set_workspace_env(&root_s, "kube").unwrap();
        let mut cfg = Config::default();
        cfg.env.insert(
            "kube".into(),
            EnvConfig {
                placement: PlacementMode::K8s,
                ..Default::default()
            },
        );
        let commit = record_private_land(&db, &root, &feat_s, "feat");
        git(
            &root,
            &["worktree", "remove", "--force", feat.to_str().unwrap()],
        );
        let rows_before = db.list_merge_queue().unwrap();
        let selected = rows_before
            .iter()
            .find(|row| row.worktree == feat_s)
            .expect("landed queue row")
            .clone();
        let outcome = remove_landed_with_config(
            &cfg,
            &db,
            &root,
            &feat_s,
            "feat",
            "main",
            Some(&commit),
            &selected,
            true,
        );
        assert!(
            !matches!(outcome, CleanupOutcome::Removed { .. }),
            "{outcome:?}"
        );
        assert_eq!(db.list_merge_queue().unwrap(), rows_before, "row kept");
        assert!(util::git_ok(
            &root,
            &["rev-parse", "--verify", "--quiet", "refs/heads/feat"]
        ));
        let _ = std::fs::remove_dir_all(&root); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
    }

    #[test]
    fn crash_recovery_converges_under_a_local_ambient_env() {
        let _isolation = TestIsolation::new();
        let db = Db::open_memory().unwrap();
        let (root, feat) = repo_with_feat(&db, "crash-local");
        let root_s = root.to_string_lossy().to_string();
        let feat_s = feat.to_string_lossy().to_string();
        db.set_workspace_env(&root_s, "host").unwrap();
        let mut cfg = Config::default();
        cfg.env.insert(
            "host".into(),
            EnvConfig {
                placement: PlacementMode::Local,
                ..Default::default()
            },
        );
        let commit = record_private_land(&db, &root, &feat_s, "feat");
        git(
            &root,
            &["worktree", "remove", "--force", feat.to_str().unwrap()],
        );
        let selected = db
            .list_merge_queue()
            .unwrap()
            .into_iter()
            .find(|row| row.worktree == feat_s)
            .expect("landed queue row");
        let first = remove_landed_with_config(
            &cfg,
            &db,
            &root,
            &feat_s,
            "feat",
            "main",
            Some(&commit),
            &selected,
            true,
        );
        assert!(
            matches!(
                first,
                CleanupOutcome::Removed {
                    queue_removed: true,
                    branch_deleted: false,
                    ..
                }
            ),
            "{first:?}"
        );
        assert!(db.list_merge_queue().unwrap().is_empty(), "row retired");
        let second = remove_landed(&db, &root, &feat_s, "feat", "main", Some(&commit), true);
        assert!(
            matches!(
                second,
                CleanupOutcome::Refused {
                    class: CleanupRefusal::Identity,
                    ..
                }
            ),
            "{second:?}"
        );
        let _ = std::fs::remove_dir_all(&root); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
    }

    #[test]
    fn crash_recovery_without_a_registry_row_keeps_a_remote_repo_row() {
        let _isolation = TestIsolation::new();
        let db = Db::open_memory().unwrap();
        let (root, feat) = repo_with_feat(&db, "crash-unregistered-remote");
        let root_s = root.to_string_lossy().to_string();
        let feat_s = feat.to_string_lossy().to_string();
        let mut cfg = Config::default();
        cfg.env.insert(
            "kube".into(),
            EnvConfig {
                placement: PlacementMode::K8s,
                ..Default::default()
            },
        );
        db.del_worktree(&feat_s).unwrap();
        let commit = record_private_land(&db, &root, &feat_s, "feat");
        db.set_workspace_env(&root_s, "kube").unwrap();
        git(
            &root,
            &["worktree", "remove", "--force", feat.to_str().unwrap()],
        );
        let rows_before = db.list_merge_queue().unwrap();
        let selected = rows_before
            .iter()
            .find(|row| row.worktree == feat_s)
            .expect("landed queue row")
            .clone();
        let outcome = remove_landed_with_config(
            &cfg,
            &db,
            &root,
            &feat_s,
            "feat",
            "main",
            Some(&commit),
            &selected,
            true,
        );
        assert!(
            !matches!(outcome, CleanupOutcome::Removed { .. }),
            "{outcome:?}"
        );
        assert_eq!(db.list_merge_queue().unwrap(), rows_before, "row kept");
        assert!(util::git_ok(
            &root,
            &["rev-parse", "--verify", "--quiet", "refs/heads/feat"]
        ));
        let _ = std::fs::remove_dir_all(&root); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
    }

    #[test]
    fn crash_recovery_without_a_registry_row_converges_locally() {
        let _isolation = TestIsolation::new();
        let db = Db::open_memory().unwrap();
        let (root, feat) = repo_with_feat(&db, "crash-unregistered-local");
        let feat_s = feat.to_string_lossy().to_string();
        db.del_worktree(&feat_s).unwrap();
        let commit = record_private_land(&db, &root, &feat_s, "feat");
        git(
            &root,
            &["worktree", "remove", "--force", feat.to_str().unwrap()],
        );
        let cfg = Config::default();
        let selected = db
            .list_merge_queue()
            .unwrap()
            .into_iter()
            .find(|row| row.worktree == feat_s)
            .expect("landed queue row");
        let outcome = remove_landed_with_config(
            &cfg,
            &db,
            &root,
            &feat_s,
            "feat",
            "main",
            Some(&commit),
            &selected,
            true,
        );
        assert!(
            matches!(
                outcome,
                CleanupOutcome::Removed {
                    queue_removed: true,
                    branch_deleted: false,
                    ..
                }
            ),
            "{outcome:?}"
        );
        assert!(db.list_merge_queue().unwrap().is_empty(), "row retired");
        let _ = std::fs::remove_dir_all(&root); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
    }

    /// A checkout that still exists never takes the already-gone path.
    #[test]
    fn existing_checkout_does_not_take_the_already_gone_path() {
        let _isolation = TestIsolation::new();
        let db = Db::open_memory().unwrap();
        let (root, feat) = repo_with_feat(&db, "crash-e");
        let feat_s = feat.to_string_lossy().to_string();
        let commit = record_private_land(&db, &root, &feat_s, "feat");
        let outcome = remove_landed(&db, &root, &feat_s, "feat", "main", Some(&commit), true);
        assert!(
            matches!(&outcome, CleanupOutcome::Removed { bookkeeping_errors, .. } if bookkeeping_errors.is_empty()),
            "{outcome:?}"
        );
        assert!(!feat.exists(), "normal removal ran");
        assert!(
            db.list_merge_queue()
                .unwrap()
                .iter()
                .any(|r| r.worktree == feat_s
                    && thegn_core::merge_sweep::CleanupHold::from_detail(
                        r.error_detail.as_deref()
                    )
                    .is_some()),
            "normal path keeps its branch hold row"
        );
        let _ = std::fs::remove_dir_all(&root); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
    }

    /// Reverse order: the queue row was retired but the worktree survived the
    /// crash. Cleanup refuses on identity and touches neither.
    #[test]
    fn crash_after_row_retired_before_worktree_removal_keeps_worktree() {
        let _isolation = TestIsolation::new();
        let db = Db::open_memory().unwrap();
        let (root, feat) = repo_with_feat(&db, "crash-b");
        let feat_s = feat.to_string_lossy().to_string();
        let commit = record_private_land(&db, &root, &feat_s, "feat");
        db.remove_merge_entry_as(&feat_s, "thegn close").unwrap();
        for _ in 0..2 {
            let outcome = remove_landed(&db, &root, &feat_s, "feat", "main", Some(&commit), false);
            assert!(
                matches!(
                    outcome,
                    CleanupOutcome::Refused {
                        class: CleanupRefusal::Identity,
                        ..
                    }
                ),
                "{outcome:?}"
            );
            assert!(feat.is_dir(), "worktree kept");
            assert!(db.list_merge_queue().unwrap().is_empty());
        }
        let _ = std::fs::remove_dir_all(&root); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
    }

    /// A completed hold re-run (crash recovery of the notice path) converges
    /// on one hold row and exactly one inbox notice.
    #[test]
    fn rerunning_a_held_landed_cleanup_records_one_notice() {
        let _isolation = TestIsolation::new();
        let db = Db::open_memory().unwrap();
        let (root, feat) = repo_with_feat(&db, "crash-c");
        let feat_s = feat.to_string_lossy().to_string();
        let commit = record_private_land(&db, &root, &feat_s, "feat");
        apply_landed(&cfg(OnLanded::Remove), &db, &root, &feat_s, "feat", &commit);
        assert!(!feat.exists());
        let notices = db.get_unread_notifications().unwrap().len();
        let rows = db.list_merge_queue().unwrap();
        apply_landed(&cfg(OnLanded::Remove), &db, &root, &feat_s, "feat", &commit);
        assert_eq!(
            db.get_unread_notifications().unwrap().len(),
            notices,
            "no double notice"
        );
        assert_eq!(db.list_merge_queue().unwrap(), rows, "hold row unchanged");
        let _ = std::fs::remove_dir_all(&root); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
    }

    #[test]
    fn audit_cleanup_outcome_covers_every_class() {
        for outcome in [
            CleanupOutcome::KeptDirty,
            CleanupOutcome::KeptChanged,
            CleanupOutcome::Refused {
                class: CleanupRefusal::Identity,
                reason: "x".into(),
            },
            CleanupOutcome::Removed {
                branch_deleted: false,
                queue_removed: true,
                discarded_build_state: false,
                discarded_tool_state: false,
                branch_held: false,
                bookkeeping_errors: Vec::new(),
            },
        ] {
            audit_cleanup_outcome("/wt", Some("abc"), &outcome);
        }
    }

    #[test]
    fn landed_remove_does_not_overlap_an_existing_destroy() {
        let _isolation = TestIsolation::new();
        let db = Db::open_memory().unwrap();
        let (root, feat) = repo_with_feat(&db, "rm-claimed");
        let feat_s = feat.to_string_lossy().to_string();
        db.enqueue_merge(&feat_s, "feat", "main").unwrap();
        let commit = record_private_land(&db, &root, &feat_s, "feat");
        let claim = crate::worktree_lifecycle::try_scoped_destroy_path(&feat)
            .expect("test should own the first destroy claim");

        apply_landed(&cfg(OnLanded::Remove), &db, &root, &feat_s, "feat", &commit);

        assert!(feat.is_dir(), "overlapping cleanup must not remove files");
        assert!(
            util::git_ok(
                &root,
                &["rev-parse", "--verify", "--quiet", "refs/heads/feat"]
            ),
            "overlapping cleanup must not delete the branch"
        );
        assert!(
            db.list_merge_queue()
                .unwrap()
                .iter()
                .any(|row| row.worktree == feat_s),
            "refused cleanup retains its retry record"
        );

        drop(claim);
        let _ = std::fs::remove_dir_all(&root); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
        let _ = std::fs::remove_dir_all(&feat); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
    }

    #[test]
    fn generic_landed_or_mismatched_commit_cannot_remove_a_worktree() {
        let _isolation = TestIsolation::new();
        let db = Db::open_memory().unwrap();
        let (root, feat) = repo_with_feat(&db, "unproven");
        let path = feat.to_str().unwrap();
        let commit = record_private_land(&db, &root, path, "feat");
        let before = db.list_merge_queue().unwrap();
        apply(
            &cfg(OnLanded::Remove),
            &db,
            &root,
            path,
            "feat",
            LifecycleEvent::Landed,
        );
        assert!(feat.exists());
        apply_landed(
            &cfg(OnLanded::Remove),
            &db,
            &root,
            path,
            "feat",
            "wrong-commit",
        );
        assert!(feat.exists());
        assert_eq!(db.list_merge_queue().unwrap(), before);
        db.remove_merge_entry(path).unwrap();
        apply_landed(&cfg(OnLanded::Remove), &db, &root, path, "feat", &commit);
        assert!(feat.exists());
        std::fs::remove_dir_all(&feat).unwrap();
        std::fs::remove_dir_all(&root).unwrap();
    }

    // Regression: unattended cleanup must not recursively delete a directory
    // once Git can no longer prove that it is the registered worktree.
    #[test]
    fn landed_remove_keeps_git_unregistered_directory() {
        let _isolation = TestIsolation::new();
        let db = Db::open_memory().unwrap();
        let (root, _feat) = repo_with_feat(&db, "rmfail");
        let root_s = root.to_string_lossy().to_string();
        // A path that is NOT a registered git worktree makes
        // `git worktree remove` fail. Absence of a `.git` marker is not proof
        // that an unattended recursive delete is safe.
        let bogus = root.with_extension("bogus");
        std::fs::create_dir_all(&bogus).unwrap();
        let bogus_s = bogus.to_string_lossy().to_string();
        let fid = db
            .ensure_folder(&root_s, "Merging", "repo", "repo")
            .unwrap();
        db.put_worktree("bogus", &root_s, &bogus_s, "bogus", None, Some(fid))
            .unwrap();
        db.enqueue_merge(&bogus_s, "bogus", "main").unwrap();
        let commit = record_private_land(&db, &root, &bogus_s, "bogus");
        apply_landed(
            &cfg(OnLanded::Remove),
            &db,
            &root,
            &bogus_s,
            "bogus",
            &commit,
        );
        assert!(bogus.exists(), "the unverified directory is retained");
        assert!(
            db.worktrees()
                .unwrap()
                .iter()
                .any(|w| w.worktree == bogus_s),
            "retained directory stays visible in the cache"
        );
        assert!(
            db.list_merge_queue()
                .unwrap()
                .iter()
                .any(|r| r.worktree == bogus_s),
            "unverified cleanup retains its retry record"
        );
        let _ = std::fs::remove_dir_all(&root); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
        let _ = std::fs::remove_dir_all(&bogus); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
    }

    #[test]
    fn landed_detach_removes_worktree_but_keeps_branch() {
        let _isolation = TestIsolation::new();
        let db = Db::open_memory().unwrap();
        let (root, feat) = repo_with_feat(&db, "detach");
        let feat_s = feat.to_string_lossy().to_string();
        let commit = record_private_land(&db, &root, &feat_s, "feat");
        apply_landed(&cfg(OnLanded::Detach), &db, &root, &feat_s, "feat", &commit);
        assert!(!feat.is_dir(), "worktree dir removed");
        assert!(
            util::git_ok(
                &root,
                &["rev-parse", "--verify", "--quiet", "refs/heads/feat"]
            ),
            "branch kept"
        );
        let _ = std::fs::remove_dir_all(&root); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
    }

    #[test]
    fn cleanup_outcome_claim_busy_is_typed() {
        let _isolation = TestIsolation::new();
        let db = Db::open_memory().unwrap();
        let (root, feat) = repo_with_feat(&db, "rm-claim-busy-typed");
        let feat_s = feat.to_string_lossy().to_string();
        let commit = record_private_land(&db, &root, &feat_s, "feat");
        let before = db.list_merge_queue().unwrap();
        let _claim = crate::worktree_lifecycle::try_scoped_destroy_path(&feat)
            .expect("test should own the destroy claim");

        let outcome = remove_landed(&db, &root, &feat_s, "feat", "main", Some(&commit), true);

        assert!(matches!(
            outcome,
            CleanupOutcome::Refused {
                class: CleanupRefusal::ClaimBusy,
                ..
            }
        ));
        assert_eq!(db.list_merge_queue().unwrap(), before);
        assert!(feat.is_dir());
        std::fs::remove_dir_all(&root).unwrap();
        std::fs::remove_dir_all(&feat).unwrap();
    }

    #[expect(
        clippy::disallowed_methods,
        reason = "fixture reads HEAD from its private test repository"
    )]
    fn land_under_claim(
        db: &Db,
        root: &Path,
        worktree: &str,
        branch: &str,
    ) -> (String, thegn_core::store::MergeClaim) {
        let output = cleanup::test_git(root)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        assert!(output.status.success());
        let commit = String::from_utf8(output.stdout).unwrap().trim().to_owned();
        db.enqueue_merge(worktree, branch, "main").unwrap();
        let claim = db
            .admit_merge_claim(&thegn_core::store::MergeClaimRequest {
                worktree: worktree.into(),
                branch: branch.into(),
                repo_root: root.to_string_lossy().into_owned(),
                target_branch: "main".into(),
                location: String::new(),
                expected_status: "queued".into(),
                lease_until_ms: util::now_ms() + 60_000,
                owner_hint: Some("other-drain".into()),
            })
            .unwrap()
            .expect("claim admitted for queued fixture row");
        let claim = db
            .land_merge_status_claimed(
                &claim,
                &thegn_core::store::MergeStatusFields {
                    result_oid: Some(commit.clone()),
                    ..Default::default()
                },
                &commit,
            )
            .unwrap();
        (commit, claim)
    }

    #[test]
    fn landed_remove_skips_a_row_another_drain_holds() {
        let _isolation = TestIsolation::new();
        let db = Db::open_memory().unwrap();
        let (root, feat) = repo_with_feat(&db, "rm-merge-claimed");
        let feat_s = feat.to_string_lossy().to_string();
        let (commit, claim) = land_under_claim(&db, &root, &feat_s, "feat");
        let before = db.list_merge_queue().unwrap();

        let notes =
            apply_landed_reported(&cfg(OnLanded::Remove), &db, &root, &feat_s, "feat", &commit);

        assert!(
            notes
                .iter()
                .any(|note| note.text().contains("merge claim held"))
        );
        assert!(feat.is_dir());
        assert!(util::git_ok(
            &root,
            &["rev-parse", "--verify", "--quiet", "refs/heads/feat"]
        ));
        assert_eq!(db.list_merge_queue().unwrap(), before);

        let settled = db.settle_merge_claim_no_child(&claim).unwrap();
        db.release_merge_claim(&settled).unwrap();
        let retry_notes =
            apply_landed_reported(&cfg(OnLanded::Remove), &db, &root, &feat_s, "feat", &commit);
        assert!(!retry_notes.is_empty());
        assert!(!feat.is_dir());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn cleanup_outcome_merge_claimed_is_typed() {
        let _isolation = TestIsolation::new();
        let db = Db::open_memory().unwrap();
        let (root, feat) = repo_with_feat(&db, "rm-merge-claimed-typed");
        let feat_s = feat.to_string_lossy().to_string();
        let (commit, _claim) = land_under_claim(&db, &root, &feat_s, "feat");
        let before = db.list_merge_queue().unwrap();

        let outcome = remove_landed(&db, &root, &feat_s, "feat", "main", Some(&commit), true);

        assert!(matches!(
            outcome,
            CleanupOutcome::Refused {
                class: CleanupRefusal::MergeClaimed,
                ..
            }
        ));
        assert_eq!(db.list_merge_queue().unwrap(), before);
        assert!(feat.is_dir());
        std::fs::remove_dir_all(&root).unwrap();
        std::fs::remove_dir_all(&feat).unwrap();
    }

    #[test]
    fn live_merge_claim_table() {
        let now_ms = 10_000;
        let cells = [
            ("none", None, false),
            (
                "unclaimed",
                Some(thegn_core::store::MergeClaimView {
                    worktree: "/wt".into(),
                    generation: 1,
                    phase: "unclaimed".into(),
                    lease_until_ms: Some(now_ms + 1),
                    owner_hint: None,
                }),
                false,
            ),
            (
                "claimed_no_child live",
                Some(thegn_core::store::MergeClaimView {
                    worktree: "/wt".into(),
                    generation: 1,
                    phase: "claimed_no_child".into(),
                    lease_until_ms: Some(now_ms),
                    owner_hint: None,
                }),
                true,
            ),
            (
                "claimed_no_child expired",
                Some(thegn_core::store::MergeClaimView {
                    worktree: "/wt".into(),
                    generation: 1,
                    phase: "claimed_no_child".into(),
                    lease_until_ms: Some(now_ms - 1),
                    owner_hint: None,
                }),
                false,
            ),
            (
                "claimed_no_child without lease",
                Some(thegn_core::store::MergeClaimView {
                    worktree: "/wt".into(),
                    generation: 1,
                    phase: "claimed_no_child".into(),
                    lease_until_ms: None,
                    owner_hint: None,
                }),
                true,
            ),
            (
                "settled live",
                Some(thegn_core::store::MergeClaimView {
                    worktree: "/wt".into(),
                    generation: 1,
                    phase: "settled".into(),
                    lease_until_ms: Some(now_ms + 1),
                    owner_hint: None,
                }),
                true,
            ),
            (
                "settled expired",
                Some(thegn_core::store::MergeClaimView {
                    worktree: "/wt".into(),
                    generation: 1,
                    phase: "settled".into(),
                    lease_until_ms: Some(now_ms - 1),
                    owner_hint: None,
                }),
                false,
            ),
        ];
        for (cell, view, expected) in cells {
            assert_eq!(
                live_merge_claim(view.as_ref(), now_ms),
                expected,
                "cell: {cell}"
            );
        }
    }

    // A `thegn land` files the worktree into Merged in place: the sidebar folder
    // moves but the dir and branch stay. Under the destructive `remove` arm it
    // STILL files rather than deleting — the leave-in-place contract, since the
    // land is typically scripted from inside the worktree it lands.
    #[test]
    fn land_in_place_files_into_merged_and_keeps_worktree() {
        let _isolation = TestIsolation::new();
        let db = Db::open_memory().unwrap();
        let (root, feat) = repo_with_feat(&db, "lip");
        let (root_s, feat_s) = (
            root.to_string_lossy().to_string(),
            feat.to_string_lossy().to_string(),
        );
        db.enqueue_merge(&feat_s, "feat", "main").unwrap();
        apply(
            &cfg(OnLanded::Remove),
            &db,
            &root,
            &feat_s,
            "feat",
            LifecycleEvent::LandedInPlace,
        );
        assert_eq!(folder_of(&db, &root_s, &feat_s).as_deref(), Some("Merged"));
        assert!(feat.is_dir(), "worktree dir left in place");
        assert!(
            util::git_ok(
                &root,
                &["rev-parse", "--verify", "--quiet", "refs/heads/feat"]
            ),
            "branch kept"
        );
        let _ = std::fs::remove_dir_all(&root); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
        let _ = std::fs::remove_dir_all(&feat); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
    }

    #[test]
    fn checked_land_lifecycle_surfaces_an_unavailable_write() {
        let _isolation = TestIsolation::new();
        let state = tempfile::tempdir().unwrap();
        let path = state.path().join("thegn.db");
        let db = Db::open_at(&path).unwrap();
        let (root, feat) = repo_with_feat(&db, "lip-readonly");
        let feat_s = feat.to_string_lossy().to_string();
        drop(db);

        let read_only = Db::open_read_only_wal_at(&path)
            .unwrap()
            .expect("existing state DB");
        let error =
            apply_landed_in_place_checked(&cfg(OnLanded::Move), &read_only, &root, &feat_s, "feat")
                .expect_err("checked land must not swallow a failed folder write");
        assert!(
            error.to_string().to_ascii_lowercase().contains("readonly"),
            "write failure must remain actionable: {error:#}"
        );

        let _ = std::fs::remove_dir_all(&root); // best-effort: test cleanup
        let _ = std::fs::remove_dir_all(&feat); // best-effort: test cleanup
    }

    // With `on_landed = "off"` a land-in-place clears a stranded lifecycle-folder
    // ("Merging") membership, but a folder the user hand-filed the worktree into
    // is left strictly alone (the same host-side guard the dequeue path uses).
    #[test]
    fn land_in_place_off_unfiles_lifecycle_but_not_user_folder() {
        let _isolation = TestIsolation::new();
        let db = Db::open_memory().unwrap();
        let (root, feat) = repo_with_feat(&db, "lipoff");
        let (root_s, feat_s) = (
            root.to_string_lossy().to_string(),
            feat.to_string_lossy().to_string(),
        );
        let c = cfg(OnLanded::Off);
        // Stranded in "Merging" after an enqueue → cleared by the land-in-place.
        apply(&c, &db, &root, &feat_s, "feat", LifecycleEvent::Enqueued);
        assert_eq!(folder_of(&db, &root_s, &feat_s).as_deref(), Some("Merging"));
        apply(
            &c,
            &db,
            &root,
            &feat_s,
            "feat",
            LifecycleEvent::LandedInPlace,
        );
        assert_eq!(
            folder_of(&db, &root_s, &feat_s),
            None,
            "un-filed from Merging under off"
        );
        // A user folder survives a land-in-place under off.
        let fid = db
            .ensure_folder(&root_s, "My stuff", "repo", "repo")
            .unwrap();
        db.set_worktree_folder(&feat_s, Some(fid)).unwrap();
        apply(
            &c,
            &db,
            &root,
            &feat_s,
            "feat",
            LifecycleEvent::LandedInPlace,
        );
        assert_eq!(
            folder_of(&db, &root_s, &feat_s).as_deref(),
            Some("My stuff"),
            "user folder untouched by land-in-place under off"
        );
        assert!(feat.is_dir(), "worktree dir left in place");
        let _ = std::fs::remove_dir_all(&root); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
        let _ = std::fs::remove_dir_all(&feat); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
    }

    #[test]
    fn failure_files_into_needs_attention() {
        let _isolation = TestIsolation::new();
        let db = Db::open_memory().unwrap();
        let (root, feat) = repo_with_feat(&db, "fail");
        let (root_s, feat_s) = (
            root.to_string_lossy().to_string(),
            feat.to_string_lossy().to_string(),
        );
        apply(
            &cfg(OnLanded::Move),
            &db,
            &root,
            &feat_s,
            "feat",
            LifecycleEvent::Failed,
        );
        assert_eq!(
            folder_of(&db, &root_s, &feat_s).as_deref(),
            Some("Needs attention")
        );
        let _ = std::fs::remove_dir_all(&root); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
        let _ = std::fs::remove_dir_all(&feat); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
    }

    // A plain dequeue (`merge rm`/`clear`, and now a successful `thegn land`)
    // pulls the worktree back out of the lifecycle folder its enqueue filed it
    // into — the fix for worktrees stranded in "Merging" after a fold-actor land.
    #[test]
    fn dequeue_unfiles_from_lifecycle_folder() {
        let _isolation = TestIsolation::new();
        let db = Db::open_memory().unwrap();
        let (root, feat) = repo_with_feat(&db, "deq");
        let (root_s, feat_s) = (
            root.to_string_lossy().to_string(),
            feat.to_string_lossy().to_string(),
        );
        apply(
            &cfg(OnLanded::Move),
            &db,
            &root,
            &feat_s,
            "feat",
            LifecycleEvent::Enqueued,
        );
        assert_eq!(folder_of(&db, &root_s, &feat_s).as_deref(), Some("Merging"));
        // The branch is unused by the un-file, so an empty branch (what `thegn
        // land` / `dequeue_worktree` pass) is fine.
        apply(
            &cfg(OnLanded::Move),
            &db,
            &root,
            &feat_s,
            "",
            LifecycleEvent::Dequeued,
        );
        assert_eq!(folder_of(&db, &root_s, &feat_s), None, "un-filed to root");
        let _ = std::fs::remove_dir_all(&root); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
        let _ = std::fs::remove_dir_all(&feat); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
    }

    // The un-file guard: a dequeue must only clear membership of a
    // lifecycle-managed folder (Merging / Needs attention / Merged). A folder the
    // user hand-filed the worktree into is left strictly alone.
    #[test]
    fn dequeue_leaves_user_filed_folder_alone() {
        let _isolation = TestIsolation::new();
        let db = Db::open_memory().unwrap();
        let (root, feat) = repo_with_feat(&db, "dequser");
        let (root_s, feat_s) = (
            root.to_string_lossy().to_string(),
            feat.to_string_lossy().to_string(),
        );
        let fid = db
            .ensure_folder(&root_s, "My stuff", "repo", "repo")
            .unwrap();
        db.set_worktree_folder(&feat_s, Some(fid)).unwrap();
        apply(
            &cfg(OnLanded::Move),
            &db,
            &root,
            &feat_s,
            "",
            LifecycleEvent::Dequeued,
        );
        assert_eq!(
            folder_of(&db, &root_s, &feat_s).as_deref(),
            Some("My stuff"),
            "user folder untouched by dequeue"
        );
        let _ = std::fs::remove_dir_all(&root); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
        let _ = std::fs::remove_dir_all(&feat); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
    }

    #[test]
    fn home_worktree_is_never_touched() {
        let _isolation = TestIsolation::new();
        let db = Db::open_memory().unwrap();
        let (root, feat) = repo_with_feat(&db, "home");
        let root_s = root.to_string_lossy().to_string();
        // worktree == repo_root ⇒ guarded no-op even with an aggressive action.
        apply(
            &cfg(OnLanded::Remove),
            &db,
            &root,
            &root_s,
            "main",
            LifecycleEvent::Landed,
        );
        assert!(root.is_dir(), "main checkout must not be removed");
        let _ = std::fs::remove_dir_all(&root); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
        let _ = std::fs::remove_dir_all(&feat); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
    }

    #[test]
    #[expect(
        clippy::disallowed_methods,
        reason = "read the fixture repository HEAD"
    )]
    fn home_checkout_alias_is_never_filed() {
        let _isolation = TestIsolation::new();
        let db = Db::open_memory().unwrap();
        let (root, feat) = repo_with_feat(&db, "home-alias-filed");
        let root_s = root.to_string_lossy().to_string();
        let alias = root.join("..").join(root.file_name().unwrap());
        let alias_s = alias.to_string_lossy().to_string();

        for event in [LifecycleEvent::Enqueued, LifecycleEvent::Failed] {
            apply(&cfg(OnLanded::Move), &db, &root, &alias_s, "main", event);
            assert!(
                db.worktrees()
                    .unwrap()
                    .iter()
                    .all(|worktree| worktree.worktree != alias_s),
                "home alias must never be registered for {event:?}"
            );
            assert_eq!(folder_of(&db, &root_s, &alias_s), None);
        }

        let output = cleanup::test_git(&root)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        assert!(output.status.success());
        let commit = String::from_utf8(output.stdout).unwrap().trim().to_owned();
        apply_landed(&cfg(OnLanded::Move), &db, &root, &alias_s, "main", &commit);
        assert!(
            db.worktrees()
                .unwrap()
                .iter()
                .all(|worktree| worktree.worktree != alias_s),
            "landed home alias must never be registered"
        );
        assert_eq!(folder_of(&db, &root_s, &alias_s), None);
        assert!(root.is_dir(), "main checkout remains present");

        // best-effort: remove the fixture repository after the assertions.
        if let Err(error) = std::fs::remove_dir_all(&root) {
            tracing::debug!(target: "merge_lifecycle::tests", %error, "fixture root cleanup failed");
        }
        // best-effort: remove the linked worktree after the assertions.
        if let Err(error) = std::fs::remove_dir_all(&feat) {
            tracing::debug!(target: "merge_lifecycle::tests", %error, "fixture worktree cleanup failed");
        }
    }

    #[test]
    fn home_checkout_alias_is_a_no_op_for_land_in_place() {
        let _isolation = TestIsolation::new();
        let db = Db::open_memory().unwrap();
        let (root, feat) = repo_with_feat(&db, "home-alias-land");
        let alias = root.join("..").join(root.file_name().unwrap());
        let alias_s = alias.to_string_lossy().to_string();

        apply_landed_in_place_checked(&cfg(OnLanded::Move), &db, &root, &alias_s, "main").unwrap();
        assert!(
            db.worktrees()
                .unwrap()
                .iter()
                .all(|worktree| worktree.worktree != alias_s),
            "land-in-place must not register a home alias"
        );
        assert!(root.is_dir(), "main checkout remains present");

        // best-effort: remove the fixture repository after the assertions.
        if let Err(error) = std::fs::remove_dir_all(&root) {
            tracing::debug!(target: "merge_lifecycle::tests", %error, "fixture root cleanup failed");
        }
        // best-effort: remove the linked worktree after the assertions.
        if let Err(error) = std::fs::remove_dir_all(&feat) {
            tracing::debug!(target: "merge_lifecycle::tests", %error, "fixture worktree cleanup failed");
        }
    }

    #[test]
    fn home_checkout_unreadable_identity_is_refused() {
        let _isolation = TestIsolation::new();
        let db = Db::open_memory().unwrap();
        let (root, feat) = repo_with_feat(&db, "home-unreadable");
        let bad = format!("{}/bad\0path", root.display());
        let notes = apply_reported(
            &cfg(OnLanded::Move),
            &db,
            &root,
            &bad,
            "feat",
            LifecycleEvent::Enqueued,
        );
        assert_eq!(notes.len(), 1);
        assert!(matches!(
            &notes[0],
            LifecycleNote::Warn(note) if note.contains("worktree identity unreadable")
        ));
        assert!(
            db.worktrees()
                .unwrap()
                .iter()
                .all(|worktree| worktree.worktree != bad)
        );

        let checked = apply_landed_in_place_checked(&cfg(OnLanded::Move), &db, &root, &bad, "feat");
        assert!(matches!(
            checked,
            Err(error) if error.to_string().contains("worktree identity unreadable")
        ));

        // best-effort: remove the fixture repository after the assertions.
        if let Err(error) = std::fs::remove_dir_all(&root) {
            tracing::debug!(target: "merge_lifecycle::tests", %error, "fixture root cleanup failed");
        }
        // best-effort: remove the linked worktree after the assertions.
        if let Err(error) = std::fs::remove_dir_all(&feat) {
            tracing::debug!(target: "merge_lifecycle::tests", %error, "fixture worktree cleanup failed");
        }
    }

    #[test]
    fn home_checkout_real_worktree_is_still_filed() {
        let _isolation = TestIsolation::new();
        let db = Db::open_memory().unwrap();
        let (root, feat) = repo_with_feat(&db, "home-real-worktree");
        let root_s = root.to_string_lossy().to_string();
        let feat_s = feat.to_string_lossy().to_string();

        apply(
            &cfg(OnLanded::Move),
            &db,
            &root,
            &feat_s,
            "feat",
            LifecycleEvent::Enqueued,
        );
        assert_eq!(folder_of(&db, &root_s, &feat_s).as_deref(), Some("Merging"));

        // best-effort: remove the fixture repository after the assertions.
        if let Err(error) = std::fs::remove_dir_all(&root) {
            tracing::debug!(target: "merge_lifecycle::tests", %error, "fixture root cleanup failed");
        }
        // best-effort: remove the linked worktree after the assertions.
        if let Err(error) = std::fs::remove_dir_all(&feat) {
            tracing::debug!(target: "merge_lifecycle::tests", %error, "fixture worktree cleanup failed");
        }
    }

    #[test]
    fn home_checkout_decision_table() {
        let _isolation = TestIsolation::new();
        let db = Db::open_memory().unwrap();
        let (root, feat) = repo_with_feat(&db, "home-decision-table");
        let root_s = root.to_string_lossy().to_string();
        let alias = root.join("..").join(root.file_name().unwrap());
        let missing = root.join("missing");
        let missing_root = root.join("missing-root");
        let bad = format!("{}/bad\0path", root.display());

        assert_eq!(home_checkout(&root_s, &root), Ok(true));
        assert_eq!(home_checkout(&alias.to_string_lossy(), &root), Ok(true));
        assert_eq!(home_checkout(&feat.to_string_lossy(), &root), Ok(false));
        assert_eq!(home_checkout(&missing.to_string_lossy(), &root), Ok(false));
        assert_eq!(home_checkout(&root_s, &missing_root), Ok(false));
        assert!(matches!(
            home_checkout(&bad, &root),
            Err(reason) if reason.contains("worktree identity unreadable")
        ));

        // best-effort: remove the fixture repository after the assertions.
        if let Err(error) = std::fs::remove_dir_all(&root) {
            tracing::debug!(target: "merge_lifecycle::tests", %error, "fixture root cleanup failed");
        }
        // best-effort: remove the linked worktree after the assertions.
        if let Err(error) = std::fs::remove_dir_all(&feat) {
            tracing::debug!(target: "merge_lifecycle::tests", %error, "fixture worktree cleanup failed");
        }
    }

    #[test]
    fn toggle_off_is_inert() {
        let _isolation = TestIsolation::new();
        let db = Db::open_memory().unwrap();
        let (root, feat) = repo_with_feat(&db, "off");
        let (root_s, feat_s) = (
            root.to_string_lossy().to_string(),
            feat.to_string_lossy().to_string(),
        );
        let mut c = cfg(OnLanded::Move);
        c.organize_folders = false;
        apply(&c, &db, &root, &feat_s, "feat", LifecycleEvent::Enqueued);
        assert_eq!(folder_of(&db, &root_s, &feat_s), None);
        let _ = std::fs::remove_dir_all(&root); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
        let _ = std::fs::remove_dir_all(&feat); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
    }

    #[test]
    fn lifecycle_note_refused_cleanup_is_returned() {
        let _isolation = TestIsolation::new();
        let db = Db::open_memory().unwrap();
        let (root, feat) = repo_with_feat(&db, "note-refused");
        let feat_s = feat.to_string_lossy().to_string();
        db.enqueue_merge(&feat_s, "feat", "main").unwrap();
        let commit = record_private_land(&db, &root, &feat_s, "feat");
        let claim = crate::worktree_lifecycle::try_scoped_destroy_path(&feat)
            .expect("test should own the first destroy claim");

        let notes =
            apply_landed_reported(&cfg(OnLanded::Remove), &db, &root, &feat_s, "feat", &commit);

        assert_eq!(
            notes,
            [LifecycleNote::Warn(
                "merge cleanup: kept feat — cleanup already in progress: cleanup is already in progress"
                    .into()
            )]
        );
        drop(claim);
        if let Err(error) = std::fs::remove_dir_all(&root) {
            tracing::debug!(%error, "test cleanup");
        }
        if let Err(error) = std::fs::remove_dir_all(&feat) {
            tracing::debug!(%error, "test cleanup");
        }
    }

    #[test]
    fn lifecycle_note_branch_held_removal_is_returned() {
        let _isolation = TestIsolation::new();
        let db = Db::open_memory().unwrap();
        let (root, feat) = repo_with_feat(&db, "note-branch-held");
        let feat_s = feat.to_string_lossy().to_string();
        let commit = record_private_land(&db, &root, &feat_s, "feat");

        let notes =
            apply_landed_reported(&cfg(OnLanded::Remove), &db, &root, &feat_s, "feat", &commit);

        assert!(notes.contains(&LifecycleNote::Info(
            "merge cleanup: removed feat; branch kept for explicit cleanup".into()
        )));
        if let Err(error) = std::fs::remove_dir_all(&root) {
            tracing::debug!(%error, "test cleanup");
        }
    }

    #[test]
    fn hold_notice_joins_warn_notes_and_skips_info() {
        let notes = [
            LifecycleNote::Info("removed".into()),
            LifecycleNote::Warn("first".into()),
            LifecycleNote::Warn("second".into()),
        ];
        assert_eq!(hold_notice(&notes).as_deref(), Some("first; second"));
    }

    #[test]
    fn hold_notice_none_without_warnings() {
        assert_eq!(hold_notice(&[]), None);
        assert_eq!(hold_notice(&[LifecycleNote::Info("ok".into())]), None);
    }

    #[test]
    fn apply_landed_records_one_queue_held_notice_for_a_kept_worktree() {
        let _isolation = TestIsolation::new();
        let db = Db::open_memory().unwrap();
        let (root, feat) = repo_with_feat(&db, "hold-inbox");
        let feat_s = feat.to_string_lossy().to_string();
        db.enqueue_merge(&feat_s, "feat", "main").unwrap();
        let commit = record_private_land(&db, &root, &feat_s, "feat");
        let claim = crate::worktree_lifecycle::try_scoped_destroy_path(&feat)
            .expect("test should own the first destroy claim");

        apply_landed(&cfg(OnLanded::Remove), &db, &root, &feat_s, "feat", &commit);
        let held = db.get_unread_notifications().unwrap();
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].kind.as_str(), "queue_held");
        assert!(held[0].message.contains("kept feat"));

        apply_landed(&cfg(OnLanded::Remove), &db, &root, &feat_s, "feat", &commit);
        assert_eq!(db.get_unread_notifications().unwrap().len(), 1);

        drop(claim);
        if let Err(error) = std::fs::remove_dir_all(&root) {
            tracing::debug!(%error, "test cleanup");
        }
        if let Err(error) = std::fs::remove_dir_all(&feat) {
            tracing::debug!(%error, "test cleanup");
        }
    }

    #[test]
    fn apply_landed_records_nothing_on_a_clean_removal() {
        let _isolation = TestIsolation::new();
        let db = Db::open_memory().unwrap();
        let (root, feat) = repo_with_feat(&db, "hold-none");
        let feat_s = feat.to_string_lossy().to_string();
        let commit = record_private_land(&db, &root, &feat_s, "feat");

        apply_landed(&cfg(OnLanded::Remove), &db, &root, &feat_s, "feat", &commit);

        assert!(db.get_unread_notifications().unwrap().is_empty());
        if let Err(error) = std::fs::remove_dir_all(&root) {
            tracing::debug!(%error, "test cleanup");
        }
    }

    #[test]
    fn lifecycle_note_disabled_folders_returns_nothing() {
        let _isolation = TestIsolation::new();
        let db = Db::open_memory().unwrap();
        let (root, feat) = repo_with_feat(&db, "note-disabled");
        let feat_s = feat.to_string_lossy().to_string();
        let config = MergeQueueConfig {
            organize_folders: false,
            ..cfg(OnLanded::Remove)
        };

        let notes = apply_landed_reported(&config, &db, &root, &feat_s, "feat", "commit");

        assert!(notes.is_empty());
        if let Err(error) = std::fs::remove_dir_all(&root) {
            tracing::debug!(%error, "test cleanup");
        }
        if let Err(error) = std::fs::remove_dir_all(&feat) {
            tracing::debug!(%error, "test cleanup");
        }
    }
}
