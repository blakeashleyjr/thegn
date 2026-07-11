//! File the active worktree into a sidebar folder, optimistically.
//!
//! Extracted from `run.rs` (pinned by the file-size ratchet) and reshaped to
//! feel instant. The old inline version did blocking `Db::open` +
//! `ensure_folder` + `set_worktree_folder` on the loop and *then* fired
//! `RefreshKind::Model`, so the row didn't visibly move under its folder until a
//! full `build_model` hydration round-trip completed — which reads all
//! worktrees, folders, PR cache and git status. That's what made the move "take
//! a while."
//!
//! Now we follow the codebase's optimistic-update + deferred-persist pattern
//! (cf. `run::persist_active_focus`, `drawer_state::set_flag`): mutate the
//! in-memory model so the sidebar regroups on the *same* frame, then push the
//! durable DB write onto `spawn_blocking`. The DB is a cache; git is the source
//! of truth, so a best-effort deferred write is the sanctioned trade. The
//! deferred task fires `RefreshKind::Model` after it writes, which reconciles a
//! freshly-created folder's temporary id with the real DB id.

use thegn_core::models::FolderRow;
use thegn_core::store::WorkspaceStore;
use tokio::sync::mpsc::UnboundedSender;

use crate::chrome::FrameModel;
use crate::hydrate::RefreshKind;
use crate::run::{SidebarState, active_worktree_repo, now_secs};
use crate::sidebar::DbWorktree;

/// File the active worktree into `folder` (created if absent), updating the
/// sidebar model in place so the move shows immediately. The durable DB write
/// is deferred off-loop. Returns a status line on success or a human-readable
/// reason on failure.
pub(crate) fn file_active_worktree(
    session: &crate::session::Session,
    sb: &mut SidebarState,
    model: &mut FrameModel,
    folder: &str,
    refresh_tx: &UnboundedSender<RefreshKind>,
    waker: &termwiz::terminal::TerminalWaker,
) -> Result<String, String> {
    let folder = folder.trim();
    if folder.is_empty() {
        return Err("Folder name is empty".into());
    }
    let (wt_path, repo_path) =
        active_worktree_repo(session).ok_or("No worktree to file into a folder")?;

    // Optimistic regroup: resolve/synthesize the folder id and move the worktree
    // under it in the model, then rebuild the sidebar so it shows this frame.
    apply_optimistic_move(
        &mut model.sidebar_db_folders,
        &mut model.sidebar_db_worktrees,
        &repo_path,
        &wt_path,
        folder,
        now_secs(),
    );
    sb.rebuild(model, session);

    // Defer the durable write. Best-effort per the DB-is-a-cache rule; the
    // refresh (sent *after* the write) reconciles a synthetic new-folder id.
    let folder_owned = folder.to_string();
    let refresh_tx = refresh_tx.clone();
    let waker = waker.clone();
    tokio::task::spawn_blocking(move || {
        if let Ok(db) = thegn_core::db::Db::open()
            && let Ok(real_fid) = db.ensure_folder(&repo_path, &folder_owned)
        {
            let _ = db.set_worktree_folder(&wt_path, Some(real_fid));
        }
        if refresh_tx.send(RefreshKind::Model).is_ok() {
            let _ = waker.wake();
        }
    });

    Ok(format!("Filed worktree into \"{folder}\""))
}

