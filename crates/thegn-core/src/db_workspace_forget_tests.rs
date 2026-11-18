use super::{ForgetWorkspace, PrQueueLeased, WorkspaceForgetCounts};
use crate::autopilot::{AutopilotIssueKey, AutopilotState};
use crate::db::Db;
use crate::issue::{AgentDispatchStatus, NewDispatch};
use crate::models::{GroupTabRow, TabGroupRow};
use crate::store::{
    AutopilotStore, ClaimOutcome, HibernationRow, HibernationStore, NotificationStore, PoolStore,
    RepoTrustStore, WorkspaceStore, WorktreeAuxStore,
};
use rusqlite::params;

const SCOPE: &str = "sidebar";

fn group(name: &str, worktree: &str) -> TabGroupRow {
    TabGroupRow {
        name: name.into(),
        kind: "branch".into(),
        worktree: worktree.into(),
        ordinal: 0,
        active_tab: 0,
    }
}

fn tab(name: &str) -> GroupTabRow {
    GroupTabRow {
        group_name: name.into(),
        ordinal: 0,
        title: "1".into(),
        pane_tree: r#"{"leaf":0}"#.into(),
        focused_pane: 0,
        pane_cwds: String::new(),
        pane_cmds: String::new(),
        pane_sessions: String::new(),
        scrollback_snapshot: String::new(),
    }
}

/// One workspace with a branch worktree and a row in every table the forget
/// touches. Returns the branch worktree path and its dispatch id.
fn seed(db: &Db, repo: &str, slug: &str) -> (String, i64) {
    let wt = format!("{repo}-feat");
    db.put_workspace(repo, slug, "repo").unwrap();
    assert_eq!(db.slug_for_repo(repo, slug).unwrap(), slug);
    db.put_worktree(&format!("{slug}/feat"), repo, &wt, "feat", None, None)
        .unwrap();
    for session in ["s1", "s2"] {
        db.put_tab_group(session, &group(&format!("{slug}/feat"), &wt))
            .unwrap();
        db.put_group_tab(session, &tab(&format!("{slug}/feat")))
            .unwrap();
    }
    db.set_ui_state(SCOPE, &format!("pin:{slug}/feat"), "1")
        .unwrap();
    db.set_ui_state(SCOPE, &format!("collapse:{slug}"), "1")
        .unwrap();
    db.enqueue_merge(&wt, "feat", "main").unwrap();
    let dispatch = db
        .put_agent_dispatch(NewDispatch::new(&format!("linear:{slug}"), &wt, "claude"))
        .unwrap();
    db.repo_trust_decide(repo, &format!("rid-{slug}"), "{}", "approved", 1)
        .unwrap();
    db.insert_pool_spare(&format!("spare-{slug}"), repo, "default")
        .unwrap();
    db.set_pool_target(repo, "default", 2).unwrap();
    db.put_hibernation(&HibernationRow {
        worktree_path: wt.clone(),
        repo_path: repo.into(),
        env_name: "default".into(),
        sandbox_name: format!("sb-{slug}"),
        snapshot_id: "snap".into(),
        head: String::new(),
        state: "hibernated".into(),
        created_at: 1,
        updated_at: 1,
    })
    .unwrap();
    db.upsert_forward(&wt, 8080, 18080, "http://localhost:18080")
        .unwrap();
    db.upsert_share(&wt, 3000, 1, "tailscale", None, "up")
        .unwrap();
    db.conn()
        .execute(
            "INSERT INTO pr_queue (key, repo_root, number, worktree, branch, queued_at, updated_at)
             VALUES (?1, ?2, 7, ?3, 'feat', 1, 1)",
            params![format!("pr-{slug}"), repo, wt],
        )
        .unwrap();
    db.conn()
        .execute(
            "INSERT INTO automation_proposals (approval_issue, approval_stage, approval_sha,
               rule_id, repo_root, issue_id, provider, account, stage, base_sha,
               created_at_ms, expires_at_ms)
             VALUES (?1, 'code', 'sha', 'rule', ?2, 'X-1', 'linear', 'work', 'code', 'base', 1, 2)",
            params![format!("approval-{slug}"), repo],
        )
        .unwrap();
    let key = AutopilotIssueKey::new("linear", "work", &format!("AP-{slug}"));
    assert!(matches!(
        db.claim_autopilot(&key, repo, 5, 5, 1).unwrap(),
        ClaimOutcome::Claimed(_)
    ));
    (wt, dispatch)
}

fn target<'a>(repo: &'a str, slug: &'a str) -> ForgetWorkspace<'a> {
    ForgetWorkspace {
        repo_path: repo,
        slug: Some(slug),
        sidebar_scope: SCOPE,
    }
}

fn count(db: &Db, sql: &str, arg: &str) -> i64 {
    db.conn()
        .query_row(sql, params![arg], |row| row.get(0))
        .unwrap()
}

