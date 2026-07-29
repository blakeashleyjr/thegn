//! Off-loop worktree rename (audit run.rs:12131 / run.rs:12147).
//!
//! `worktree::BranchSet::load` runs `git for-each-ref` + `git worktree list`,
//! and `worktree::rename` runs `git branch -m` + `git worktree move` — several
//! subprocesses plus a directory move. Running them inline in the key-dispatch
//! match blocks the event loop (violating the no-blocking-I/O invariant), so
//! `request` moves the whole thing onto `spawn_blocking` and hands the result
//! back over a channel + waker pulse, exactly like the git-op pipeline.
//!
//! The completion (`apply`) re-keys the live session group by IDENTITY
//! (`old_path`), never by the index captured when the rename prompt opened:
//! background reaps/prunes shift indices while the modal is open, so re-keying
//! by a stale index would corrupt an unrelated worktree's name + path.

use crate::session::Session;
use termwiz::terminal::TerminalWaker;
use tokio::sync::mpsc as tokio_mpsc;

/// One finished (or failed) worktree rename, tagged with the pre-rename
/// identity so the loop can re-locate the group even if indices shifted.
pub(crate) struct RenameDone {
    /// The worktree's path BEFORE the rename — the stable key for re-location.
    pub old_path: String,
    /// The deduped branch the user actually got (may differ from what was typed).
    pub want: String,
    pub result: Result<std::path::PathBuf, String>,
}

/// Run the branch/worktree rename off the loop. `old_path`/`old_branch`
/// identify the worktree; `text` is the raw typed name. The result lands on
/// `tx` with a waker pulse.
pub(crate) fn request(
    repo_root: String,
    old_path: String,
    old_branch: String,
    text: String,
    cfg: thegn_core::config::Config,
    tx: &tokio_mpsc::UnboundedSender<RenameDone>,
    waker: &TerminalWaker,
) {
    let tx = tx.clone();
    let waker = waker.clone();
    tokio::task::spawn_blocking(move || {
        let root = std::path::Path::new(&repo_root);
        // Dedupe the typed name against existing branches (excluding the one
        // being renamed) — this is a git read, hence off-loop.
        let mut taken = thegn_core::worktree::BranchSet::load(root);
        taken.remove(&old_branch);
        let want = thegn_core::worktree::dedupe(text.trim(), &taken);
        let result = thegn_core::worktree::rename(
            root,
            std::path::Path::new(&old_path),
            &old_branch,
            &want,
            &cfg,
        );
        let _ = tx.send(RenameDone {
            old_path,
            want,
            result,
        });
        let _ = waker.wake();
    });
}

/// Apply a finished rename to the live session. Re-keys the group found by
/// `old_path` (identity, not index) and persists the rename to the DB cache.
/// Returns the status line to show. Pure of I/O except the DB cache write
/// (best-effort; git is the source of truth).
pub(crate) fn apply(session: &mut Session, done: RenameDone) -> String {
    let RenameDone {
        old_path,
        want,
        result,
    } = done;
    let new_path = match result {
        Ok(p) => p,
        Err(why) => return format!("rename failed: {why}"),
    };
    let new_path_s = new_path.to_string_lossy().into_owned();
    // Resolve the group by IDENTITY (old_path), never by a captured index —
    // background reaps/prunes may have shifted indices while the modal was open.
    let Some(g) = session.worktrees.iter_mut().find(|g| g.path == old_path) else {
        return format!("Renamed to {want} (worktree no longer in session)");
    };
    let slug = crate::sidebar::split_tab(&g.name)
        .map(|(s, _)| s)
        .unwrap_or_default();
    g.name = format!("{slug}/{want}");
    g.path = new_path_s.clone();
    let tab = g.name.clone();
    use thegn_core::store::WorkspaceStore;
    if let Ok(db) = thegn_core::db::Db::open() {
        // best-effort: the DB is a cache; git already moved the worktree.
        let _ = db.rename_worktree(&old_path, &new_path_s, &tab, &want);
    }
    format!("Renamed to {want}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{GroupKind, Session, WorktreeGroup};

    fn group(name: &str, path: &str) -> WorktreeGroup {
        WorktreeGroup::new(name, GroupKind::Branch, path)
    }

    #[test]
    fn apply_rekeys_by_identity_not_index() {
        let mut session = Session {
            worktrees: vec![
                group("repo/main", "/wt/main"),
                group("repo/feature", "/wt/feature"),
                group("repo/other", "/wt/other"),
            ],
            ..Default::default()
        };
        // The rename was armed against index 2 (/wt/other), but by the time it
        // completes a background reap removed index 0 → indices shifted.
        session.worktrees.remove(0);
        let done = RenameDone {
            old_path: "/wt/other".into(),
            want: "renamed".into(),
            result: Ok(std::path::PathBuf::from("/wt/renamed")),
        };
        let status = apply(&mut session, done);
        assert_eq!(status, "Renamed to renamed");
        // The correct group (found by old_path) was re-keyed, not whatever now
        // sits at the stale index.
        let g = session
            .worktrees
            .iter()
            .find(|g| g.path == "/wt/renamed")
            .expect("renamed group present by new path");
        assert_eq!(g.name, "repo/renamed");
        // The unrelated group at the (formerly) captured index is untouched.
        assert!(
            session
                .worktrees
                .iter()
                .any(|g| g.name == "repo/feature" && g.path == "/wt/feature")
        );
    }

    #[test]
    fn apply_reports_when_group_gone() {
        let mut session = Session {
            worktrees: vec![group("repo/main", "/wt/main")],
            ..Default::default()
        };
        let done = RenameDone {
            old_path: "/wt/vanished".into(),
            want: "x".into(),
            result: Ok(std::path::PathBuf::from("/wt/x")),
        };
        let status = apply(&mut session, done);
        assert!(status.contains("no longer in session"), "got: {status}");
        assert_eq!(session.worktrees.len(), 1);
    }

    #[test]
    fn apply_surfaces_error() {
        let mut session = Session::default();
        let done = RenameDone {
            old_path: "/wt/x".into(),
            want: "x".into(),
            result: Err("boom".into()),
        };
        let status = apply(&mut session, done);
        assert_eq!(status, "rename failed: boom");
    }
}
