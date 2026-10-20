//! Workspace removal — the single path behind Alt+Shift+X, the sidebar "Remove
//! workspace" action, and the `DeleteWorkspace` command. Extracted from the
//! pinned `run.rs` (kept flat).
//!
//! Loop-hygiene invariant (audit run.rs:1770): the **destructive** arm deletes
//! worktree directories from disk — `git worktree remove` subprocesses, an ssh
//! remote-dir removal, and a recursive `remove_dir_all` (a Rust worktree with a
//! `target/` is easily multi-GB → tens of seconds). That work MUST NOT run on
//! the event loop, so it is handed to a spawned thread that pulses the
//! `TerminalWaker` when done — mirroring [`crate::run::delete_groups_with_mode`]. The
//! loop retains live groups until each worker reports success, then only
//! performs the in-memory prune; cache writes stay on the worker.

use std::path::Path;

use thegn_core::store::WorkspaceStore;

use crate::chrome::FrameModel;
use crate::panes::Panes;
use crate::run::SIDEBAR_SCOPE;

#[cfg(test)]
use crate::run::{forget_worktree_group, now_secs};

/// Worktree directories to delete from disk when removing the workspace at
/// `repo_path`: every registered worktree whose `repo_root` matches, EXCEPT the
/// home checkout (its path == `repo_path`, which must never be deleted) and any
/// empty-path legacy rows. Split out so the safety-critical home-skip guard is
/// unit-testable without real I/O.
pub(crate) fn workspace_worktree_dirs(
    db: &thegn_core::db::Db,
    repo_path: &str,
) -> Result<Vec<String>, String> {
    db.worktrees()
        .map(|rows| {
            rows.into_iter()
                .filter(|w| {
                    w.repo_root == repo_path && w.worktree != repo_path && !w.worktree.is_empty()
                })
                .map(|w| w.worktree)
                .collect()
        })
        .map_err(|error| error.to_string())
}

/// Return branch worktrees known to the live session. This is deliberately
/// independent of SQLite so an unregistered live checkout is still included
/// in a destructive workspace removal.
pub(crate) fn live_workspace_worktree_dirs(
    session: &crate::session::Session,
    repo_path: &str,
    slug: &str,
) -> Vec<String> {
    session
        .worktrees
        .iter()
        .filter(|group| {
            crate::sidebar::split_tab(&group.name).is_some_and(|(group_slug, _)| group_slug == slug)
                && !group.path.is_empty()
                && group.path != repo_path
                && group.kind != crate::session::GroupKind::Home
        })
        .map(|group| group.path.clone())
        .collect()
}