/// Pure in-memory regroup shared by the loop path and its tests: resolve the
/// target folder id (matching `db::ensure_folder`'s case-insensitive/trimmed
/// rule), synthesizing a temporary negative-id `FolderRow` when the folder is
/// new, then point the worktree at `wt_path` to it. Returns the resolved id.
///
/// The synthetic id is negative so it can never collide with a real DB
/// `folder_id` (SQLite rowids are positive); the deferred write's
/// `RefreshKind::Model` swaps it for the real id on the next hydration.
fn apply_optimistic_move(
    folders: &mut Vec<FolderRow>,
    worktrees: &mut [DbWorktree],
    repo_path: &str,
    wt_path: &str,
    folder: &str,
    now: i64,
) -> i64 {
    let want = folder.trim();
    let existing = folders
        .iter()
        .find(|f| f.repo_path == repo_path && f.name.trim().eq_ignore_ascii_case(want))
        .map(|f| f.folder_id);
    let fid = match existing {
        Some(id) => id,
        None => {
            // Next unused negative id (min of existing, capped at 0, minus 1).
            let sentinel = folders
                .iter()
                .map(|f| f.folder_id)
                .min()
                .unwrap_or(0)
                .min(0)
                - 1;
            let position = folders
                .iter()
                .filter(|f| f.repo_path == repo_path)
                .map(|f| f.position)
                .max()
                .unwrap_or(-1)
                + 1;
            folders.push(FolderRow {
                folder_id: sentinel,
                repo_path: repo_path.to_string(),
                name: want.to_string(),
                position,
                created_at: now,
            });
            sentinel
        }
    };

    // Move the worktree. If it isn't in the model yet (rare: freshly created,
    // unhydrated), the deferred write + refresh still corrects it.
    if let Some(w) = worktrees.iter_mut().find(|w| w.path == wt_path) {
        w.folder_id = Some(fid);
    }
    fid
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wt(path: &str, folder_id: Option<i64>) -> DbWorktree {
        DbWorktree {
            slug: "repo".into(),
            branch: "b".into(),
            repo_path: "/repo".into(),
            tab_name: "repo/b".into(),
            path: path.into(),
            folder_id,
            sandbox_backend: None,
            env_name: None,
        }
    }

    fn folder(id: i64, name: &str, position: i64) -> FolderRow {
        FolderRow {
            folder_id: id,
            repo_path: "/repo".into(),
            name: name.into(),
            position,
            created_at: 0,
        }
    }

    #[test]
    fn existing_folder_moves_worktree_without_creating() {
        let mut folders = vec![folder(1, "Merging", 0), folder(2, "Done", 1)];
        let mut worktrees = vec![wt("/repo/wt", None)];
        // Case-insensitive + trimmed match, as db::ensure_folder does.
        let fid = apply_optimistic_move(
            &mut folders,
            &mut worktrees,
            "/repo",
            "/repo/wt",
            "  done ",
            99,
        );
        assert_eq!(fid, 2, "matched existing folder id");
        assert_eq!(folders.len(), 2, "no new folder created");
        assert_eq!(worktrees[0].folder_id, Some(2));
    }

    #[test]
    fn new_folder_gets_negative_sentinel_and_next_position() {
        let mut folders = vec![folder(1, "Merging", 0), folder(2, "Done", 3)];
        let mut worktrees = vec![wt("/repo/wt", Some(1))];
        let fid = apply_optimistic_move(
            &mut folders,
            &mut worktrees,
            "/repo",
            "/repo/wt",
            "Fresh",
            99,
        );
        assert!(
            fid < 0,
            "synthetic id is negative to never collide with a DB rowid"
        );
        assert_eq!(fid, -1);
        assert_eq!(folders.len(), 3, "one new folder appended");
        let new = folders.last().unwrap();
        assert_eq!(new.name, "Fresh");
        assert_eq!(new.position, 4, "position is max+1 for the workspace");
        assert_eq!(new.created_at, 99);
        assert_eq!(
            worktrees[0].folder_id,
            Some(-1),
            "worktree re-filed off its old folder"
        );
    }

    #[test]
    fn sentinel_ids_never_collide_across_repeated_new_folders() {
        let mut folders = vec![folder(1, "A", 0)];
        let mut worktrees = vec![wt("/repo/wt", None)];
        let a = apply_optimistic_move(&mut folders, &mut worktrees, "/repo", "/repo/wt", "X", 0);
        let b = apply_optimistic_move(&mut folders, &mut worktrees, "/repo", "/repo/wt", "Y", 0);
        assert_eq!(a, -1);
        assert_eq!(b, -2, "each new folder takes a fresh, lower sentinel");
        assert_eq!(worktrees[0].folder_id, Some(-2));
    }

    #[test]
    fn missing_worktree_in_model_is_a_no_op_move_but_still_resolves_folder() {
        let mut folders = vec![folder(1, "A", 0)];
        let mut worktrees = vec![wt("/repo/other", None)];
        let fid = apply_optimistic_move(
            &mut folders,
            &mut worktrees,
            "/repo",
            "/repo/absent",
            "A",
            0,
        );
        assert_eq!(fid, 1);
        assert_eq!(worktrees[0].folder_id, None, "unrelated worktree untouched");
    }
}
