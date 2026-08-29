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

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use thegn_core::config::MergeQueueConfig;
use thegn_core::db::Db;
use thegn_core::merge_lifecycle::{LifecycleAction, LifecycleEvent, decide};
use thegn_core::store::{WorkspaceStore, WorktreeAuxStore};

/// Paths identified by the off-loop vanished-tab probe. The compositor applies
/// this result without re-checking disk or SQLite.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VanishedTabs {
    pub paths: Vec<String>,
}

static RECONCILIATION_IN_FLIGHT: AtomicBool = AtomicBool::new(false);

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
    // The home / main checkout is a fixed anchor — never file or remove it.
    if Path::new(worktree) == repo_root {
        return;
    }
    match decide(cfg, event) {
        LifecycleAction::Noop => {}
        LifecycleAction::FileInto(folder) => file_into(db, repo_root, worktree, branch, &folder),
        LifecycleAction::RemoveWorktree { delete_branch } => {
            remove_landed(db, repo_root, worktree, branch, delete_branch)
        }
        LifecycleAction::Unfile => unfile(cfg, db, repo_root, worktree),
    }
}

/// Un-file a worktree from its lifecycle folder (Merging / Needs attention /
/// Merged) back to the ungrouped repo root — the cleanup a plain dequeue
/// (`merge rm` / `merge clear`) needs, since it neither lands nor fails. Guarded
/// so it only clears membership of a folder the lifecycle itself manages: if the
/// user has since hand-filed the worktree into a folder of their own, it is left
/// alone. Best-effort — sidebar bookkeeping must never fail a queue mutation.
fn unfile(cfg: &MergeQueueConfig, db: &Db, repo_root: &Path, worktree: &str) {
    // The worktree's current folder, if any. No row / no folder ⇒ nothing to do.
    let Some(fid) = db.worktrees().ok().and_then(|rows| {
        rows.into_iter()
            .find(|w| w.worktree == worktree)
            .and_then(|w| w.folder_id)
    }) else {
        return;
    };
    // Resolve that folder's name (keyed by the worktree's own workspace string,
    // the same resolution `file_into` uses) and only un-file when it is one the
    // lifecycle manages.
    let recorded = db.repo_root_for(worktree).ok().flatten();
    let repo_path = workspace_repo_path(db, repo_root, recorded.as_deref());
    let name = db
        .folders_for_workspace(&repo_path)
        .ok()
        .and_then(|folders| {
            folders
                .into_iter()
                .find(|f| f.folder_id == fid)
                .map(|f| f.name)
        });
    let is_lifecycle_folder = |n: &str| {
        let n = n.trim();
        !n.is_empty()
            && (n == cfg.queued_folder.trim()
                || n == cfg.failed_folder.trim()
                || n == cfg.merged_folder.trim())
    };
    if name.as_deref().map(is_lifecycle_folder).unwrap_or(false) {
        let _ = db.set_worktree_folder(worktree, None); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
    }
}

/// The exact `workspaces.repo_path` string the sidebar keys folders by, for the
/// repo whose main checkout is `repo_root` (and whose worktree row records
/// `recorded`). A folder MUST be created under this string or the sidebar —
/// which matches `folders.repo_path == workspaces.repo_path` byte-for-byte —
/// won't render it. Prefer an existing `workspaces` row matched exactly (on the
/// recorded path or `repo_root`) then by canonicalized path; fall back to the
/// recorded worktree path, then `repo_root`. Runs off-loop, so the `canonicalize`
/// stat is fine.
fn workspace_repo_path(db: &Db, repo_root: &Path, recorded: Option<&str>) -> String {
    if let Ok(rows) = db.workspaces() {
        let want = std::fs::canonicalize(repo_root).ok(); // best-effort: optional input: a vanished path just fails the match; the caller falls back to the raw root
        if let Some(w) = rows.iter().find(|w| {
            Some(w.repo_path.as_str()) == recorded
                || Path::new(&w.repo_path) == repo_root
                || (want.is_some() && std::fs::canonicalize(&w.repo_path).ok() == want)
        }) {
            return w.repo_path.clone();
        }
    }
    recorded
        .map(str::to_owned)
        .unwrap_or_else(|| repo_root.to_string_lossy().into_owned())
}

