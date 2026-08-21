//! Loop-side bodies for the sidebar's folder/terminal actions (rename folder,
//! new empty folder, delete folder, close terminal). All follow the
//! optimistic-update + deferred-persist pattern of `sidebar_folder.rs`: the
//! model mutates so the sidebar regroups this frame, the durable DB write
//! rides `spawn_blocking` (best-effort, DB-is-a-cache), and the refresh sent
//! *after* the write reconciles synthetic state with real rows.

use thegn_core::store::WorkspaceStore;
use tokio::sync::mpsc::UnboundedSender;

use crate::chrome::FrameModel;
use crate::handlers::worktree_delete::DeleteCtx;
use crate::hydrate::RefreshKind;
use crate::run::{SidebarState, now_secs};

/// Rename folder `folder_id` to `name`, optimistically.
pub(crate) fn rename_folder(
    session: &crate::session::Session,
    sb: &mut SidebarState,
    model: &mut FrameModel,
    folder_id: i64,
    name: &str,
    refresh_tx: &UnboundedSender<RefreshKind>,
    waker: &termwiz::terminal::TerminalWaker,
) -> String {
    let name = name.trim();
    if name.is_empty() {
        return "Folder name is empty".into();
    }
    if let Some(f) = model
        .sidebar_db_folders
        .iter_mut()
        .find(|f| f.folder_id == folder_id)
    {
        f.name = name.to_string();
    }
    sb.optimistic_db_edit_at = Some(std::time::Instant::now());
    sb.rebuild(model, session);

    let name = name.to_string();
    let refresh_tx = refresh_tx.clone();
    let waker = waker.clone();
    tokio::task::spawn_blocking(move || {
        // best-effort beyond the warn: the DB is a cache, but a silent failure
        // means the deferred refresh visibly REVERTS the rename with no
        // explanation — log why.
        match thegn_core::db::Db::open() {
            Ok(db) => {
                if let Err(e) = db.rename_folder(folder_id, &name) {
                    tracing::warn!(target: "thegn::sidebar", error = %e, "folder rename not persisted");
                }
            }
            Err(e) => {
                tracing::warn!(target: "thegn::sidebar", error = %e, "folder rename not persisted: DB unavailable");
            }
        }
        if refresh_tx.send(RefreshKind::Model).is_ok() {
            let _ = waker.wake();
        }
    });
    "Folder renamed".into()
}

/// Create an empty folder named `name` in `repo_path`'s workspace,
/// optimistically (synthetic negative id until the deferred `ensure_folder`
/// write lands — same sentinel scheme as `sidebar_folder::apply_optimistic_move`).
pub(crate) fn create_empty_folder(
    session: &crate::session::Session,
    sb: &mut SidebarState,
    model: &mut FrameModel,
    repo_path: &str,
    name: &str,
    refresh_tx: &UnboundedSender<RefreshKind>,
    waker: &termwiz::terminal::TerminalWaker,
) -> String {
    let name = name.trim();
    if name.is_empty() {
        return "Folder name is empty".into();
    }
    let exists = model
        .sidebar_db_folders
        .iter()
        .any(|f| f.repo_path == repo_path && f.name.trim().eq_ignore_ascii_case(name));
    if !exists {
        let sentinel = model
            .sidebar_db_folders
            .iter()
            .map(|f| f.folder_id)
            .min()
            .unwrap_or(0)
            .min(0)
            - 1;
        let position = model
            .sidebar_db_folders
            .iter()
            .filter(|f| f.repo_path == repo_path)
            .map(|f| f.position)
            .max()
            .unwrap_or(-1)
            + 1;
        model
            .sidebar_db_folders
            .push(thegn_core::models::FolderRow {
                folder_id: sentinel,
                repo_path: repo_path.to_string(),
                name: name.to_string(),
                position,
                created_at: now_secs(),
            });
        sb.optimistic_db_edit_at = Some(std::time::Instant::now());
        sb.rebuild(model, session);
    }

    let repo_path = repo_path.to_string();
    let name_owned = name.to_string();
    let refresh_tx = refresh_tx.clone();
    let waker = waker.clone();
    tokio::task::spawn_blocking(move || {
        // best-effort beyond the warn: a silent failure means the sentinel
        // row visibly VANISHES on the deferred refresh — log why.
        match thegn_core::db::Db::open() {
            Ok(db) => {
                if let Err(e) = db.ensure_folder(&repo_path, &name_owned) {
                    tracing::warn!(target: "thegn::sidebar", error = %e, "folder create not persisted");
                }
            }
            Err(e) => {
                tracing::warn!(target: "thegn::sidebar", error = %e, "folder create not persisted: DB unavailable");
            }
        }
        if refresh_tx.send(RefreshKind::Model).is_ok() {
            let _ = waker.wake();
        }
    });
    format!("Created folder \"{name}\"")
}