/// Delete a workspace's worktree directories from disk OFF the event loop.
/// Runs `git worktree remove` + `purge_worktree_files` (local + remote ssh) per
/// dir on a dedicated thread, then pulses `waker` so the loop repaints. Callers
/// keep the in-memory groups and show a pending status until this finishes.
/// No-op (no thread spawned) when `dirs` is empty.
pub(crate) fn spawn_delete_workspace_dirs(
    repo_path: &str,
    dirs: Vec<String>,
    cfg: thegn_core::config::Config,
    slug: &str,
    session_id: &str,
    waker: Option<termwiz::terminal::TerminalWaker>,
) -> std::io::Result<()> {
    let root = Path::new(repo_path).to_path_buf();
    crate::worktree_lifecycle::spawn_workspace_destroy(
        cfg,
        root,
        slug.to_string(),
        session_id.to_string(),
        dirs,
        waker,
    )
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
#[expect(clippy::too_many_arguments)]
pub(crate) fn remove_workspace(
    session: &mut crate::session::Session,
    panes: &mut Panes,
    repo_path: &str,
    slug: &str,
    display: &str,
    keep_files: bool,
    cfg: &thegn_core::config::Config,
    waker: Option<termwiz::terminal::TerminalWaker>,
) -> String {
    let was_active = session.id == repo_path;

    // Start from live session groups. The worker adds DB-only rows after it
    // opens SQLite, so a missing DB row or a DB-open failure cannot hide a live
    // checkout from the destructive set.
    let live_dirs = live_workspace_worktree_dirs(session, repo_path, slug);

    // Destructive: delete each worktree dir from disk off-loop. The DB/session
    // rows stay visible until each worker reports a successful transaction.
    if !keep_files {
        if let Err(error) = spawn_delete_workspace_dirs(
            repo_path,
            live_dirs.clone(),
            cfg.clone(),
            slug,
            &session.id,
            waker,
        ) {
            return format!("Workspace '{display}' removal failed to start: {error}");
        }
        return workspace_removed_status(display, false, live_dirs.len());
    }

    // Keep-files is a pure in-memory reconciliation on the loop. Its cache
    // cleanup runs on the DB writer and, by contract, does not run destroy
    // hooks or touch the checkout.
    let session_end_errors: Vec<String> = live_dirs
        .iter()
        .filter_map(|path| {
            crate::worktree_lifecycle::session_end_once(cfg, Path::new(path), waker.clone())
                .err()
                .map(|error| format!("{path}: {error}"))
        })
        .collect();
    remove_workspace_in_memory(session, panes, slug);
    let session_id = session.id.clone();
    let repo_path_owned = repo_path.to_string();
    let slug_owned = slug.to_string();
    let runtime_handle = tokio::runtime::Handle::try_current().ok();
    let daemon_config = cfg.daemon.clone();
    crate::db_task::persist_critical(move |db| {
        let orphaned_sids = forget_workspace_in_db(db, &session_id, &repo_path_owned, &slug_owned);
        crate::handlers::daemon_lifecycle::kill_orphaned_daemon_sessions_on(
            runtime_handle,
            daemon_config,
            orphaned_sids,
        );
    });

    // Removing the active workspace leaves the session pointing at nothing;
    // land on the next available workspace, else empty out.
    if was_active {
        land_after_workspace_removed(session, None);
    }

    let mut status = workspace_removed_status(display, keep_files, live_dirs.len());
    if !session_end_errors.is_empty() {
        status.push_str(&format!(
            "; session_end failed to start: {}",
            session_end_errors.join("; ")
        ));
    }
    status
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
            0 => format!("Removed project '{display}' (files kept on disk)"),
            1 => format!("Removed project '{display}' (1 worktree remains on disk)"),
            n => format!("Removed project '{display}' ({n} worktrees remain on disk)"),
        }
    } else {
        match orphan_count {
            0 => format!("Removed project '{display}'"),
            1 => format!("Deleting project '{display}' (removing 1 worktree from disk…)"),
            n => format!("Deleting project '{display}' (removing {n} worktrees from disk…)"),
        }
    }
}

/// Engine for [`remove_workspace`], split from the process-global `Db::open()`
/// so tests can inject an isolated DB. Closes the workspace's live groups
/// (always, reaping their panes) and — when a `db` is present — prunes every DB
/// trace and persists the trimmed layout. Non-destructive: never touches the
/// worktree files on disk.
#[cfg(test)]
pub(crate) fn remove_workspace_with_db(
    session: &mut crate::session::Session,
    panes: &mut Panes,
    db: Option<&thegn_core::db::Db>,
    repo_path: &str,
    slug: &str,
) {
    let groups: Vec<crate::session::WorktreeGroup> = session
        .worktrees
        .iter()
        .filter(|g| {
            crate::sidebar::split_tab(&g.name)
                .filter(|(repo, _)| repo == slug)
                .is_some()
        })
        .cloned()
        .collect();
    if let Some(db) = db {
        for group in &groups {
            forget_worktree_group(db, &session.id, group, true, None);
        }
        if let Err(error) = db.delete_tab_groups_with_prefix(slug) {
            tracing::debug!(%error, slug, "workspace removal could not prune matching groups from every persisted session");
        }
    }
    remove_workspace_in_memory(session, panes, slug);

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

/// Close the live groups belonging to a workspace without touching disk or
/// the DB. Lifecycle completion handling uses this after its worker has
/// already completed the cache transaction.
pub(crate) fn remove_workspace_in_memory(
    session: &mut crate::session::Session,
    panes: &mut Panes,
    slug: &str,
) {
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
        for tab in &session.worktrees[gi].tabs {
            for id in tab.center.pane_ids() {
                panes.table.remove(&id);
            }
        }
        session.switch_to(gi);
        session.close_active_group();
    }
}

/// Remove a successfully destroyed worktree from the cache on a worker. The
/// loop later reconciles the corresponding live group separately.
pub(crate) fn forget_worktree_path_in_db(db: &thegn_core::db::Db, session_id: &str, path: &str) {
    if let Ok(groups) = db.groups_for_session(session_id) {
        for group in groups.into_iter().filter(|group| group.worktree == path) {
            let _ = db.delete_tab_group(session_id, &group.name); // best-effort: cache write: layout is resurrection state
            del_ui_state_segment(db, SIDEBAR_SCOPE, "pin", &group.name);
        }
    }
    let _ = db.del_worktree(path); // best-effort: cache write: git is the source of truth
    let _ = db.delete_tab_groups_for_worktree(session_id, path); // best-effort: cache write: clean up legacy layout rows
    thegn_core::activity::forget(path);
}

