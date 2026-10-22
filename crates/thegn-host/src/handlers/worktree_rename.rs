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
use std::path::{Path, PathBuf};
use termwiz::terminal::TerminalWaker;
use tokio::sync::mpsc as tokio_mpsc;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SweepNote {
    Info(String),
    Warn(String),
}

pub(crate) fn sweep_rename_journals(
    roots: &[PathBuf],
    pending: impl Fn(&Path) -> Result<bool, String>,
    reconcile: impl Fn(&Path) -> Result<Option<thegn_core::worktree::RenameRecovery>, String>,
) -> Vec<SweepNote> {
    let mut notes = Vec::new();
    for root in roots {
        let shown_root = crate::merge_sweep::safe_display(&root.display().to_string());
        match pending(root) {
            Ok(false) => {}
            Err(error) => notes.push(SweepNote::Warn(format!(
                "rename journal check failed for {shown_root}: {}",
                crate::merge_sweep::safe_display(&error)
            ))),
            Ok(true) => match reconcile(root) {
                Ok(Some(thegn_core::worktree::RenameRecovery::RolledBack)) => {
                    notes.push(SweepNote::Info(format!(
                        "rolled back an interrupted worktree rename in {shown_root}"
                    )));
                }
                Ok(Some(
                    thegn_core::worktree::RenameRecovery::Completed
                    | thegn_core::worktree::RenameRecovery::NotStarted,
                )) => notes.push(SweepNote::Info(format!(
                    "cleared a finished rename journal in {shown_root}"
                ))),
                Ok(None) => {}
                Err(error) => notes.push(SweepNote::Warn(format!(
                    "interrupted worktree rename in {shown_root}: {}",
                    crate::merge_sweep::safe_display(&error)
                ))),
            },
        }
    }
    notes
}

pub(crate) fn spawn_launch_sweep(
    refresh_tx: tokio_mpsc::UnboundedSender<crate::hydrate::RefreshKind>,
    waker: TerminalWaker,
) {
    tokio::task::spawn_blocking(move || {
        use thegn_core::store::WorkspaceStore;

        let db = match thegn_core::db::Db::open() {
            Ok(db) => db,
            Err(error) => {
                tracing::debug!(target: "thegn::worktree", %error, "rename journal sweep skipped: state DB unavailable");
                return;
            }
        };
        let workspaces = match db.workspaces() {
            Ok(workspaces) => workspaces,
            Err(error) => {
                tracing::debug!(target: "thegn::worktree", %error, "rename journal sweep skipped: state DB unavailable");
                return;
            }
        };
        let roots: Vec<PathBuf> = workspaces
            .into_iter()
            .filter(|workspace| workspace.kind == "repo")
            .map(|workspace| PathBuf::from(workspace.repo_path))
            .collect();
        let notes = sweep_rename_journals(
            &roots,
            thegn_core::worktree::rename_intent_pending,
            thegn_core::worktree::reconcile_rename_intent,
        );
        if notes.is_empty() {
            return;
        }
        let priority = if notes.iter().any(|note| matches!(note, SweepNote::Warn(_))) {
            thegn_core::notification::Priority::Alert
        } else {
            thegn_core::notification::Priority::Info
        };
        let message = notes
            .into_iter()
            .map(|note| match note {
                SweepNote::Info(text) | SweepNote::Warn(text) => text,
            })
            .collect::<Vec<_>>()
            .join("\n");
        if let Err(error) =
            refresh_tx.send(crate::hydrate::RefreshKind::Toast { message, priority })
        {
            tracing::debug!(target: "thegn::worktree", %error, "rename journal sweep toast receiver is gone");
            return;
        }
        if let Err(error) = waker.wake() {
            tracing::debug!(target: "thegn::worktree", %error, "rename journal sweep toast wake failed");
        }
    });
}

pub(crate) struct RegistryRename {
    pub(crate) old_path: String,
    pub(crate) new_path: String,
    pub(crate) tab: String,
    pub(crate) want: String,
}

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

