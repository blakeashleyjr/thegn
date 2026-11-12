//! Roster tracking for a plain `session open`.

use anyhow::Result;
use thegn_core::db::Db;
use thegn_core::issue::{AgentDispatchStatus, NewDispatch};
use thegn_core::store::NotificationStore;
use thegn_svc::control::client::ControlClient;

/// The terminal result recorded for a one-shot integration row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IntegrationOutcome {
    Landed,
    RefusedGate,
}

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
    insert_and_start_with_status(
        db,
        issue_id,
        worktree_path,
        agent_name,
        session_id,
        |db, row_id| {
            db.set_dispatch_status_checked(
                row_id,
                AgentDispatchStatus::Queued,
                AgentDispatchStatus::Running,
                None,
            )
        },
    )
}

/// Record one non-root `land` in the plain roster, without making tracking
/// authoritative for Git. A prior plain land row is idempotent only when it
/// already has the same terminal outcome; another owner's active row is left
/// untouched rather than creating or changing a dispatch.
pub(crate) fn track_integration(
    db: &Db,
    worktree: &str,
    label: &str,
    outcome: IntegrationOutcome,
) -> Result<()> {
    let issue_id = format!("local:{label}");
    let (issue_id, canonical_worktree) =
        crate::cmd::dispatch::dispatch_identity(&issue_id, worktree)?;
    let mut rows = db.list_dispatches()?;
    rows.sort_by_key(|row| row.id);

    let existing_plain = rows.iter().rev().find(|row| {
        row.worktree_path == canonical_worktree && row.stage.is_none() && row.agent_name == "land"
    });
    let active = rows
        .iter()
        .rev()
        .find(|row| row.worktree_path == canonical_worktree && row.status.is_active());
    if active.is_some_and(|row| row.agent_name != "land" || row.stage.is_some()) {
        return Ok(());
    }

    let target = match outcome {
        IntegrationOutcome::Landed => AgentDispatchStatus::Done,
        IntegrationOutcome::RefusedGate => AgentDispatchStatus::Failed,
    };
    if existing_plain.is_some_and(|row| !row.status.is_active() && row.status == target) {
        return Ok(());
    }

    let row_id = if let Some(row) = active {
        row.id
    } else {
        insert_and_start(
            db,
            &issue_id,
            &canonical_worktree,
            "land",
            &format!("land-{}", std::process::id()),
        )?
    };
    let current = db
        .get_dispatch(row_id)?
        .ok_or_else(|| anyhow::anyhow!("integration tracking row {row_id} disappeared"))?;
    if current.status == AgentDispatchStatus::Queued {
        db.set_dispatch_status_checked(
            row_id,
            AgentDispatchStatus::Queued,
            AgentDispatchStatus::Running,
            None,
        )?;
    }
    db.set_dispatch_status_checked(row_id, AgentDispatchStatus::Running, target, Some(label))
}

