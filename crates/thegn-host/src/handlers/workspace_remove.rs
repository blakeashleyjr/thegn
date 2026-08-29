//! Workspace removal — the single path behind Alt+Shift+X, the sidebar "Remove
//! workspace" action, and the `DeleteWorkspace` command. Extracted from the
//! pinned `run.rs` (kept flat).
//!
//! Loop-hygiene invariant (audit run.rs:1770): the **destructive** arm deletes
//! worktree directories from disk — `git worktree remove` subprocesses, an ssh
//! remote-dir removal, and a recursive `remove_dir_all` (a Rust worktree with a
//! `target/` is easily multi-GB → tens of seconds). That work MUST NOT run on
//! the event loop, so it is handed to a spawned thread that pulses the
//! `TerminalWaker` when done — mirroring [`crate::run::delete_groups`]. Only the
//! in-memory session prune + DB-row pruning (cheap, cache writes) stays on the
//! loop, and the status line reports "removing N worktrees…" until the thread
//! finishes.

use std::path::Path;

use thegn_core::store::WorkspaceStore;

use crate::chrome::FrameModel;
use crate::panes::Panes;
use crate::run::{SIDEBAR_SCOPE, forget_worktree_group, now_secs};

/// Worktree directories to delete from disk when removing the workspace at
/// `repo_path`: every registered worktree whose `repo_root` matches, EXCEPT the
/// home checkout (its path == `repo_path`, which must never be deleted) and any
/// empty-path legacy rows. Split out so the safety-critical home-skip guard is
/// unit-testable without real I/O.
pub(crate) fn workspace_worktree_dirs(db: &thegn_core::db::Db, repo_path: &str) -> Vec<String> {
    db.worktrees()
        .map(|rows| {
            rows.into_iter()
                .filter(|w| {
                    w.repo_root == repo_path && w.worktree != repo_path && !w.worktree.is_empty()
                })
                .map(|w| w.worktree)
                .collect()
        })
        .unwrap_or_default()
}

/// Delete a workspace's worktree directories from disk OFF the event loop.
/// Runs `git worktree remove` + `purge_worktree_files` (local + remote ssh) per
/// dir on a dedicated thread, then pulses `waker` so the loop repaints. Callers
/// keep the in-memory/DB prune on the loop and show a pending status until this
/// finishes. No-op (no thread spawned) when `dirs` is empty.
pub(crate) fn spawn_delete_workspace_dirs(
    repo_path: &str,
    dirs: Vec<String>,
    waker: Option<termwiz::terminal::TerminalWaker>,
) {
    if dirs.is_empty() {
        return;
    }
    let root = repo_path.to_string();
    std::thread::spawn(move || {
        let root = Path::new(&root);
        let cfg =
            thegn_core::config::Config::load_layered(&thegn_core::config::ProcessEnv, &[], None);
        let workspace = thegn_core::repo::repo_slug(root);
        for path in &dirs {
            let branch = thegn_core::util::git_out(
                Path::new(path),
                &["symbolic-ref", "--quiet", "--short", "HEAD"],
            )
            .unwrap_or_default();
            let pre = crate::worktree_lifecycle::run_event(
                &cfg,
                root,
                Path::new(path),
                &branch,
                &workspace,
                thegn_core::hooks::HookEvent::PreDestroy,
                thegn_core::hooks::HookExecutionMode::Force,
            );
            if !pre.results.is_empty() && pre.results.iter().any(|r| !r.succeeded()) {
                thegn_core::msg::warn(&format!("workspace cleanup {path}: {}", pre.message()));
            }
            // git is the source of truth; both calls are idempotent and
            // best-effort — a failure only leaves a dir that re-adopts on the
            // next launch, never corrupts state.
            thegn_core::worktree::remove(root, Path::new(path), "", false);
            thegn_core::worktree::purge_worktree_files(Path::new(path));
            if !Path::new(path).exists() {
                let post = crate::worktree_lifecycle::run_event(
                    &cfg,
                    root,
                    Path::new(path),
                    &branch,
                    &workspace,
                    thegn_core::hooks::HookEvent::PostDestroy,
                    thegn_core::hooks::HookExecutionMode::Force,
                );
                if !post.results.is_empty() && post.results.iter().any(|r| !r.succeeded()) {
                    thegn_core::msg::warn(&format!("workspace cleanup {path}: {}", post.message()));
                }
            }
        }
        if let Some(waker) = waker {
            let _ = waker.wake(); // best-effort: waker pulse: an input nudge must never fail the calling path
        }
    });
}