/// Apply a finished rename optimistically to the live session. The registry
/// write is queued on the FIFO DB writer; failures return asynchronously.
pub(crate) fn apply(
    session: &mut Session,
    region_last_w: &mut Option<String>,
    done: RenameDone,
    tx: &tokio_mpsc::UnboundedSender<crate::run::run_pending_db::PendingDbResult>,
    waker: &TerminalWaker,
) -> String {
    apply_with(session, region_last_w, done, |request| {
        let status_want = request.want.clone();
        let want = request.want;
        let old_path = request.old_path;
        let new_path = request.new_path;
        let tab = request.tab;
        use thegn_core::store::WorkspaceStore;
        crate::run::run_pending_db::queue_reported_write(
            tx,
            waker,
            move |db| {
                db.rename_worktree(&old_path, &new_path, &tab, &want)
                    .map_err(|error| error.to_string())
            },
            move |error: String| registry_failure_status(&status_want, &error),
        );
    })
}

pub(crate) fn registry_failure_status(want: &str, error: &str) -> String {
    format!("Renamed to {want} (registry update failed: {error})")
}

/// `apply` with the registry queue injected as a seam.
///
/// The session re-keying is pure; only the cache write touches the world. A
/// unit test must never open the CANONICAL `$XDG_STATE_HOME/thegn/thegn.db`.
fn apply_with(
    session: &mut Session,
    region_last_w: &mut Option<String>,
    done: RenameDone,
    queue_rename: impl FnOnce(RegistryRename),
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
    queue_rename(RegistryRename {
        old_path,
        new_path: new_path_s,
        tab,
        want: want.clone(),
    });
    format!("Renamed to {want}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{GroupKind, Session, WorktreeGroup};
    use std::cell::RefCell;

    fn group(name: &str, path: &str) -> WorktreeGroup {
        WorktreeGroup::new(name, GroupKind::Branch, path)
    }

    #[test]
    fn launch_sweep_reconciles_only_roots_with_a_journal() {
        let roots = [
            PathBuf::from("/repo/a"),
            PathBuf::from("/repo/b"),
            PathBuf::from("/repo/c"),
        ];
        let reconciled = RefCell::new(Vec::new());
        let notes = sweep_rename_journals(
            &roots,
            |root| Ok(root.file_name().is_some_and(|name| name == "b")),
            |root| {
                reconciled.borrow_mut().push(root.to_path_buf());
                Ok(Some(thegn_core::worktree::RenameRecovery::RolledBack))
            },
        );
        assert_eq!(*reconciled.borrow(), vec![PathBuf::from("/repo/b")]);
        assert_eq!(
            notes,
            vec![SweepNote::Info(
                "rolled back an interrupted worktree rename in /repo/b".into()
            )]
        );
    }

    #[test]
    fn launch_sweep_keeps_going_after_an_ambiguous_journal() {
        let roots = [PathBuf::from("/repo/b"), PathBuf::from("/repo/c")];
        let notes = sweep_rename_journals(
            &roots,
            |_| Ok(true),
            |root| {
                if root.file_name().is_some_and(|name| name == "b") {
                    Err("ambiguous state; journal kept at /x".into())
                } else {
                    Ok(Some(thegn_core::worktree::RenameRecovery::Completed))
                }
            },
        );
        assert!(matches!(
            notes.as_slice(),
            [SweepNote::Warn(warning), SweepNote::Info(info)]
                if warning.contains("journal kept") && info.contains("/repo/c")
        ));

        let pending_error = sweep_rename_journals(
            &[PathBuf::from("/repo/unreadable")],
            |_| Err("permission denied".into()),
            |_| Ok(None),
        );
        assert!(matches!(
            pending_error.as_slice(),
            [SweepNote::Warn(text)] if text.contains("check failed")
        ));
    }

    #[test]
    #[expect(
        clippy::disallowed_methods,
        reason = "real git fixture exercises journal rollback"
    )]
    fn launch_sweep_rolls_back_a_real_split_rename() {
        let dir = tempfile::tempdir_in("/tmp").unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let run = |args: &[&str]| {
            thegn_core::util::git_cmd(&repo)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .args(args)
                .status()
                .unwrap()
        };
        assert!(run(&["init", "-q", "-b", "main"]).success());
        assert!(run(&["config", "user.email", "t@t.t"]).success());
        assert!(run(&["config", "user.name", "t"]).success());
        assert!(
            run(&[
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--allow-empty",
                "-q",
                "-m",
                "init"
            ])
            .success()
        );

        let wt_old = dir.path().join("wt-old");
        assert!(
            run(&[
                "worktree",
                "add",
                "-q",
                "-b",
                "old",
                wt_old.to_str().unwrap(),
                "main"
            ])
            .success()
        );
        let repo = std::fs::canonicalize(repo).unwrap();
        let wt_old = std::fs::canonicalize(wt_old).unwrap();
        let wt_new = dir.path().join("wt-new");
        let journal = repo.join(".git/thegn-rename-intent.json");
        std::fs::write(
            journal.as_path(),
            serde_json::to_vec(&serde_json::json!({
                "version": 1,
                "old_branch": "old",
                "new_branch": "new",
                "old_path": wt_old.to_string_lossy(),
                "new_path": wt_new.to_string_lossy(),
            }))
            .unwrap(),
        )
        .unwrap();
        assert!(
            thegn_core::util::git_cmd(&repo)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .args(["branch", "-m", "old", "new"])
                .status()
                .unwrap()
                .success()
        );

        let notes = sweep_rename_journals(
            std::slice::from_ref(&repo),
            thegn_core::worktree::rename_intent_pending,
            thegn_core::worktree::reconcile_rename_intent,
        );
        assert!(matches!(&notes[..], [SweepNote::Info(text)] if text.contains("rolled back")));
        assert!(
            thegn_core::util::git_cmd(&repo)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .args(["show-ref", "--verify", "--quiet", "refs/heads/old"])
                .status()
                .unwrap()
                .success()
        );
        assert!(
            !thegn_core::util::git_cmd(&repo)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .args(["show-ref", "--verify", "--quiet", "refs/heads/new"])
                .status()
                .unwrap()
                .success()
        );
        assert!(!journal.exists());
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
        let status = apply_with(&mut session, &mut bookmark, done, |_| {});
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
        let status = apply_with(&mut session, &mut bookmark, done, |_| {});
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
        let status = apply_with(&mut session, &mut bookmark, done, |_| {});
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
            apply_with(&mut session, &mut bookmark, done, |_| {}),
            "Renamed to renamed"
        );
        assert_eq!(bookmark.as_deref(), Some("repo/renamed"));
    }

    /// The failure status remains THE-516's exact user-facing wording.
    #[test]
    fn registry_failure_status_preserves_the_the_516_text() {
        assert_eq!(
            registry_failure_status("renamed", "schema v68 → v69 refused"),
            "Renamed to renamed (registry update failed: schema v68 → v69 refused)"
        );
    }

    #[test]
    fn apply_returns_optimistic_status_and_queues_the_registry_write() {
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
        let mut queued = None;
        let status = apply_with(&mut session, &mut bookmark, done, |request| {
            queued = Some(request);
        });
        assert_eq!(status, "Renamed to renamed");
        let request = queued.expect("registry write queued");
        assert_eq!(request.old_path, "/wt/feature");
        assert_eq!(request.new_path, "/wt/renamed");
        assert_eq!(request.tab, "repo/renamed");
        assert_eq!(request.want, "renamed");
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
        apply_with(&mut session, &mut bookmark, done, |request| {
            seen = Some((
                request.old_path,
                request.new_path,
                request.tab,
                request.want,
            ));
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
