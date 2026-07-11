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
//! handler reaps the orphaned tab via [`reconcile_removed_tabs`] (which runs ON
//! the loop, so it can tear down panes) — nothing in `apply` touches the session.

use std::path::Path;

use thegn_core::config::MergeQueueConfig;
use thegn_core::db::Db;
use thegn_core::merge_lifecycle::{LifecycleAction, LifecycleEvent, decide};
use thegn_core::store::{WorkspaceStore, WorktreeAuxStore};

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
    }
}

/// File `worktree` into the named sidebar folder (find-or-create), scoped to the
/// worktree's own workspace so it lands under the right repo in the tree.
fn file_into(db: &Db, repo_root: &Path, worktree: &str, branch: &str, folder: &str) {
    // `repo_root_for` doubles as the "is this worktree in the DB cache?" probe:
    // it reads the row's `repo_path`, so `None` means there is no row. Prefer
    // that recorded repo_path (the folder is workspace-scoped exactly as the
    // sidebar filters it), falling back to the fold's repo root.
    let recorded = db.repo_root_for(worktree).ok().flatten();
    let repo_path = recorded
        .clone()
        .unwrap_or_else(|| repo_root.to_string_lossy().into_owned());
    // best-effort throughout: sidebar filing is cosmetic and must never fail a merge.
    let Ok(fid) = db.ensure_folder(&repo_path, folder) else {
        return;
    };
    if recorded.is_some() {
        // Row already cached: a narrow update that leaves every other column intact.
        let _ = db.set_worktree_folder(worktree, Some(fid));
    } else {
        // No cache row — the worktree was created via git / the `wt` CLI, not the
        // in-app wizard/provision path that calls `put_worktree`. A bare
        // `set_worktree_folder` would `UPDATE … WHERE worktree=?` and match zero
        // rows, so the filing would silently vanish. Register the row instead,
        // with the canonical `{slug}/{branch}` tab name the sidebar joins live
        // tabs to (`db_by_tab`), so the folder actually shows.
        let slug = thegn_core::repo::repo_slug_with(db, Path::new(&repo_path));
        let tab = thegn_core::repo::branch_tab(&slug, branch);
        let _ = db.put_worktree(&tab, &repo_path, worktree, branch, None, Some(fid));
    }
}

/// Remove a landed worktree (and its branch when `delete_branch`), then drop its
/// cache rows. The branch name comes from the caller (the queue row), not live
/// git — the fold may already have fast-forwarded the branch away.
fn remove_landed(db: &Db, repo_root: &Path, worktree: &str, branch: &str, delete_branch: bool) {
    thegn_core::worktree::remove(repo_root, Path::new(worktree), branch, delete_branch);
    // best-effort cache cleanup: git is the source of truth.
    let _ = db.remove_merge_entry(worktree);
    let _ = db.del_worktree(worktree);
}

/// After an in-app fold, tear down any live tab whose worktree dir was just
/// removed by an `on_landed = remove/detach` land — panes + session group +
/// focus — exactly as a manual close does (`delete_groups`). That primitive
/// never deletes the branch, so `detach` (keep-branch) is preserved. Remote
/// worktrees (a non-local path) are exempt. Returns whether anything was torn
/// down. Runs ON the loop; cheap (one `is_dir` stat per group) and the caller
/// gates it to real fold completions, so it never touches the idle path.
pub(crate) fn reconcile_removed_tabs(
    session: &mut crate::session::Session,
    panes: &mut crate::panes::Panes,
    waker: &termwiz::terminal::TerminalWaker,
) -> bool {
    let remote: std::collections::HashSet<String> = Db::open()
        .ok()
        .and_then(|db| db.worktrees().ok())
        .map(|rows| {
            rows.into_iter()
                .filter(|w| !w.location.is_empty())
                .map(|w| w.worktree)
                .collect()
        })
        .unwrap_or_default();
    let gone: Vec<usize> = session
        .worktrees
        .iter()
        .enumerate()
        .filter(|(_, g)| {
            !g.path.is_empty() && !remote.contains(&g.path) && !Path::new(&g.path).is_dir()
        })
        .map(|(i, _)| i)
        .collect();
    if gone.is_empty() {
        return false;
    }
    let _ = crate::run::delete_groups(session, panes, gone, false, Some(waker.clone()));
    true
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
            "sz-mlife-{tag}-{}-{}",
            std::process::id(),
            util::now()
        ));
        let feat = root.with_extension("feat");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&feat);
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
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&feat);
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
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&feat);
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
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&feat);
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
        let _ = std::fs::remove_dir_all(&root);
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
        let _ = std::fs::remove_dir_all(&root);
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
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&feat);
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
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&feat);
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
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&feat);
    }
}