/// Remove a successfully destroyed workspace from the cache on a worker. The
/// current session's tab groups are removed by slug so an in-flight deletion
/// cannot leave the home group or a stale layout resurrected.
pub(crate) fn forget_workspace_in_db(
    db: &thegn_core::db::Db,
    session_id: &str,
    repo_path: &str,
    slug: &str,
) -> Vec<String> {
    if let Ok(groups) = db.groups_for_session(session_id) {
        for group in groups.into_iter().filter(|group| {
            crate::sidebar::split_tab(&group.name).is_some_and(|(group_slug, _)| group_slug == slug)
        }) {
            let _ = db.delete_tab_group(session_id, &group.name); // best-effort: cache write: layout is resurrection state
        }
    }
    let mut orphaned_sids = Vec::new();
    match db.delete_tab_groups_with_prefix(slug) {
        Ok(deleted_tabs) => {
            for (deleted_session, pane_sessions) in deleted_tabs {
                if deleted_session != session_id {
                    orphaned_sids.extend(daemon_sids_from_pane_sessions(&pane_sessions));
                }
            }
            orphaned_sids.sort_unstable();
            orphaned_sids.dedup();
        }
        Err(error) => {
            tracing::debug!(%error, slug, "workspace removal could not prune matching groups from every persisted session");
        }
    }
    let _ = db.del_worktrees_for_repo(repo_path); // best-effort: cache write: git is the source of truth
    let _ = db.del_workspace(repo_path); // best-effort: cache write: git is the source of truth
    let _ = db.del_repo_slug(repo_path); // best-effort: cache write: git is the source of truth
    let _ = db.tombstone_workspace(repo_path); // best-effort: cache write: removal tombstone
    if db.active_workspace().ok().flatten().as_deref() == Some(repo_path) {
        let _ = db.del_ui_state("", "active_workspace"); // best-effort: cache write: active-workspace pointer
    }
    del_ui_state_segment(db, SIDEBAR_SCOPE, "collapse", slug);
    del_ui_state_segment(db, SIDEBAR_SCOPE, "pin", slug);
    orphaned_sids
}