/// Remove a workspace — the single path behind both Alt+Shift+X and the sidebar
/// "Remove workspace" action. Always closes every live worktree group the
/// workspace owns and prunes its DB rows (`workspaces`, the `worktrees`
/// registry, its slug, and the active-workspace pointer). When `keep_files` is
/// false it *also* deletes the workspace's worktree directories from disk (the
/// home checkout at `repo_path` is always preserved, off the loop); when true
/// the files stay on disk and the workspace re-appears if reopened. If the
/// removed workspace is the active one, the session switches to the next
/// available workspace (or empties when none remain).
///
/// The destructive disk-removal is dispatched to a background thread (see
/// [`spawn_delete_workspace_dirs`]) so the compositor never blocks on git/ssh/fs.
pub(crate) fn remove_workspace(
    session: &mut crate::session::Session,
    panes: &mut Panes,
    repo_path: &str,
    slug: &str,
    display: &str,
    keep_files: bool,
    waker: Option<termwiz::terminal::TerminalWaker>,
) -> String {
    let db = thegn_core::db::Db::open().ok(); // best-effort: cache: removal proceeds on disk/git; a failed open just leaves stale rows
    let was_active = session.id == repo_path;

    // Read the workspace's branch-worktree dirs from the registry BEFORE
    // `remove_workspace_with_db` prunes it. The home checkout (its path ==
    // `repo_path`) is never included — only branch worktrees. Used either to
    // delete them (destructive) or to report how many survive (keep-files).
    let worktree_dirs = db
        .as_ref()
        .map(|db| workspace_worktree_dirs(db, repo_path))
        .unwrap_or_default();

    // Destructive: delete the workspace's worktree dirs from disk — OFF the
    // event loop (git subprocess + ssh remote rm + recursive delete can take
    // seconds to minutes). The DB/session prune below runs on the loop.
    if !keep_files {
        spawn_delete_workspace_dirs(repo_path, worktree_dirs.clone(), waker);
    }

    remove_workspace_with_db(session, panes, db.as_ref(), repo_path, slug);

    // Removing the active workspace leaves the session pointing at nothing;
    // land on the next available workspace, else empty out.
    if was_active {
        land_after_workspace_removed(session, db.as_ref());
    }

    workspace_removed_status(display, keep_files, worktree_dirs.len())
}

/// After removing the *active* workspace, land on the first remaining workspace,
/// or empty the session (no dangling context) when none remain. Split out so it
/// can be unit-tested with an injected DB (the parent opens the process DB).
pub(crate) fn land_after_workspace_removed(
    session: &mut crate::session::Session,
    db: Option<&thegn_core::db::Db>,
) {
    let mut switched = false;
    if let Some(db) = db
        && let Ok(workspaces) = db.workspaces()
        && let Some(next) = workspaces.first()
    {
        switched = session.switch_to_workspace(&next.repo_path, db).is_ok();
    }
    if !switched {
        session.id.clear();
        session.worktrees.clear();
        session.active = 0;
    }
}

/// The status line after a workspace removal. Non-destructive (`keep_files`)
/// removals report the orphaned-worktree count so the user knows what survived;
/// destructive removals report that the removal is running in the background.
pub(crate) fn workspace_removed_status(
    display: &str,
    keep_files: bool,
    orphan_count: usize,
) -> String {
    if keep_files {
        match orphan_count {
            0 => format!("Removed workspace '{display}' (files kept on disk)"),
            1 => format!("Removed workspace '{display}' (1 worktree remains on disk)"),
            n => format!("Removed workspace '{display}' ({n} worktrees remain on disk)"),
        }
    } else {
        match orphan_count {
            0 => format!("Removed workspace '{display}'"),
            1 => format!("Deleting workspace '{display}' (removing 1 worktree from disk…)"),
            n => format!("Deleting workspace '{display}' (removing {n} worktrees from disk…)"),
        }
    }
}