fn expected_counts() -> WorkspaceForgetCounts {
    WorkspaceForgetCounts {
        worktrees: 1,
        tab_groups: 2,
        pins: 1,
        collapse_keys: 1,
        merge_queue: 1,
        pr_queue: 1,
        dispatches_abandoned: 1,
        autopilot_stopped: 1,
        pool_spares: 1,
        pool_targets: 1,
        hibernations: 1,
        forwards: 1,
        shares: 1,
        automation_proposals: 1,
    }
}

#[test]
fn forget_prunes_every_owned_row_and_keeps_the_sibling_intact() {
    let db = Db::open_memory().unwrap();
    let (app_wt, app_dispatch) = seed(&db, "/r/app", "app");
    let (keep_wt, keep_dispatch) = seed(&db, "/r/keep", "keep");
    db.set_active_workspace("/r/app").unwrap();

    assert_eq!(
        db.workspace_forget_counts(&target("/r/app", "app"))
            .unwrap(),
        expected_counts()
    );
    let applied = db
        .forget_workspace_offline(&target("/r/app", "app"))
        .unwrap();
    assert_eq!(applied, expected_counts());

    // Registry and slug-keyed state are gone, the tombstone is written and
    // the active pointer cleared.
    let workspaces: Vec<String> = db
        .workspaces()
        .unwrap()
        .into_iter()
        .map(|row| row.repo_path)
        .collect();
    assert_eq!(workspaces, vec!["/r/keep".to_string()]);
    assert!(db.workspace_tombstoned("/r/app").unwrap());
    assert!(!db.workspace_tombstoned("/r/keep").unwrap());
    assert_eq!(db.repo_slug_lookup("/r/app").unwrap(), None);
    assert_eq!(db.active_workspace().unwrap(), None);
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*) FROM worktrees WHERE repo_path=?1",
            "/r/app"
        ),
        0
    );
    for session in ["s1", "s2"] {
        let names: Vec<String> = db
            .groups_for_session(session)
            .unwrap()
            .into_iter()
            .map(|row| row.name)
            .collect();
        assert_eq!(names, vec!["keep/feat".to_string()], "{session}");
    }
    assert_eq!(db.get_ui_state(SCOPE, "pin:app/feat").unwrap(), None);
    assert_eq!(db.get_ui_state(SCOPE, "collapse:app").unwrap(), None);

    // Background-work rows keyed to the forgotten workspace are gone.
    for (sql, arg) in [
        (
            "SELECT COUNT(*) FROM merge_queue WHERE worktree=?1",
            app_wt.as_str(),
        ),
        ("SELECT COUNT(*) FROM pr_queue WHERE repo_root=?1", "/r/app"),
        (
            "SELECT COUNT(*) FROM pool_spares WHERE repo_path=?1",
            "/r/app",
        ),
        (
            "SELECT COUNT(*) FROM pool_targets WHERE repo_path=?1",
            "/r/app",
        ),
        (
            "SELECT COUNT(*) FROM worktree_hibernations WHERE repo_path=?1",
            "/r/app",
        ),
        (
            "SELECT COUNT(*) FROM forwards WHERE worktree=?1",
            app_wt.as_str(),
        ),
        (
            "SELECT COUNT(*) FROM shares WHERE worktree=?1",
            app_wt.as_str(),
        ),
        (
            "SELECT COUNT(*) FROM automation_proposals WHERE repo_root=?1",
            "/r/app",
        ),
    ] {
        assert_eq!(count(&db, sql, arg), 0, "{sql}");
    }
    // History survives: merge events, the abandoned dispatch, the stopped run.
    assert!(
        count(
            &db,
            "SELECT COUNT(*) FROM merge_queue_events WHERE worktree=?1",
            &app_wt
        ) >= 1
    );
    assert_eq!(
        db.get_dispatch(app_dispatch).unwrap().unwrap().status,
        AgentDispatchStatus::Abandoned
    );
    let runs = db.list_autopilot_runs("/r/app", 10).unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].state, AutopilotState::Stopped);
    // Trust follows the repo; it is never forgotten.
    assert_eq!(db.repo_trust_list("/r/app").unwrap().len(), 1);

    // The kept workspace is fully intact.
    assert_eq!(
        db.workspace_forget_counts(&target("/r/keep", "keep"))
            .unwrap(),
        expected_counts()
    );
    assert_eq!(
        db.get_dispatch(keep_dispatch).unwrap().unwrap().status,
        AgentDispatchStatus::Queued
    );
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*) FROM merge_queue WHERE worktree=?1",
            &keep_wt
        ),
        1
    );
    assert_eq!(db.active_workspace().unwrap(), None);
    assert_eq!(db.repo_slug_lookup("/r/keep").unwrap(), Some("keep".into()));
}