/// File `worktree` into the named sidebar folder (find-or-create), scoped to the
/// worktree's own workspace so it lands under the right repo in the tree.
fn file_into(db: &Db, repo_root: &Path, worktree: &str, branch: &str, folder: &str) {
    // `repo_root_for` doubles as the "is this worktree in the DB cache?" probe:
    // it reads the row's `repo_path`, so `None` means there is no row.
    let recorded = db.repo_root_for(worktree).ok().flatten();
    // File under the SAME `repo_path` string the sidebar keys folders by. The
    // sidebar renders a folder only when `folders.repo_path == workspaces.repo_path`
    // byte-for-byte (see `hydrate::workspace_list` + `sidebar::build_rows`), so a
    // folder created under a divergent string (a worktree row registered by an
    // external tool, a trailing slash, a symlinked path) yields no header and the
    // filed worktree is orphaned. Resolve to the workspace's own string.
    let repo_path = workspace_repo_path(db, repo_root, recorded.as_deref());
    // best-effort throughout: sidebar filing is cosmetic and must never fail a merge.
    let Ok(fid) = db.ensure_folder(&repo_path, folder) else {
        return;
    };
    if recorded.is_some() {
        // Row already cached: a narrow update that leaves every other column intact.
        let _ = db.set_worktree_folder(worktree, Some(fid)); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
    } else {
        // No cache row — the worktree was created via git / the `wt` CLI, not the
        // in-app wizard/provision path that calls `put_worktree`. A bare
        // `set_worktree_folder` would `UPDATE … WHERE worktree=?` and match zero
        // rows, so the filing would silently vanish. Register the row instead,
        // with the canonical `{slug}/{branch}` tab name the sidebar joins live
        // tabs to (`db_by_tab`), so the folder actually shows.
        let slug = thegn_core::repo::repo_slug_with(db, Path::new(&repo_path));
        let tab = thegn_core::repo::branch_tab(&slug, branch);
        let _ = db.put_worktree(&tab, &repo_path, worktree, branch, None, Some(fid)); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
    }
}

/// Does this worktree have uncommitted work (staged, unstaged, or untracked)?
/// Missing/unreadable is treated as NOT dirty so a stale path still gets cleaned
/// up — `git worktree remove` is the one that decides, and it fails safely.
pub(crate) fn worktree_is_dirty(worktree: &str) -> bool {
    thegn_core::util::git_out(Path::new(worktree), &["status", "--porcelain"])
        .is_some_and(|s| !s.trim().is_empty())
}

