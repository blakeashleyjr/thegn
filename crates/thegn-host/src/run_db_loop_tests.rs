use super::*;
use crate::session::{GroupKind, Session, WorktreeGroup};
use std::collections::HashSet;

#[test]
fn vanished_path_fence_keeps_groups_that_are_not_being_created() {
    let session = Session {
        id: "session".into(),
        worktrees: vec![
            WorktreeGroup::new("app/feat", GroupKind::Branch, "/repo/feat"),
            WorktreeGroup::new("app/other", GroupKind::Branch, "/repo/other"),
        ],
        active: 0,
    };
    let creating = HashSet::from([("app/feat".to_string(), 0)]);
    let paths = vec!["/repo/feat".to_string(), "/repo/other".to_string()];

    assert_eq!(
        fence_vanished_paths(&session, &creating, &paths),
        vec!["/repo/other".to_string()]
    );
}

#[test]
fn workspace_context_reads_the_group_slug_without_opening_the_db() {
    let state = crate::testenv::IsolatedState::new();
    let root = tempfile::tempdir().unwrap();
    let session = Session {
        id: root.path().to_string_lossy().into_owned(),
        worktrees: vec![WorktreeGroup::new(
            "foo-2/home",
            GroupKind::Home,
            root.path().to_string_lossy().into_owned(),
        )],
        active: 0,
    };

    assert_eq!(
        workspace_context(&session),
        (Some(root.path().to_path_buf()), Some("foo-2".into()))
    );
    assert!(!state.state_home().join("thegn/thegn.db").exists());
}

#[test]
fn workspace_context_without_groups_never_creates_the_db() {
    let state = crate::testenv::IsolatedState::new();
    let root = tempfile::tempdir().unwrap();
    let session = Session {
        id: root.path().to_string_lossy().into_owned(),
        worktrees: Vec::new(),
        active: 0,
    };

    assert_eq!(
        workspace_context(&session),
        (Some(root.path().to_path_buf()), None)
    );
    assert!(!state.state_home().join("thegn/thegn.db").exists());
}

#[test]
fn workspace_context_reads_an_existing_slug_read_only() {
    let state = crate::testenv::IsolatedState::new();
    let root = tempfile::tempdir().unwrap();
    let root_s = root.path().to_string_lossy().into_owned();
    {
        let db = thegn_core::db::Db::open().unwrap();
        db.slug_for_repo(&root_s, "foo").unwrap();
    }
    let session = Session {
        id: root_s,
        worktrees: Vec::new(),
        active: 0,
    };

    assert_eq!(workspace_context(&session).1.as_deref(), Some("foo"));
    let db = thegn_core::db::Db::open().unwrap();
    assert_eq!(db.repo_slug_rows().unwrap().len(), 1);
    assert!(state.state_home().join("thegn/thegn.db").exists());
}

#[test]
fn delete_workspace_slug_uses_the_group_prefix() {
    let state = crate::testenv::IsolatedState::new();
    let root = tempfile::tempdir().unwrap();
    let session = Session {
        id: root.path().to_string_lossy().into_owned(),
        worktrees: vec![WorktreeGroup::new(
            "bar-3/home",
            GroupKind::Home,
            root.path().to_string_lossy().into_owned(),
        )],
        active: 0,
    };

    assert_eq!(
        delete_workspace_slug(&session, &session.id).unwrap(),
        "bar-3"
    );
    assert!(!state.state_home().join("thegn/thegn.db").exists());
}

#[test]
fn delete_workspace_slug_fails_fast_under_a_held_writer() {
    let state = crate::testenv::IsolatedState::new();
    drop(thegn_core::db::Db::open().unwrap());
    let db_path = state.state_home().join("thegn/thegn.db");
    let writer = rusqlite::Connection::open(db_path).unwrap();
    writer
        .execute_batch(
            "BEGIN IMMEDIATE; INSERT INTO repo_slugs(repo_path, slug) VALUES('/other','o');",
        )
        .unwrap();

    let session = Session {
        id: "/fresh/repo".into(),
        worktrees: Vec::new(),
        active: 0,
    };
    let started = std::time::Instant::now();
    let result = delete_workspace_slug(&session, &session.id);
    let elapsed = started.elapsed();

    assert!(matches!(result, Err(status) if status.contains("busy")));
    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "delete slug lookup took {elapsed:?}"
    );
    writer.execute_batch("ROLLBACK;").unwrap();
}
