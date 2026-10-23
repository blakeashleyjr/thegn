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
use std::collections::HashMap;
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
            Self::Unsafe => "unsafe or unverified checkout",
            Self::Remote => "remote worktree",
            Self::Resources => "attached runtime resources",
            Self::Removal => "removal failed",
        }
    }
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
static REMOTE_GROUPS: OnceLock<Mutex<HashMap<String, u64>>> = OnceLock::new();

struct ReconciliationCompletionGuard;

impl Drop for ReconciliationCompletionGuard {
    fn drop(&mut self) {
        RECONCILIATION_IN_FLIGHT.store(false, Ordering::Release);
    }
}

fn sample_vanished_tabs(
    groups: &[VanishedGroup],
    is_dir: impl Fn(&str) -> bool,
    load_registry: impl FnOnce() -> anyhow::Result<(
        Vec<thegn_core::models::WorktreeRow>,
        Vec<thegn_core::models::WorkspaceRow>,
    )>,
    remote_groups: &Mutex<HashMap<String, u64>>,
) -> anyhow::Result<Vec<String>> {
    let current = groups
        .iter()
        .map(|group| (group.path.as_str(), group.epoch))
        .collect::<std::collections::HashSet<_>>();
    // RECONCILIATION_IN_FLIGHT admits only one sampler, so holding this memo
    // lock across the registry load cannot block another concurrent sampler.
    let mut remote = remote_groups
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    remote.retain(|path, epoch| current.contains(&(path.as_str(), *epoch)));
    let missing: Vec<&VanishedGroup> = groups
        .iter()
        .filter(|group| remote.get(&group.path) != Some(&group.epoch))
        .filter(|group| Path::new(&group.path).is_absolute() && !is_dir(&group.path))
        .collect();
    if missing.is_empty() {
        return Ok(Vec::new());
    }
    let (rows, workspaces) = load_registry()?;
    let mut gone = Vec::new();
    let mut unregistered_count = 0;
    let mut first_unregistered_path = None;
    for group in missing {
        let path = &group.path;
        let is_workspace_root = workspaces
            .iter()
            .any(|workspace| workspace.repo_path == *path);
        let is_local_worktree = match rows.iter().find(|row| row.worktree == *path) {
            Some(row) if row.location.is_empty() || row.location == "local" => true,
            Some(_) => false,
            None if is_workspace_root => true,
            None => {
                unregistered_count += 1;
                if first_unregistered_path.is_none() {
                    first_unregistered_path = Some(path.as_str());
                }
                continue;
            }
        };
        if !is_local_worktree {
            tracing::debug!(path, "skipping vanished-tab probe for remote worktree");
            remote.insert(path.clone(), group.epoch);
        } else {
            gone.push(path.clone());
        }
    }
    if let Some(first_path) = first_unregistered_path {
        tracing::info!(
            count = unregistered_count,
            first_path,
            "skipped vanished-tab probes for unregistered paths"
        );
    }
    Ok(gone)
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
    for note in apply_landed_reported(cfg, db, repo_root, worktree, branch, commit) {
        note.emit();
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
    let selected = match db.list_merge_queue() {
        Ok(rows) => rows.into_iter().find(|row| row.worktree == worktree),
        Err(error) => {
            return CleanupOutcome::Refused {
                class: CleanupRefusal::Database,
                reason: format!("queue identity unavailable: {error}"),
            };
        }
    };
    let Some(selected) = selected.filter(|row| {
        row.status == "landed"
            && row.branch == branch
            && row.target_branch == target
            && landed.is_some()
            && row.result_oid.as_deref() == landed
    }) else {
        return CleanupOutcome::Refused {
            class: CleanupRefusal::Identity,
            reason: "committed outcome does not match a current landed queue row".into(),
        };
    };
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
            db.remove_merge_entry(worktree)?;
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
    let groups: Vec<VanishedGroup> = session
        .worktrees
        .iter()
        .filter(|g| !g.path.is_empty())
        .map(|g| VanishedGroup {
            path: g.path.clone(),
            epoch: g.epoch,
        })
        .collect();
    let admission = crate::sched::spawn_bg(move || {
        // Install before any worker operation so every exit releases ownership.
        let _completion = ReconciliationCompletionGuard;
        let mut sampled_db = None;
        let remote_groups = REMOTE_GROUPS.get_or_init(|| Mutex::new(HashMap::new()));
        let sample = sample_vanished_tabs(
            &groups,
            |path| Path::new(path).is_dir(),
            || {
                let db = Db::open()?;
                let rows = db.worktrees()?;
                let workspaces = db.workspaces()?;
                sampled_db = Some(db);
                Ok((rows, workspaces))
            },
            remote_groups,
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
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use thegn_core::config::OnLanded;
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
            |_| false,
            || anyhow::bail!("injected database open failure"),
            &remote,
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
                |_| false,
                || {
                    loads.set(loads.get() + 1);
                    Ok((Vec::new(), Vec::new()))
                },
                &remote,
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
            |_| false,
            || Ok((db.worktrees()?, db.workspaces()?)),
            &remote,
        )
        .unwrap();
        assert!(gone.is_empty());
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
            |_| true,
            || {
                loads.set(loads.get() + 1);
                Ok((Vec::new(), Vec::new()))
            },
            &remote,
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
            |_| false,
            || {
                loads.set(loads.get() + 1);
                Ok((db.worktrees()?, db.workspaces()?))
            },
            &remote,
        )
        .unwrap();
        assert!(first.is_empty());
        assert_eq!(loads.get(), 1);

        let same_group = [sample_group(1)];
        let second = sample_vanished_tabs(
            &same_group,
            |_| false,
            || {
                loads.set(loads.get() + 1);
                Ok((db.worktrees()?, db.workspaces()?))
            },
            &remote,
        )
        .unwrap();
        assert!(second.is_empty());
        assert_eq!(loads.get(), 1);

        let new_epoch = [sample_group(2)];
        let third = sample_vanished_tabs(
            &new_epoch,
            |_| false,
            || {
                loads.set(loads.get() + 1);
                Ok((db.worktrees()?, db.workspaces()?))
            },
            &remote,
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
            |path| Path::new(path).is_dir(),
            || Ok((db.worktrees()?, db.workspaces()?)),
            &remote,
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
            |path| Path::new(path).is_dir(),
            || Ok((db.worktrees()?, db.workspaces()?)),
            &remote,
        )
        .unwrap();
        assert_eq!(gone, [gone_worktree.to_string_lossy().into_owned()]);
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
        let root = std::env::temp_dir().join(format!(
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