fn insert_and_start_with_status(
    db: &Db,
    issue_id: &str,
    worktree_path: &str,
    agent_name: &str,
    session_id: &str,
    start: impl FnOnce(&Db, i64) -> Result<()>,
) -> Result<i64> {
    let (issue_id, worktree_path) =
        crate::cmd::dispatch::dispatch_identity(issue_id, worktree_path)?;
    if let Some(row) = db.list_dispatches()?.into_iter().find(|row| {
        row.status.is_active()
            && thegn_core::pipeline_identity::same_issue(&row.issue_id, &issue_id)
    }) {
        anyhow::bail!(
            "issue {issue_id} already has an active dispatch row {}; finish, fail, or abandon it before tracking another plain session",
            row.id
        );
    }
    let row_id = db.put_dispatch_with_parent_check(track_row(
        &issue_id,
        &worktree_path,
        agent_name,
        session_id,
    ))?;
    // A headless agent can exit before this insert finishes. The daemon's
    // session-id exit lookup retries for only 500 ms, so that race may leave
    // the row unstamped; the failure cleanup below still makes it terminal.
    if let Err(error) = start(db, row_id) {
        let note = "plain session tracking could not start the inserted dispatch row";
        return match db.set_dispatch_status_checked(
            row_id,
            AgentDispatchStatus::Queued,
            AgentDispatchStatus::Failed,
            Some(note),
        ) {
            Ok(()) => Err(error.context(
                "dispatch row was written but could not be started; row marked failed",
            )),
            Err(cleanup_error) => Err(error.context(format!(
                "dispatch row was written but could not be started; marking it failed also failed: {cleanup_error:#}"
            ))),
        };
    }
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
                Ok(()) => Err(error.context("tracked session opened but dispatch tracking failed; session was killed")),
                Err(kill_error) => Err(error.context(format!(
                    "tracked session opened but dispatch tracking failed; session kill also failed: {kill_error:#}"
                ))),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        IntegrationOutcome, insert_and_start, insert_and_start_with_status, track_integration,
        track_row,
    };
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
        assert_eq!(
            row.worktree_path,
            std::fs::canonicalize(&worktree)
                .expect("canonical worktree")
                .to_string_lossy()
        );
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
        assert!(format!("{error:#}").contains("invalid pipeline issue id"));
        assert!(db.list_dispatches().expect("roster list").is_empty());
    }

    #[test]
    fn insert_and_start_refuses_a_duplicate_active_issue() {
        let state = crate::testenv::IsolatedState::new();
        let (_repo_dir, worktree) = git_repo();
        let db = db(&state);
        let worktree = worktree.to_str().expect("utf8 worktree");
        insert_and_start(&db, "linear:THE-1976", worktree, "coder", "session-1")
            .expect("first tracked row");
        let error = insert_and_start(&db, "LINEAR:THE-1976", worktree, "coder", "session-2")
            .expect_err("duplicate active issue must be refused");
        assert!(format!("{error:#}").contains("already has an active dispatch row"));
        let (active, truncated) = db
            .list_dispatches_with_status(
                &[
                    AgentDispatchStatus::Queued,
                    AgentDispatchStatus::Spawning,
                    AgentDispatchStatus::Running,
                    AgentDispatchStatus::WaitingHuman,
                    AgentDispatchStatus::PrOpen,
                ],
                50,
            )
            .expect("active roster list");
        assert!(!truncated);
        assert_eq!(active.len(), 1);
    }

    #[test]
    fn insert_and_start_marks_a_row_failed_when_starting_fails() {
        let state = crate::testenv::IsolatedState::new();
        let (_repo_dir, worktree) = git_repo();
        let db = db(&state);
        let error = insert_and_start_with_status(
            &db,
            "linear:THE-1976",
            worktree.to_str().expect("utf8 worktree"),
            "coder",
            "session-failed-start",
            |_db, _row_id| Err(anyhow::anyhow!("forced start failure")),
        )
        .expect_err("forced start failure");
        assert!(format!("{error:#}").contains("forced start failure"));
        let rows = db.list_dispatches().expect("roster list");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, AgentDispatchStatus::Failed);
        assert_eq!(
            db.dispatch_notes(rows[0].id, None, 0)
                .expect("failure note")
                .last()
                .expect("failure note exists")
                .text,
            "plain session tracking could not start the inserted dispatch row"
        );
        let (active, _) = db
            .list_dispatches_with_status(
                &[
                    AgentDispatchStatus::Queued,
                    AgentDispatchStatus::Spawning,
                    AgentDispatchStatus::Running,
                    AgentDispatchStatus::WaitingHuman,
                    AgentDispatchStatus::PrOpen,
                ],
                50,
            )
            .expect("active roster list");
        assert!(active.is_empty());
    }

    #[test]
    fn track_integration_moves_running_to_done() {
        let state = crate::testenv::IsolatedState::new();
        let (_repo_dir, worktree) = git_repo();
        let db = db(&state);
        let path = worktree.to_str().expect("utf8 worktree");
        track_integration(&db, path, "tracked", IntegrationOutcome::Landed).expect("land row");
        let rows = db.list_dispatches().expect("roster list");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].issue_id, "local:tracked");
        assert_eq!(rows[0].status, AgentDispatchStatus::Done);
        assert_eq!(rows[0].agent_name, "land");
    }

    #[test]
    fn track_integration_moves_running_to_failed_for_a_refused_gate() {
        let state = crate::testenv::IsolatedState::new();
        let (_repo_dir, worktree) = git_repo();
        let db = db(&state);
        track_integration(
            &db,
            worktree.to_str().expect("utf8 worktree"),
            "tracked",
            IntegrationOutcome::RefusedGate,
        )
        .expect("failed land row");
        let rows = db.list_dispatches().expect("roster list");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, AgentDispatchStatus::Failed);
    }

    #[test]
    fn track_integration_is_idempotent_for_a_repeat_land() {
        let state = crate::testenv::IsolatedState::new();
        let (_repo_dir, worktree) = git_repo();
        let db = db(&state);
        let path = worktree.to_str().expect("utf8 worktree");
        track_integration(&db, path, "tracked", IntegrationOutcome::Landed)
            .expect("first land row");
        track_integration(&db, path, "tracked", IntegrationOutcome::Landed)
            .expect("repeat land row");
        assert_eq!(db.list_dispatches().expect("roster list").len(), 1);
    }

    #[test]
    fn track_integration_does_not_change_a_foreign_active_row() {
        let state = crate::testenv::IsolatedState::new();
        let (_repo_dir, worktree) = git_repo();
        let db = db(&state);
        let path = worktree.to_str().expect("utf8 worktree");
        insert_and_start(&db, "linear:THE-1980", path, "coder", "session-1980")
            .expect("active row");
        track_integration(&db, path, "tracked", IntegrationOutcome::RefusedGate)
            .expect("ignored foreign row");
        let rows = db.list_dispatches().expect("roster list");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, AgentDispatchStatus::Running);
        assert_eq!(rows[0].agent_name, "coder");
    }

    #[test]
    fn track_integration_records_a_new_outcome_after_a_different_terminal_one() {
        let state = crate::testenv::IsolatedState::new();
        let (_repo_dir, worktree) = git_repo();
        let db = db(&state);
        let path = worktree.to_str().expect("utf8 worktree");
        track_integration(&db, path, "tracked", IntegrationOutcome::RefusedGate)
            .expect("refused land row");
        track_integration(&db, path, "tracked", IntegrationOutcome::Landed)
            .expect("successful land row");
        let rows = db.list_dispatches().expect("roster list");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].status, AgentDispatchStatus::Failed);
        assert_eq!(rows[1].status, AgentDispatchStatus::Done);
    }
}