#[test]
fn forget_counts_change_nothing() {
    let db = Db::open_memory().unwrap();
    seed(&db, "/r/app", "app");
    let before = count(
        &db,
        "SELECT COUNT(*) FROM worktrees WHERE repo_path=?1",
        "/r/app",
    );
    db.workspace_forget_counts(&target("/r/app", "app"))
        .unwrap();
    db.workspace_forget_counts(&target("/r/app", "app"))
        .unwrap();
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*) FROM worktrees WHERE repo_path=?1",
            "/r/app"
        ),
        before
    );
    assert!(!db.workspace_tombstoned("/r/app").unwrap());
}

#[test]
fn slug_prefix_never_touches_a_sibling_sharing_a_leading_substring() {
    let db = Db::open_memory().unwrap();
    seed(&db, "/r/api", "api");
    seed(&db, "/r/api-v2", "api-v2");
    db.forget_workspace_offline(&target("/r/api", "api"))
        .unwrap();
    assert_eq!(
        db.workspace_forget_counts(&target("/r/api-v2", "api-v2"))
            .unwrap(),
        expected_counts()
    );
}

#[test]
fn a_leased_pr_row_refuses_and_leaves_the_registry_untouched() {
    let db = Db::open_memory().unwrap();
    seed(&db, "/r/app", "app");
    db.conn()
        .execute(
            "UPDATE pr_queue SET agent_claim_owner='agent', agent_claim_lease_until_ms=?1
             WHERE key='pr-app'",
            params![crate::util::now_ms() + 600_000],
        )
        .unwrap();
    let error = db
        .forget_workspace_offline(&target("/r/app", "app"))
        .unwrap_err();
    assert!(error.downcast_ref::<PrQueueLeased>().is_some(), "{error:#}");
    assert!(error.to_string().contains("pr-app"));
    assert!(!db.workspace_tombstoned("/r/app").unwrap());
    let left = db
        .workspace_forget_counts(&target("/r/app", "app"))
        .unwrap();
    assert_eq!(left.dispatches_abandoned, 1, "refused before abandoning");
    assert_eq!(left.worktrees, 1);
    assert_eq!(left.tab_groups, 2);
    assert_eq!(left.merge_queue, 1);
}

#[test]
fn forget_without_a_slug_still_prunes_the_registry() {
    let db = Db::open_memory().unwrap();
    seed(&db, "/r/app", "app");
    let counts = db
        .forget_workspace_offline(&ForgetWorkspace {
            repo_path: "/r/app",
            slug: None,
            sidebar_scope: SCOPE,
        })
        .unwrap();
    assert_eq!(counts.tab_groups, 0);
    assert_eq!(counts.worktrees, 1);
    assert!(db.workspaces().unwrap().is_empty());
    assert_eq!(db.groups_for_session("s1").unwrap().len(), 1);
}

#[test]
fn forget_slug_prefers_the_persisted_slug_then_an_unclaimed_basename() {
    let db = Db::open_memory().unwrap();
    db.put_workspace("/a/lib", "lib", "repo").unwrap();
    assert_eq!(db.slug_for_repo("/a/lib", "lib").unwrap(), "lib");
    db.put_workspace("/b/lib", "lib", "repo").unwrap();
    assert_eq!(db.slug_for_repo("/b/lib", "lib").unwrap(), "lib-2");
    assert_eq!(db.forget_slug("/b/lib").unwrap(), Some("lib-2".into()));
    // No persisted slug: the basename is used only while nobody claimed it.
    assert_eq!(db.forget_slug("/c/lib").unwrap(), None);
    assert_eq!(
        db.forget_slug("/c/Tools.git").unwrap(),
        Some("tools".into())
    );
    assert_eq!(db.forget_slug("/").unwrap(), None);
}

#[test]
fn backup_copies_a_verified_database_and_refuses_an_existing_target() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_at(&dir.path().join("thegn.db")).unwrap();
    db.put_workspace("/r/app", "app", "repo").unwrap();
    let dest = dir.path().join("forget-backup-1").join("thegn.db");
    db.backup_to(&dest).unwrap();
    let copy = Db::open_at(&dest).unwrap();
    assert_eq!(copy.user_version().unwrap(), db.user_version().unwrap());
    assert_eq!(copy.workspaces().unwrap().len(), 1);
    let error = db.backup_to(&dest).unwrap_err();
    assert!(error.to_string().contains("already exists"), "{error:#}");
}

#[test]
fn nested_transaction_joins_the_outer_one() {
    let db = Db::open_memory().unwrap();
    let result: anyhow::Result<()> = db.transaction_immediate(|outer| {
        outer.transaction(|inner| {
            inner.put_workspace("/r/x", "x", "repo")?;
            Ok(())
        })?;
        anyhow::bail!("roll back the whole unit")
    });
    assert!(result.is_err());
    assert!(db.workspaces().unwrap().is_empty());
}
