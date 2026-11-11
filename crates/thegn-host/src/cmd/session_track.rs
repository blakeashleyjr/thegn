//! Roster tracking for a plain `session open`.

use anyhow::{Context, Result};
use thegn_core::db::Db;
use thegn_core::issue::{AgentDispatchStatus, NewDispatch};
use thegn_svc::control::client::ControlClient;

/// Build the plain roster row for an already-open daemon session.
///
/// `issue_id` and `worktree_path` are canonicalized by [`insert_and_start`]
/// before this mapping is used. Keeping this pure makes the session binding
/// and the non-pipeline shape explicit and directly testable.
pub(crate) fn track_row<'a>(
    issue_id: &'a str,
    worktree_path: &'a str,
    agent_name: &'a str,
    session_id: &'a str,
) -> NewDispatch<'a> {
    NewDispatch {
        issue_id,
        worktree_path,
        agent_name,
        stage: None,
        parent_id: None,
        session_id: Some(session_id),
        artifact_path: None,
        chunk_path: None,
    }
}

/// Canonicalize the identity, insert the plain row, and move it from queued
/// to running. This is the database-only helper used by the host tests and by
/// the opened-session compensation path.
pub(crate) fn insert_and_start(
    db: &Db,
    issue_id: &str,
    worktree_path: &str,
    agent_name: &str,
    session_id: &str,
) -> Result<i64> {
    let (issue_id, worktree_path) =
        crate::cmd::dispatch::dispatch_identity(issue_id, worktree_path)?;
    let row_id = db.put_dispatch_with_parent_check(track_row(
        &issue_id,
        &worktree_path,
        agent_name,
        session_id,
    ))?;
    db.set_dispatch_status_checked(
        row_id,
        AgentDispatchStatus::Queued,
        AgentDispatchStatus::Running,
        None,
    )?;
    Ok(row_id)
}

/// Track a session after it has opened. If any local roster operation fails,
/// tear down the daemon session so `--track` never leaves a live untracked
/// worker behind.
pub(crate) async fn track_opened(
    client: &ControlClient,
    issue_id: &str,
    worktree_path: &str,
    agent_name: &str,
    session_id: &str,
) -> Result<i64> {
    let result = (|| {
        let db = Db::open()?;
        insert_and_start(&db, issue_id, worktree_path, agent_name, session_id)
    })();
    match result {
        Ok(row_id) => Ok(row_id),
        Err(error) => {
            let kill = client.kill(session_id).await;
            match kill {
                Ok(()) => Err(error.context(
                    "tracked session opened but its dispatch row could not be written; session was killed",
                )),
                Err(kill_error) => Err(error.context(format!(
                    "tracked session opened but its dispatch row could not be written; session kill also failed: {kill_error:#}"
                ))),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{insert_and_start, track_row};
    use std::path::PathBuf;
    use tempfile::TempDir;
    use thegn_core::db::Db;
    use thegn_core::issue::AgentDispatchStatus;
    use thegn_core::store::NotificationStore;
    use thegn_core::util::git_ok;

    fn git_repo() -> (TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("temporary git repo");
        let root = dir.path().join("tracked");
        std::fs::create_dir_all(&root).expect("create git repo root");
        assert!(git_ok(&root, &["init", "-q"]));
        assert!(git_ok(&root, &["config", "user.email", "t@t"]));
        assert!(git_ok(&root, &["config", "user.name", "t"]));
        assert!(git_ok(
            &root,
            &[
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--allow-empty",
                "-q",
                "-m",
                "init"
            ]
        ));
        (dir, root)
    }

    fn db(state: &crate::testenv::IsolatedState) -> Db {
        Db::open_at(&state.state_home().join("track.db")).expect("isolated database")
    }

    #[test]
    fn track_row_maps_plain_fields_and_binds_the_session() {
        let row = track_row("linear:THE-1976", "/worktree", "coder", "session-1");
        assert_eq!(row.issue_id, "linear:THE-1976");
        assert_eq!(row.worktree_path, "/worktree");
        assert_eq!(row.agent_name, "coder");
        assert_eq!(row.stage, None);
        assert_eq!(row.parent_id, None);
        assert_eq!(row.session_id, Some("session-1"));
        assert_eq!(row.artifact_path, None);
        assert_eq!(row.chunk_path, None);
    }

    #[test]
    fn insert_and_start_writes_a_plain_running_row_visible_to_roster_list() {
        let state = crate::testenv::IsolatedState::new();
        let (_repo_dir, worktree) = git_repo();
        let db = db(&state);
        let row_id = insert_and_start(
            &db,
            "LINEAR:THE-1976",
            worktree.to_str().expect("utf8 worktree"),
            "coder",
            "session-1976",
        )
        .expect("tracked row");
        let row = db
            .get_dispatch(row_id)
            .expect("read row")
            .expect("row exists");
        assert_eq!(row.issue_id, "linear:THE-1976");
        assert_eq!(row.worktree_path, worktree.to_string_lossy());
        assert_eq!(row.status, AgentDispatchStatus::Running);
        assert_eq!(row.stage, None);
        assert_eq!(row.session_id.as_deref(), Some("session-1976"));
        assert_eq!(
            db.dispatch_kind(row_id).expect("row kind").as_deref(),
            Some("plain")
        );
        let (listed, truncated) = db
            .dispatch_history_page(None, 50)
            .expect("dispatch list roster query");
        assert!(!truncated);
        assert!(listed.iter().any(|listed| listed.id == row_id));
    }

    #[test]
    fn insert_and_start_refuses_an_invalid_issue_id() {
        let state = crate::testenv::IsolatedState::new();
        let (_repo_dir, worktree) = git_repo();
        let db = db(&state);
        let error = insert_and_start(
            &db,
            "linear:THE 1976",
            worktree.to_str().expect("utf8 worktree"),
            "coder",
            "session-invalid",
        )
        .expect_err("invalid issue id must be refused");
        assert!(error.to_string().contains("invalid pipeline issue id"));
        assert!(db.list_dispatches().expect("roster list").is_empty());
    }
}