/// Engine for [`remove_workspace`], split from the process-global `Db::open()`
/// so tests can inject an isolated DB. Closes the workspace's live groups
/// (always, reaping their panes) and — when a `db` is present — prunes every DB
/// trace and persists the trimmed layout. Non-destructive: never touches the
/// worktree files on disk.
pub(crate) fn remove_workspace_with_db(
    session: &mut crate::session::Session,
    panes: &mut Panes,
    db: Option<&thegn_core::db::Db>,
    repo_path: &str,
    slug: &str,
) {
    // Close (forget, never delete from disk) the workspace's live groups,
    // highest index first so earlier indices stay valid as groups are removed.
    let mut targets: Vec<usize> = session
        .worktrees
        .iter()
        .enumerate()
        .filter_map(|(gi, g)| {
            crate::sidebar::split_tab(&g.name)
                .filter(|(repo, _)| repo == slug)
                .map(|_| gi)
        })
        .collect();
    targets.sort_unstable_by(|a, b| b.cmp(a));
    for gi in targets {
        if gi >= session.worktrees.len() {
            continue;
        }
        if let Some(db) = db {
            forget_worktree_group(db, &session.id, &session.worktrees[gi]);
        }
        for tab in &session.worktrees[gi].tabs {
            for id in tab.center.pane_ids() {
                panes.table.remove(&id);
            }
        }
        session.switch_to(gi);
        session.close_active_group();
    }

    if let Some(db) = db {
        // Prune every DB trace so the workspace doesn't re-render or resurrect.
        let _ = db.del_worktrees_for_repo(repo_path); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
        let _ = db.del_workspace(repo_path); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
        let _ = db.del_repo_slug(repo_path); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
        // Tombstone the removal: pruning the rows isn't enough because the home
        // checkout stays on disk (git is truth), so a later cold start standing
        // in this directory would `put_workspace` it back (hydrate / switch).
        // The tombstone makes "remove workspace" stick until an explicit reopen.
        let _ = db.tombstone_workspace(repo_path); // best-effort: cache write: the tombstone drives sidebar resurrection only
        if db.active_workspace().ok().flatten().as_deref() == Some(repo_path) {
            let _ = db.del_ui_state("", "active_workspace"); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
        }
        // …including its sidebar view state: `collapse:{slug}`, `pin:{slug}`,
        // `pin:{slug}/{branch}`, `collapse:{slug}/folder:{id}` would otherwise
        // orphan in ui_state forever. best-effort: cache-only keys.
        //
        // Delete the EXACT key plus the segment-anchored `{slug}/` prefix, never
        // the bare `{slug}` prefix (audit run.rs:1457): a raw `LIKE 'pin:api%'`
        // also wipes an unrelated `pin:api-v2/…` sibling workspace's state.
        del_ui_state_segment(db, SIDEBAR_SCOPE, "collapse", slug);
        del_ui_state_segment(db, SIDEBAR_SCOPE, "pin", slug);
        // Persist the trimmed layout: otherwise `tab_groups`/`group_tabs`
        // resurrect the closed groups on the next launch (see `delete_groups`).
        let _ = session.persist(db, &session.id, now_secs()); // best-effort: cache write: the trimmed layout feed; git/disk removal already reported
    }
}

/// Segment-anchored delete of a `{prefix}:{name}` ui_state key family: removes
/// the exact `{prefix}:{name}` key AND everything under `{prefix}:{name}/` (its
/// per-branch / per-folder children), but NOT sibling names that merely share a
/// leading substring (e.g. `pin:api` must not touch `pin:api-v2/…`). See audit
/// run.rs:1457. All best-effort: these are cache-only sidebar view keys.
pub(crate) fn del_ui_state_segment(db: &thegn_core::db::Db, scope: &str, prefix: &str, name: &str) {
    let _ = db.del_ui_state(scope, &format!("{prefix}:{name}")); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
    let _ = db.del_ui_state_prefix(scope, &format!("{prefix}:{name}/")); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
}