/// Delete the folder in `arg` (a stringified folder id): its filed worktrees
/// move back to the workspace root (files never touched). Optimistic model
/// update; the deferred write also prunes the folder's persisted view keys.
pub(crate) fn delete_folder(
    session: &crate::session::Session,
    sb: &mut SidebarState,
    model: &mut FrameModel,
    arg: &str,
    refresh_tx: &UnboundedSender<RefreshKind>,
    waker: &termwiz::terminal::TerminalWaker,
) -> String {
    let Ok(folder_id) = arg.parse::<i64>() else {
        return "Bad folder id".into();
    };
    // Optimistic: unfile members, drop the folder row, forget its view keys.
    for w in model
        .sidebar_db_worktrees
        .iter_mut()
        .filter(|w| w.folder_id == Some(folder_id))
    {
        w.folder_id = None;
    }
    let removed = model
        .sidebar_db_folders
        .iter()
        .position(|f| f.folder_id == folder_id)
        .map(|i| model.sidebar_db_folders.remove(i));
    // The collapse/pin key is `{slug}/folder:{id}` — resolve the slug from the
    // removed row's repo path via the model's workspace list.
    let key = removed.as_ref().and_then(|f| {
        model
            .sidebar_workspaces
            .iter()
            .find(|(_, _, _, p)| *p == f.repo_path)
            .map(|(slug, ..)| format!("{slug}/folder:{folder_id}"))
    });
    if let Some(key) = &key {
        sb.view.collapsed.remove(key);
        sb.view.pins.retain(|k| k != key);
    }
    let name = removed.map(|f| f.name).unwrap_or_default();
    sb.optimistic_db_edit_at = Some(std::time::Instant::now());
    sb.rebuild(model, session);

    let refresh_tx = refresh_tx.clone();
    let waker = waker.clone();
    tokio::task::spawn_blocking(move || {
        if let Ok(db) = thegn_core::db::Db::open() {
            // best-effort beyond the warn: a silent failure means the folder
            // visibly REAPPEARS on the deferred refresh after the toast said
            // it was deleted — log why. The view-key deletes stay silent
            // (cache-only cosmetics).
            if let Err(e) = db.del_folder(folder_id) {
                tracing::warn!(target: "thegn::sidebar", error = %e, "folder delete not persisted");
            }
            if let Some(key) = &key {
                // Exact-key deletes — NOT a LIKE prefix. The key is
                // `{slug}/folder:{id}` with no trailing delimiter, so
                // `folder:1` is a strict prefix of `folder:10`/`:12`/…; a
                // prefix delete would wipe those sibling folders' persisted
                // collapse/pin state. toggle_collapse/toggle_pin unpersist
                // these same keys exactly, so match that.
                let _ = db.del_ui_state(
                    crate::handlers::sidebar_persist::SIDEBAR_SCOPE,
                    &format!("collapse:{key}"),
                );
                let _ = db.del_ui_state(
                    crate::handlers::sidebar_persist::SIDEBAR_SCOPE,
                    &format!("pin:{key}"),
                );
            }
        }
        if refresh_tx.send(RefreshKind::Model).is_ok() {
            let _ = waker.wake();
        }
    });
    if name.is_empty() {
        "Folder deleted (worktrees kept)".into()
    } else {
        format!("Deleted folder \"{name}\" (worktrees kept)")
    }
}

/// Close the terminal named `name`: end its live session group (if loaded) and
/// delete its DB row so it doesn't resurrect on restart.
pub(crate) fn close_terminal(
    cx: &mut DeleteCtx<'_>,
    name: &str,
    refresh_tx: &UnboundedSender<RefreshKind>,
) {
    // Live group: the same close path worktrees use (panes torn down, layout
    // persisted, focus restored).
    let had_live = cx.session.worktrees.iter().any(|g| g.name == name);
    if let Some(gi) = cx.session.worktrees.iter().position(|g| g.name == name) {
        crate::handlers::worktree_delete::perform_close(cx, vec![gi]);
    }
    // DB row: optimistic removal + deferred delete.
    let id = cx
        .model
        .sidebar_db_terminals
        .iter()
        .position(|t| t.name == name)
        .map(|i| cx.model.sidebar_db_terminals.remove(i).id);
    let had_row = id.is_some();
    cx.sb.optimistic_db_edit_at = Some(std::time::Instant::now());
    cx.sb.rebuild(cx.model, cx.session);
    if let Some(id) = id {
        let refresh_tx = refresh_tx.clone();
        let waker = cx.waker.clone();
        tokio::task::spawn_blocking(move || {
            // best-effort beyond the warn: a silent failure means the
            // terminal RESURRECTS on restart after the toast said it closed.
            match thegn_core::db::Db::open() {
                Ok(db) => {
                    if let Err(e) = db.del_terminal(id) {
                        tracing::warn!(target: "thegn::sidebar", error = %e, "terminal close not persisted");
                    }
                }
                Err(e) => {
                    tracing::warn!(target: "thegn::sidebar", error = %e, "terminal close not persisted: DB unavailable");
                }
            }
            if refresh_tx.send(RefreshKind::Model).is_ok() {
                let _ = waker.wake();
            }
        });
    }
    // Honest status: only claim a close when something actually closed —
    // reporting success for an already-gone name hides real bugs.
    cx.model.status = if had_live || had_row {
        format!("Closed terminal \"{name}\"")
    } else {
        format!("No terminal named \"{name}\"")
    };
}