/// Remove a landed worktree (and its branch when `delete_branch`), then drop its
/// cache rows. The branch name comes from the caller (the queue row), not live
/// git — the fold may already have fast-forwarded the branch away.
pub(crate) fn remove_landed(
    db: &Db,
    repo_root: &Path,
    worktree: &str,
    branch: &str,
    delete_branch: bool,
) {
    // `worktree::remove` escalates to `git worktree remove --force`, which
    // discards uncommitted work. That is fine for an explicit `wt rm`, but this
    // path is automatic — it fires on a clean land, with no prompt — so a
    // worktree the user still has work in must be left alone. It keeps its
    // lifecycle folder and its branch; the next land (or a manual `wt rm`)
    // cleans it up once the work is committed or dropped.
    if worktree_is_dirty(worktree) {
        thegn_core::msg::warn(&format!(
            "{branch} landed, but {worktree} has uncommitted changes — leaving the worktree and its branch in place"
        ));
        let _ = db.remove_merge_entry(worktree); // best-effort: cache write: the queue row is bookkeeping; the worktree/branch removal below reports the real outcome
        return;
    }
    // Automatic reclaim is unattended: the shared transaction runs the hook,
    // runtime teardown, removal, and post-hook in order, but a repository-
    // authored failure can never wedge the queue.
    let cfg = thegn_core::config::Config::load_layered(&thegn_core::config::ProcessEnv, &[], None);
    let workspace = thegn_core::repo::repo_slug(repo_root);
    let (removed, message) = crate::worktree_lifecycle::destroy_one(
        &cfg,
        repo_root,
        Path::new(worktree),
        branch,
        &workspace,
        false,
        delete_branch,
        thegn_core::hooks::HookExecutionMode::Unattended,
        Some(db),
    );
    if !removed {
        thegn_core::msg::warn(&format!("merge cleanup: {message}"));
        return;
    }
    // The branch landed, so it's no longer a queue entry regardless.
    let _ = db.remove_merge_entry(worktree); // best-effort: cache write: the queue row is bookkeeping; the worktree/branch removal below reports the real outcome
    // Only drop the worktree's cache row (its folder assignment) when the dir
    // actually went away. If removal failed (a read-only sandbox mount, or
    // uncommitted changes), keep the row so the sidebar still files it under its
    // folder instead of orphaning it ungrouped under the repo root ("home").
    // git is the source of truth; the row self-corrects once the dir is gone.
    if removed {
        let _ = db.del_worktree(worktree); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
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
    let paths: Vec<String> = session
        .worktrees
        .iter()
        .filter(|g| !g.path.is_empty())
        .map(|g| g.path.clone())
        .collect();
    let spawned = std::thread::Builder::new()
        .name("thegn-vanished-tab-reconcile".into())
        .spawn(move || {
            crate::platform::qos::set_self(crate::platform::qos::Qos::Background);
            let db = Db::open().ok();
            let remote: std::collections::HashSet<String> = db
                .as_ref()
                .and_then(|db| db.worktrees().ok())
                .map(|rows| {
                    rows.into_iter()
                        .filter(|w| !w.location.is_empty())
                        .map(|w| w.worktree)
                        .collect()
                })
                .unwrap_or_default();
            let gone: Vec<String> = paths
                .into_iter()
                .filter(|path| !remote.contains(path) && !Path::new(path).is_dir())
                .collect();
            if let Some(db) = db {
                for path in &gone {
                    let _ = db.del_worktree(path); // best-effort: git is the source of truth
                }
            }
            RECONCILIATION_IN_FLIGHT.store(false, Ordering::Release);
            if !gone.is_empty() {
                crate::worktree_lifecycle::send_refresh(
                    crate::hydrate::RefreshKind::VanishedTabs(Box::new(VanishedTabs {
                        paths: gone,
                    })),
                    waker,
                );
            }
        })
        .is_ok();
    if !spawned {
        RECONCILIATION_IN_FLIGHT.store(false, Ordering::Release);
    }
}

/// Apply an off-loop vanished-tab result. This is intentionally pure with
/// respect to the filesystem and SQLite: it only removes pane/session objects,
/// queues the resulting layout cache write, and restores focus by group name.
pub(crate) fn apply_vanished_tabs(
    session: &mut crate::session::Session,
    panes: &mut crate::panes::Panes,
    paths: &[String],
) -> bool {
    let gone = vanished_group_indices(session, paths);
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
    if let Some(name) = prior
        && let Some(idx) = session.worktrees.iter().position(|g| g.name == name)
    {
        session.switch_to(idx);
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
fn vanished_group_indices(session: &crate::session::Session, paths: &[String]) -> Vec<usize> {
    session
        .worktrees
        .iter()
        .enumerate()
        .filter(|(_, g)| paths.iter().any(|path| path == &g.path))
        .map(|(i, _)| i)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use thegn_core::config::OnLanded;
    use thegn_core::util;

    // test code: fixture plumbing, never on the event loop.
    #[expect(clippy::disallowed_methods)]
    fn git(dir: &Path, args: &[&str]) {
        let ok = util::git_cmd(dir)
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
        let got = workspace_repo_path(&db, Path::new("/repos/app"), Some("/repos/app/"));
        assert_eq!(got, "/repos/app");
    }

    #[test]
    fn workspace_repo_path_falls_back_when_no_workspace_row() {
        let db = Db::open_memory().unwrap();
        // No workspace registered: fall back to the recorded path, else repo_root.
        assert_eq!(
            workspace_repo_path(&db, Path::new("/repos/none"), Some("/repos/rec")),
            "/repos/rec"
        );
        assert_eq!(
            workspace_repo_path(&db, Path::new("/repos/none"), None),
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
            vanished_group_indices(&session, &["/repo-feature".into()]),
            vec![1]
        );
        assert!(vanished_group_indices(&session, &[]).is_empty());
    }

    #[test]
    fn enqueue_files_into_merging_folder() {
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
    fn enqueue_registers_unpersisted_worktree_and_files_it() {
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
    fn landed_remove_deletes_worktree_branch_and_row() {
        let db = Db::open_memory().unwrap();
        let (root, feat) = repo_with_feat(&db, "rm");
        let feat_s = feat.to_string_lossy().to_string();
        db.enqueue_merge(&feat_s, "feat", "main").unwrap();
        apply(
            &cfg(OnLanded::Remove),
            &db,
            &root,
            &feat_s,
            "feat",
            LifecycleEvent::Landed,
        );
        assert!(!feat.is_dir(), "worktree dir removed");
        assert!(
            !util::git_ok(
                &root,
                &["rev-parse", "--verify", "--quiet", "refs/heads/feat"]
            ),
            "branch deleted"
        );
        assert!(
            db.list_merge_queue()
                .unwrap()
                .iter()
                .all(|r| r.worktree != feat_s)
        );
        let _ = std::fs::remove_dir_all(&root); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
    }

    // Regression: when the worktree can't be removed (read-only sandbox mount,
    // uncommitted changes) the cache row must be KEPT so the sidebar keeps it
    // filed, instead of dropping the row and orphaning it under "home".
    #[test]
    fn landed_remove_keeps_row_when_removal_fails() {
        let db = Db::open_memory().unwrap();
        let (root, _feat) = repo_with_feat(&db, "rmfail");
        let root_s = root.to_string_lossy().to_string();
        // A path that is NOT a registered git worktree ⇒ `git worktree remove`
        // fails (stands in for the read-only-mount removal failure).
        let bogus = root.with_extension("bogus");
        std::fs::create_dir_all(&bogus).unwrap();
        let bogus_s = bogus.to_string_lossy().to_string();
        let fid = db.ensure_folder(&root_s, "Merging").unwrap();
        db.put_worktree("bogus", &root_s, &bogus_s, "bogus", None, Some(fid))
            .unwrap();
        db.enqueue_merge(&bogus_s, "bogus", "main").unwrap();
        apply(
            &cfg(OnLanded::Remove),
            &db,
            &root,
            &bogus_s,
            "bogus",
            LifecycleEvent::Landed,
        );
        assert!(
            db.worktrees()
                .unwrap()
                .iter()
                .any(|w| w.worktree == bogus_s),
            "worktree row kept when removal failed (not orphaned)"
        );
        assert!(
            db.list_merge_queue()
                .unwrap()
                .iter()
                .all(|r| r.worktree != bogus_s),
            "queue entry still cleared"
        );
        let _ = std::fs::remove_dir_all(&root); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
        let _ = std::fs::remove_dir_all(&bogus); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
    }

    #[test]
    fn landed_detach_removes_worktree_but_keeps_branch() {
        let db = Db::open_memory().unwrap();
        let (root, feat) = repo_with_feat(&db, "detach");
        let feat_s = feat.to_string_lossy().to_string();
        apply(
            &cfg(OnLanded::Detach),
            &db,
            &root,
            &feat_s,
            "feat",
            LifecycleEvent::Landed,
        );
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

    // A `thegn land` files the worktree into Merged in place: the sidebar folder
    // moves but the dir and branch stay. Under the destructive `remove` arm it
    // STILL files rather than deleting — the leave-in-place contract, since the
    // land is typically scripted from inside the worktree it lands.
    #[test]
    fn land_in_place_files_into_merged_and_keeps_worktree() {
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

    // With `on_landed = "off"` a land-in-place clears a stranded lifecycle-folder
    // ("Merging") membership, but a folder the user hand-filed the worktree into
    // is left strictly alone (the same host-side guard the dequeue path uses).
    #[test]
    fn land_in_place_off_unfiles_lifecycle_but_not_user_folder() {
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
        let fid = db.ensure_folder(&root_s, "My stuff").unwrap();
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
        let db = Db::open_memory().unwrap();
        let (root, feat) = repo_with_feat(&db, "dequser");
        let (root_s, feat_s) = (
            root.to_string_lossy().to_string(),
            feat.to_string_lossy().to_string(),
        );
        let fid = db.ensure_folder(&root_s, "My stuff").unwrap();
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
    fn toggle_off_is_inert() {
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
}