fn daemon_sids_from_pane_sessions(json: &str) -> Vec<String> {
    let pane_sessions = match serde_json::from_str::<
        std::collections::BTreeMap<u32, crate::session::ProviderSession>,
    >(json)
    {
        Ok(pane_sessions) => pane_sessions,
        Err(error) => {
            tracing::debug!(%error, "could not parse deleted tab pane sessions while removing workspace");
            return Vec::new();
        }
    };
    pane_sessions
        .into_values()
        .filter(|provider_session| provider_session.provider == "daemon")
        .map(|provider_session| provider_session.session)
        .collect()
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
    fn forget_workspace_in_db_prunes_other_sessions_directly() {
        let db = thegn_core::db::Db::open_memory().unwrap();
        db.put_workspace("/r/lib", "lib", "repo").unwrap();
        let group = thegn_core::models::TabGroupRow {
            name: "lib/stale".into(),
            kind: "branch".into(),
            worktree: "/wt/stale".into(),
            ordinal: 0,
            active_tab: 0,
        };
        let tab = thegn_core::models::GroupTabRow {
            group_name: "lib/stale".into(),
            ordinal: 0,
            title: "1".into(),
            pane_tree: r#"{"leaf":0}"#.into(),
            focused_pane: 0,
            pane_cwds: String::new(),
            pane_cmds: String::new(),
            pane_sessions: String::new(),
            scrollback_snapshot: String::new(),
        };
        db.put_tab_group("other-session", &group).unwrap();
        db.put_group_tab("other-session", &tab).unwrap();

        assert!(forget_workspace_in_db(&db, "active-session", "/r/lib", "lib").is_empty());

        assert!(db.groups_for_session("other-session").unwrap().is_empty());
        assert!(
            db.group_tabs_for_session("other-session")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn forget_workspace_in_db_returns_foreign_daemon_sids() {
        let db = thegn_core::db::Db::open_memory().unwrap();
        db.put_workspace("/r/lib", "lib", "repo").unwrap();
        let group = |name: &str| thegn_core::models::TabGroupRow {
            name: name.into(),
            kind: "branch".into(),
            worktree: format!("/wt/{name}"),
            ordinal: 0,
            active_tab: 0,
        };
        let tab = |name: &str, pane_sessions: &str| thegn_core::models::GroupTabRow {
            group_name: name.into(),
            ordinal: 0,
            title: "1".into(),
            pane_tree: r#"{"leaf":0}"#.into(),
            focused_pane: 0,
            pane_cwds: String::new(),
            pane_cmds: String::new(),
            pane_sessions: pane_sessions.into(),
            scrollback_snapshot: String::new(),
        };
        db.put_tab_group("other-session", &group("lib/stale"))
            .unwrap();
        db.put_group_tab(
            "other-session",
            &tab(
                "lib/stale",
                r#"{"0":{"provider":"daemon","id":"x","session":"sid-a"},"1":{"provider":"sprites","id":"y","session":"sid-b"}}"#,
            ),
        )
        .unwrap();
        db.put_tab_group("active-session", &group("lib/home"))
            .unwrap();
        db.put_group_tab(
            "active-session",
            &tab(
                "lib/home",
                r#"{"0":{"provider":"daemon","id":"x","session":"sid-c"}}"#,
            ),
        )
        .unwrap();

        assert_eq!(
            forget_workspace_in_db(&db, "active-session", "/r/lib", "lib"),
            vec!["sid-a".to_string()]
        );
    }

    #[test]
    fn daemon_sids_from_malformed_pane_sessions_is_empty() {
        assert!(daemon_sids_from_pane_sessions("not json").is_empty());
    }

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

        let mut dirs = workspace_worktree_dirs(&db, "/tmp/repo-lib").unwrap();
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
    fn live_workspace_dirs_include_unregistered_worktrees_but_not_home() {
        let session = Session {
            id: "/tmp/repo-lib".into(),
            worktrees: vec![
                WorktreeGroup::new("lib/home", GroupKind::Home, "/tmp/repo-lib"),
                WorktreeGroup::new("lib/unregistered", GroupKind::Branch, "/tmp/lib-live"),
                WorktreeGroup::new("app/other", GroupKind::Branch, "/tmp/app-live"),
            ],
            active: 0,
        };
        assert_eq!(
            live_workspace_worktree_dirs(&session, "/tmp/repo-lib", "lib"),
            vec!["/tmp/lib-live"]
        );
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
        let stale_group = thegn_core::models::TabGroupRow {
            name: "lib/stale".into(),
            kind: "branch".into(),
            worktree: "/tmp/lib-stale".into(),
            ordinal: 0,
            active_tab: 0,
        };
        let stale_tab = thegn_core::models::GroupTabRow {
            group_name: "lib/stale".into(),
            ordinal: 0,
            title: "1".into(),
            pane_tree: r#"{"leaf":0}"#.into(),
            focused_pane: 0,
            pane_cwds: String::new(),
            pane_cmds: String::new(),
            pane_sessions: String::new(),
            scrollback_snapshot: String::new(),
        };
        db.put_tab_group("/tmp/repo-app", &stale_group).unwrap();
        db.put_group_tab("/tmp/repo-app", &stale_tab).unwrap();
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
        assert!(
            db.groups_for_session("/tmp/repo-app").unwrap().is_empty(),
            "the removed slug is pruned from another session's saved groups"
        );
        assert!(
            db.group_tabs_for_session("/tmp/repo-app")
                .unwrap()
                .is_empty(),
            "tabs belonging to the removed foreign group are pruned too"
        );
        assert_eq!(db.active_workspace().unwrap(), None);
        let _ = std::fs::remove_file(&db_path); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
    }

    #[test]
    fn remove_workspace_reports_status() {
        // Keep-files reports survivors; destructive reports the background job.
        assert_eq!(
            workspace_removed_status("lib", true, 0),
            "Removed project 'lib' (files kept on disk)"
        );
        assert_eq!(
            workspace_removed_status("lib", true, 1),
            "Removed project 'lib' (1 worktree remains on disk)"
        );
        assert_eq!(
            workspace_removed_status("lib", true, 3),
            "Removed project 'lib' (3 worktrees remain on disk)"
        );
        assert_eq!(
            workspace_removed_status("lib", false, 0),
            "Removed project 'lib'"
        );
        assert_eq!(
            workspace_removed_status("lib", false, 3),
            "Deleting project 'lib' (removing 3 worktrees from disk…)"
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