/// Drop a just-removed workspace (and the registered-worktree rows its
/// empty-live-groups branch would re-render) from the cached sidebar lists that
/// `refresh_tab_model` rebuilds from. Without this the row lingers until the
/// next full hydration re-reads the DB, so [`remove_workspace`] appears to do
/// nothing. Kept as a free fn so the prune is exercised by the same code the
/// event loop runs, not a copy.
pub(crate) fn forget_workspace_in_model(model: &mut FrameModel, slug: &str, repo_path: &str) {
    model
        .sidebar_workspaces
        .retain(|(s, _, _, p)| !(s == slug && p == repo_path));
    model.sidebar_db_worktrees.retain(|w| w.slug != slug);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pane::PaneEvent;
    use crate::session::{GroupKind, Session, WorktreeGroup};

    const PANE_EVENT_CHANNEL_CAPACITY: usize = 256;

    #[test]
    fn workspace_worktree_dirs_skips_home_and_other_workspaces() {
        // The destructive "delete from disk" selection MUST exclude the home
        // checkout (path == repo_path — deleting it would nuke the main repo)
        // and any sibling workspace's worktrees. Only this workspace's branch
        // worktree dirs are returned.
        let db_path = std::env::temp_dir().join(format!(
            "tg-host-wtdirs-{}-{}.sqlite",
            std::process::id(),
            now_secs()
        ));
        let db = thegn_core::db::Db::open_at(&db_path).unwrap();
        db.put_worktree(
            "lib/home",
            "/tmp/repo-lib",
            "/tmp/repo-lib",
            "home",
            None,
            None,
        )
        .unwrap();
        db.put_worktree(
            "lib/feat",
            "/tmp/repo-lib",
            "/tmp/repo-lib-feat",
            "feat",
            None,
            None,
        )
        .unwrap();
        db.put_worktree(
            "lib/fix",
            "/tmp/repo-lib",
            "/tmp/repo-lib-fix",
            "fix",
            None,
            None,
        )
        .unwrap();
        db.put_worktree(
            "app/feat",
            "/tmp/repo-app",
            "/tmp/repo-app-feat",
            "feat",
            None,
            None,
        )
        .unwrap();

        let mut dirs = workspace_worktree_dirs(&db, "/tmp/repo-lib");
        dirs.sort();
        assert_eq!(
            dirs,
            vec![
                "/tmp/repo-lib-feat".to_string(),
                "/tmp/repo-lib-fix".to_string()
            ],
            "only this workspace's branch worktrees, never home or siblings"
        );
        let _ = std::fs::remove_file(&db_path); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
    }

    #[test]
    fn remove_workspace_with_db_prunes_db_and_closes_live_groups() {
        // The engine behind "Remove workspace": it must close every live group
        // the workspace owns AND prune its DB rows so the workspace neither
        // renders nor resurrects — while leaving sibling workspaces untouched.
        let db_path = std::env::temp_dir().join(format!(
            "tg-host-remove-ws-{}-{}.sqlite",
            std::process::id(),
            now_secs()
        ));
        let db = thegn_core::db::Db::open_at(&db_path).unwrap();
        db.put_workspace("/tmp/repo-app", "app", "repo").unwrap();
        db.put_workspace("/tmp/repo-lib", "lib", "repo").unwrap();
        db.put_worktree(
            "lib/home",
            "/tmp/repo-lib",
            "/tmp/repo-lib",
            "home",
            None,
            None,
        )
        .unwrap();
        db.put_worktree(
            "lib/feat",
            "/tmp/repo-lib",
            "/tmp/repo-lib-feat",
            "feat",
            None,
            None,
        )
        .unwrap();
        db.put_worktree(
            "app/home",
            "/tmp/repo-app",
            "/tmp/repo-app",
            "home",
            None,
            None,
        )
        .unwrap();
        db.set_active_workspace("/tmp/repo-lib").unwrap();

        let mut session = Session {
            id: "/tmp/repo-lib".into(),
            worktrees: vec![
                WorktreeGroup::new("app/home", GroupKind::Home, "/tmp/repo-app"),
                WorktreeGroup::new("lib/home", GroupKind::Home, "/tmp/repo-lib"),
                WorktreeGroup::new("lib/feat", GroupKind::Branch, "/tmp/repo-lib-feat"),
            ],
            active: 1,
        };
        let (tx, _rx) = tokio::sync::mpsc::channel::<PaneEvent>(PANE_EVENT_CHANNEL_CAPACITY);
        let mut panes = Panes::new(tx);

        remove_workspace_with_db(&mut session, &mut panes, Some(&db), "/tmp/repo-lib", "lib");

        let names: Vec<&str> = session.worktrees.iter().map(|g| g.name.as_str()).collect();
        assert_eq!(names, vec!["app/home"], "all lib groups closed: {names:?}");

        let ws: Vec<String> = db
            .workspaces()
            .unwrap()
            .into_iter()
            .map(|w| w.repo_path)
            .collect();
        assert!(
            ws.contains(&"/tmp/repo-app".to_string()),
            "sibling kept: {ws:?}"
        );
        assert!(
            !ws.contains(&"/tmp/repo-lib".to_string()),
            "removed workspace row pruned: {ws:?}"
        );
        // The removal is tombstoned so a cwd/active-workspace cold start can't
        // resurrect it, while the surviving sibling is left untombstoned.
        assert!(
            db.workspace_tombstoned("/tmp/repo-lib").unwrap(),
            "removed workspace must be tombstoned"
        );
        assert!(
            !db.workspace_tombstoned("/tmp/repo-app").unwrap(),
            "sibling workspace must not be tombstoned"
        );
        let wt_roots: Vec<String> = db
            .worktrees()
            .unwrap()
            .into_iter()
            .map(|w| w.repo_root)
            .collect();
        assert!(
            !wt_roots.iter().any(|p| p == "/tmp/repo-lib"),
            "registry rows pruned: {wt_roots:?}"
        );
        assert!(
            wt_roots.iter().any(|p| p == "/tmp/repo-app"),
            "sibling registry row kept: {wt_roots:?}"
        );
        assert_eq!(db.active_workspace().unwrap(), None);
        let _ = std::fs::remove_file(&db_path); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
    }

    #[test]
    fn remove_workspace_reports_status() {
        // Keep-files reports survivors; destructive reports the background job.
        assert_eq!(
            workspace_removed_status("lib", true, 0),
            "Removed workspace 'lib' (files kept on disk)"
        );
        assert_eq!(
            workspace_removed_status("lib", true, 1),
            "Removed workspace 'lib' (1 worktree remains on disk)"
        );
        assert_eq!(
            workspace_removed_status("lib", true, 3),
            "Removed workspace 'lib' (3 worktrees remain on disk)"
        );
        assert_eq!(
            workspace_removed_status("lib", false, 0),
            "Removed workspace 'lib'"
        );
        assert_eq!(
            workspace_removed_status("lib", false, 3),
            "Deleting workspace 'lib' (removing 3 worktrees from disk…)"
        );
    }

    #[test]
    fn delete_last_workspace_empties_session() {
        // Removing the active (and only) workspace must fall back to an empty
        // home rather than leave the session pointing at a pruned workspace.
        let db_path = std::env::temp_dir().join(format!(
            "tg-host-last-ws-{}-{}.sqlite",
            std::process::id(),
            now_secs()
        ));
        let db = thegn_core::db::Db::open_at(&db_path).unwrap();
        let mut session = Session {
            id: "/tmp/repo-lib".into(),
            worktrees: vec![WorktreeGroup::new(
                "lib/home",
                GroupKind::Home,
                "/tmp/repo-lib",
            )],
            active: 0,
        };
        land_after_workspace_removed(&mut session, Some(&db));
        assert!(session.id.is_empty(), "session id cleared");
        assert!(session.worktrees.is_empty(), "no groups remain");
        assert_eq!(session.active, 0);
        let _ = std::fs::remove_file(&db_path); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
    }

    #[test]
    fn del_ui_state_segment_is_prefix_anchored() {
        // audit run.rs:1457: deleting workspace `api` must NOT wipe `api-v2`.
        let db_path = std::env::temp_dir().join(format!(
            "tg-host-uistate-seg-{}-{}.sqlite",
            std::process::id(),
            now_secs()
        ));
        let db = thegn_core::db::Db::open_at(&db_path).unwrap();
        // Target workspace `api`: exact + child keys.
        db.set_ui_state(SIDEBAR_SCOPE, "pin:api", "1").unwrap();
        db.set_ui_state(SIDEBAR_SCOPE, "pin:api/main", "1").unwrap();
        db.set_ui_state(SIDEBAR_SCOPE, "pin:api/main/folder:3", "1")
            .unwrap();
        // Sibling `api-v2`: must survive.
        db.set_ui_state(SIDEBAR_SCOPE, "pin:api-v2", "1").unwrap();
        db.set_ui_state(SIDEBAR_SCOPE, "pin:api-v2/main", "1")
            .unwrap();

        del_ui_state_segment(&db, SIDEBAR_SCOPE, "pin", "api");

        assert_eq!(db.get_ui_state(SIDEBAR_SCOPE, "pin:api").unwrap(), None);
        assert_eq!(
            db.get_ui_state(SIDEBAR_SCOPE, "pin:api/main").unwrap(),
            None
        );
        assert_eq!(
            db.get_ui_state(SIDEBAR_SCOPE, "pin:api/main/folder:3")
                .unwrap(),
            None
        );
        assert_eq!(
            db.get_ui_state(SIDEBAR_SCOPE, "pin:api-v2").unwrap(),
            Some("1".to_string()),
            "sibling workspace pin must survive"
        );
        assert_eq!(
            db.get_ui_state(SIDEBAR_SCOPE, "pin:api-v2/main").unwrap(),
            Some("1".to_string()),
            "sibling workspace child pin must survive"
        );
        let _ = std::fs::remove_file(&db_path); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
    }
}
