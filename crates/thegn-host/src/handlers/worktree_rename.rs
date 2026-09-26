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
        // best-effort: send: the consumer may be gone; a closed channel is the consumer going away
        let _ = tx.send(RenameDone {
            old_path,
            want,
            result,
        });
        let _ = waker.wake(); // best-effort: waker pulse: an input nudge must never fail the calling path
    });
}

/// Apply a finished rename to the live session. Re-keys the group found by
/// `old_path` (identity, not index) and persists the rename to the DB cache.
/// Returns the status line to show. Pure of I/O except the DB cache write
/// (best-effort; git is the source of truth).
pub(crate) fn apply(
    session: &mut Session,
    region_last_w: &mut Option<String>,
    done: RenameDone,
) -> String {
    apply_with(
        session,
        region_last_w,
        done,
        |old_path, new_path, tab, want| {
            use thegn_core::store::WorkspaceStore;
            thegn_core::db::Db::open()
                .and_then(|db| db.rename_worktree(old_path, new_path, tab, want))
                .map_err(|e| e.to_string())
        },
    )
}

/// `apply` with the registry write injected as a seam.
///
/// The session re-keying is pure; only the cache write touches the world. A
/// unit test must never open the CANONICAL `$XDG_STATE_HOME/thegn/thegn.db` —
/// doing so made these tests mutate the developer's live registry, and once the
/// migration guard landed it made them fail outright whenever the on-disk
/// schema trailed the build. Production passes the real DB; tests pass a stub.
fn apply_with(
    session: &mut Session,
    region_last_w: &mut Option<String>,
    done: RenameDone,
    cache_rename: impl FnOnce(&str, &str, &str, &str) -> Result<(), String>,
) -> String {
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
    let old_name = g.name.clone();
    g.name = thegn_core::repo::branch_tab(&slug, &want);
    g.path = new_path_s.clone();
    let tab = g.name.clone();
    if region_last_w.as_deref() == Some(old_name.as_str()) {
        *region_last_w = Some(tab.clone());
    }
    // Git already moved the worktree (it is the source of truth), so a cache
    // failure does not undo the rename — but it is reported, never swallowed
    // as success (THE-516): the registry still names the old path until the
    // next reconcile.
    match cache_rename(&old_path, &new_path_s, &tab, &want) {
        Ok(()) => format!("Renamed to {want}"),
        Err(e) => format!("Renamed to {want} (registry update failed: {e})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{GroupKind, Session, WorktreeGroup};

    fn group(name: &str, path: &str) -> WorktreeGroup {
        WorktreeGroup::new(name, GroupKind::Branch, path)
    }

    /// A registry write that succeeds without touching any database. Tests go
    /// through `apply_with` rather than `apply` so they never open the
    /// canonical state DB — see `apply_with`'s doc comment.
    fn cache_ok(_old: &str, _new: &str, _tab: &str, _want: &str) -> Result<(), String> {
        Ok(())
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
        let mut bookmark = None;
        let status = apply_with(&mut session, &mut bookmark, done, cache_ok);
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
        let mut bookmark = None;
        let status = apply(&mut session, &mut bookmark, done);
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
        let mut bookmark = None;
        let status = apply(&mut session, &mut bookmark, done);
        assert_eq!(status, "rename failed: boom");
    }

    #[test]
    fn apply_rekeys_terminal_region_worktree_bookmark() {
        let mut session = Session {
            worktrees: vec![group("repo/feature", "/wt/feature")],
            ..Default::default()
        };
        let mut bookmark = Some("repo/feature".into());
        let done = RenameDone {
            old_path: "/wt/feature".into(),
            want: "renamed".into(),
            result: Ok(std::path::PathBuf::from("/wt/renamed")),
        };
        assert_eq!(
            apply_with(&mut session, &mut bookmark, done, cache_ok),
            "Renamed to renamed"
        );
        assert_eq!(bookmark.as_deref(), Some("repo/renamed"));
    }

    /// THE-516: a registry write that fails must be reported in the status
    /// line, never swallowed as a clean success — git already moved the
    /// worktree, so the session is correct while the cache still names the old
    /// path. The seam makes this path reachable without a real database.
    #[test]
    fn apply_reports_registry_failure_without_undoing_the_rename() {
        let mut session = Session {
            worktrees: vec![group("repo/feature", "/wt/feature")],
            ..Default::default()
        };
        let done = RenameDone {
            old_path: "/wt/feature".into(),
            want: "renamed".into(),
            result: Ok(std::path::PathBuf::from("/wt/renamed")),
        };
        let mut bookmark = None;
        let status = apply_with(&mut session, &mut bookmark, done, |_, _, _, _| {
            Err("schema v68 → v69 refused".into())
        });
        assert_eq!(
            status,
            "Renamed to renamed (registry update failed: schema v68 → v69 refused)"
        );
        // The in-session re-key still happened: git is the source of truth and
        // the cache is only a cache.
        let g = &session.worktrees[0];
        assert_eq!(g.name, "repo/renamed");
        assert_eq!(g.path, "/wt/renamed");
    }

    /// The seam is handed the POST-rename identity, not the stale one — a
    /// registry keyed on the old path is what lets the next reconcile find the
    /// row at all.
    #[test]
    fn apply_hands_the_registry_both_identities() {
        let mut session = Session {
            worktrees: vec![group("repo/feature", "/wt/feature")],
            ..Default::default()
        };
        let done = RenameDone {
            old_path: "/wt/feature".into(),
            want: "renamed".into(),
            result: Ok(std::path::PathBuf::from("/wt/renamed")),
        };
        let mut seen = None;
        let mut bookmark = None;
        apply_with(&mut session, &mut bookmark, done, |old, new, tab, want| {
            seen = Some((
                old.to_string(),
                new.to_string(),
                tab.to_string(),
                want.to_string(),
            ));
            Ok(())
        });
        assert_eq!(
            seen,
            Some((
                "/wt/feature".into(),
                "/wt/renamed".into(),
                "repo/renamed".into(),
                "renamed".into()
            ))
        );
    }
}
