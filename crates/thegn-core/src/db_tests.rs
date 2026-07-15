use super::*;
// Relocated domain methods now live behind store-seam traits (`db_*.rs`).
use crate::store::{
    AccountStore, CacheStore, NotificationStore, PoolStore, ProxyStore, WorkspaceStore,
    WorktreeAuxStore,
};
use rusqlite::params;

fn db() -> Db {
    Db::open_memory().unwrap()
}

#[test]
fn pool_spare_lifecycle_claim_and_target() {
    let db = db();
    assert!(db.pool_spares_for("/repo", "sprites").unwrap().is_empty());

    // Mint two spares (provisioning), then mark them ready.
    db.insert_pool_spare("repo-pool-1", "/repo", "sprites")
        .unwrap();
    db.insert_pool_spare("repo-pool-2", "/repo", "sprites")
        .unwrap();
    assert_eq!(db.pool_spares_for("/repo", "sprites").unwrap().len(), 2);
    db.set_pool_spare_ready("repo-pool-1", Some("cp-1"), "lock-abc")
        .unwrap();
    db.set_pool_spare_ready("repo-pool-2", Some("cp-2"), "lock-abc")
        .unwrap();

    // A worktree claims a ready spare: atomic mark + bind.
    db.put_worktree("tab", "/repo", "/wt/x", "sz/x", None, None)
        .unwrap();
    let claimed = db.claim_pool_spare("/repo", "sprites", "/wt/x").unwrap();
    let (name, cp) = claimed.expect("a ready spare is claimed");
    assert!(name.starts_with("repo-pool-"));
    assert!(cp.is_some(), "checkpoint id carried through");
    assert_eq!(
        db.worktree_provider_sandbox("/wt/x").unwrap().as_deref(),
        Some(name.as_str()),
        "worktree bound to the claimed spare"
    );
    // The claimed spare is no longer 'ready'; one ready spare remains.
    let ready = db
        .pool_spares_for("/repo", "sprites")
        .unwrap()
        .into_iter()
        .filter(|s| s.state == "ready")
        .count();
    assert_eq!(ready, 1);

    // Runtime target override round-trips (and clamps ≥ 0).
    assert!(db.pool_target("/repo", "sprites").unwrap().is_none());
    db.set_pool_target("/repo", "sprites", 3).unwrap();
    assert_eq!(db.pool_target("/repo", "sprites").unwrap(), Some(3));
    db.set_pool_target("/repo", "sprites", -5).unwrap();
    assert_eq!(db.pool_target("/repo", "sprites").unwrap(), Some(0));

    // Delete drops the row.
    db.delete_pool_spare(&name).unwrap();
    assert_eq!(db.pool_spares_for("/repo", "sprites").unwrap().len(), 1);
}

#[test]
fn shares_upsert_list_and_delete() {
    let db = db();
    assert!(db.list_shares().unwrap().is_empty());

    // Insert two shares on one worktree, plus one with no URL yet.
    db.upsert_share("/wt/a", 3000, "bore", Some("http://bore.pub:1"), "up")
        .unwrap();
    db.upsert_share("/wt/a", 8080, "bore", None, "starting")
        .unwrap();
    let rows = db.list_shares().unwrap();
    assert_eq!(rows.len(), 2);

    // Upsert updates state + url in place (no duplicate row).
    db.upsert_share("/wt/a", 8080, "bore", Some("http://bore.pub:2"), "up")
        .unwrap();
    let rows = db.list_shares().unwrap();
    assert_eq!(rows.len(), 2);
    let updated = rows
        .iter()
        .find(|r| r.local_port == 8080)
        .expect("port 8080");
    assert_eq!(updated.public_url.as_deref(), Some("http://bore.pub:2"));
    assert_eq!(updated.state, "up");
    assert_eq!(updated.provider, "bore");

    db.delete_share("/wt/a", 3000).unwrap();
    let rows = db.list_shares().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].local_port, 8080);
}

#[test]
fn forwards_upsert_list_and_delete() {
    let db = db();
    assert!(db.list_forwards().unwrap().is_empty());

    // Two forwards on one worktree: one keeps its port, one was remapped.
    db.upsert_forward("/wt/a", 3000, 3000, "http://127.0.0.1:3000")
        .unwrap();
    db.upsert_forward("/wt/a", 8080, 8001, "http://127.0.0.1:8001")
        .unwrap();
    assert_eq!(db.list_forwards().unwrap().len(), 2);

    // Upsert updates the host port + url in place (conflict re-remap).
    db.upsert_forward("/wt/a", 8080, 8002, "http://127.0.0.1:8002")
        .unwrap();
    let rows = db.list_forwards().unwrap();
    assert_eq!(rows.len(), 2);
    let updated = rows
        .iter()
        .find(|r| r.container_port == 8080)
        .expect("port 8080");
    assert_eq!(updated.host_port, 8002);
    assert_eq!(updated.url, "http://127.0.0.1:8002");

    db.delete_forward("/wt/a", 3000).unwrap();
    let rows = db.list_forwards().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].container_port, 8080);
}

#[test]
fn merge_queue_enqueue_update_list_and_remove() {
    let db = db();
    assert!(db.list_merge_queue().unwrap().is_empty());

    db.enqueue_merge("/wt/a", "feat-a", "main").unwrap();
    db.enqueue_merge("/wt/b", "feat-b", "main").unwrap();
    let q = db.list_merge_queue().unwrap();
    assert_eq!(q.len(), 2);
    // Oldest-queued first, all start `queued` with empty result/conflict.
    assert_eq!(q[0].worktree, "/wt/a");
    assert_eq!(q[0].status, "queued");
    assert!(q[0].result_oid.is_none() && q[0].conflict_paths.is_none());

    // Land one (records the merge-commit oid); defer the other (paths).
    db.update_merge_status("/wt/a", "landed", Some("deadbeef"), None, None)
        .unwrap();
    db.update_merge_status(
        "/wt/b",
        "deferred",
        None,
        Some("src/x.rs\nCargo.toml"),
        None,
    )
    .unwrap();
    let by = |wt: &str| -> MergeQueueRow {
        db.list_merge_queue()
            .unwrap()
            .into_iter()
            .find(|r| r.worktree == wt)
            .unwrap()
    };
    let a = by("/wt/a");
    assert_eq!(a.status, "landed");
    assert_eq!(a.result_oid.as_deref(), Some("deadbeef"));
    let b = by("/wt/b");
    assert_eq!(b.status, "deferred");
    assert_eq!(b.conflict_paths.as_deref(), Some("src/x.rs\nCargo.toml"));

    // COALESCE keeps the prior result_oid when a later update passes None.
    db.update_merge_status("/wt/a", "verifying", None, None, None)
        .unwrap();
    assert_eq!(by("/wt/a").result_oid.as_deref(), Some("deadbeef"));

    // Re-enqueue resets a deferred branch to a clean `queued` row.
    db.enqueue_merge("/wt/b", "feat-b", "main").unwrap();
    let b = by("/wt/b");
    assert_eq!(b.status, "queued");
    assert!(b.conflict_paths.is_none() && b.error_detail.is_none());

    db.remove_merge_entry("/wt/a").unwrap();
    let q = db.list_merge_queue().unwrap();
    assert_eq!(q.len(), 1);
    assert_eq!(q[0].worktree, "/wt/b");
}

#[test]
fn get_all_issue_cache_returns_every_provider_for_a_repo() {
    let db = db();
    assert!(db.get_all_issue_cache("/repo").unwrap().is_empty());
    db.put_issue_cache("/repo", "linear", "", "[1]").unwrap();
    db.put_issue_cache("/repo", "jira", "", "[2]").unwrap();
    db.put_issue_cache("/other", "github", "", "[3]").unwrap();
    let mut got = db.get_all_issue_cache("/repo").unwrap();
    got.sort();
    assert_eq!(
        got,
        vec![
            ("jira".to_string(), "[2]".to_string()),
            ("linear".to_string(), "[1]".to_string()),
        ]
    );
    // A different repo's providers are not mixed in.
    assert_eq!(db.get_all_issue_cache("/other").unwrap().len(), 1);
    // Two accounts of the same provider cache independently and both surface.
    db.put_issue_cache("/repo", "linear", "work", "[9]")
        .unwrap();
    assert_eq!(db.get_all_issue_cache("/repo").unwrap().len(), 3);
}

#[test]
fn accounts_crud_roundtrips() {
    let db = db();
    assert!(db.account_dir("codex", "work").unwrap().is_none());
    assert!(db.list_accounts("codex").unwrap().is_empty());

    db.put_account("codex", "work", "/creds/work", true, 10)
        .unwrap();
    db.put_account("codex", "personal", "/creds/personal", false, 5)
        .unwrap();
    assert_eq!(
        db.account_dir("codex", "work").unwrap().as_deref(),
        Some("/creds/work")
    );

    // last_used DESC ordering: touch personal so it floats above work.
    db.touch_account("codex", "personal", 99).unwrap();
    let names: Vec<String> = db
        .list_accounts("codex")
        .unwrap()
        .into_iter()
        .map(|(n, _, _)| n)
        .collect();
    assert_eq!(names, vec!["personal".to_string(), "work".to_string()]);
    // managed flag survives the round-trip.
    let managed: Vec<bool> = db
        .list_accounts("codex")
        .unwrap()
        .into_iter()
        .map(|(_, _, m)| m)
        .collect();
    assert_eq!(managed, vec![false, true]);

    // put is an upsert on (provider, name).
    db.put_account("codex", "work", "/creds/work2", true, 20)
        .unwrap();
    assert_eq!(
        db.account_dir("codex", "work").unwrap().as_deref(),
        Some("/creds/work2")
    );

    db.del_account("codex", "work").unwrap();
    assert!(db.account_dir("codex", "work").unwrap().is_none());
    assert_eq!(db.list_accounts("codex").unwrap().len(), 1);
}

#[test]
fn commit_cache_roundtrips_json_and_timestamp() {
    let db = db();
    assert!(db.get_commit_cache("/wt").unwrap().is_none());
    db.put_commit_cache("/wt", r#"[{"short":"abc1234"}]"#)
        .unwrap();
    let (json, fetched_at) = db.get_commit_cache("/wt").unwrap().unwrap();
    assert_eq!(json, r#"[{"short":"abc1234"}]"#);
    assert!(fetched_at > 0);
}

#[test]
fn transaction_commits_on_ok_and_passes_value_through() {
    let db = db();
    let n = db
        .transaction(|db| {
            db.touch_repo("/r/a", "a")?;
            db.touch_repo("/r/b", "b")?;
            Ok(42)
        })
        .unwrap();
    assert_eq!(n, 42);
    assert_eq!(db.recent_repos(10).unwrap().len(), 2);
}

#[test]
fn transaction_rolls_back_on_err() {
    let db = db();
    let res: Result<()> = db.transaction(|db| {
        db.touch_repo("/r/a", "a")?;
        anyhow::bail!("boom")
    });
    assert!(res.is_err());
    // The insert before the error must not be visible.
    assert!(db.recent_repos(10).unwrap().is_empty());
}

#[test]
fn tab_groups_roundtrip_ordered_by_ordinal() {
    use crate::models::{GroupTabRow, TabGroupRow};
    let db = db();
    let sess = "s1";
    let mk = |name: &str, ord: i64| TabGroupRow {
        name: name.into(),
        kind: "branch".into(),
        worktree: format!("/wt/{name}"),
        ordinal: ord,
        active_tab: 0,
    };
    let mktab = |group: &str, ord: i64| GroupTabRow {
        group_name: group.into(),
        ordinal: ord,
        title: (ord + 1).to_string(),
        pane_tree: r#"{"leaf":0}"#.into(),
        focused_pane: 0,
        pane_cwds: String::new(),
        pane_cmds: String::new(),
        pane_sessions: String::new(),
        scrollback_snapshot: String::new(),
    };
    // Insert out of order; expect ordinal ordering back.
    db.put_tab_group(sess, &mk("app/feat", 1)).unwrap();
    db.put_tab_group(sess, &mk("app/home", 0)).unwrap();
    db.put_group_tab(sess, &mktab("app/feat", 0)).unwrap();
    db.put_group_tab(sess, &mktab("app/feat", 1)).unwrap();
    db.put_group_tab(sess, &mktab("app/home", 0)).unwrap();
    let groups = db.groups_for_session(sess).unwrap();
    assert_eq!(groups.len(), 2);
    assert_eq!(groups[0].name, "app/home");
    assert_eq!(groups[1].name, "app/feat");
    let tabs = db.group_tabs_for_session(sess).unwrap();
    assert_eq!(tabs.len(), 3);

    // Upsert replaces in place (no duplicate row).
    db.put_tab_group(sess, &mk("app/feat", 5)).unwrap();
    let groups = db.groups_for_session(sess).unwrap();
    assert_eq!(groups.len(), 2);
    assert_eq!(
        groups
            .iter()
            .find(|g| g.name == "app/feat")
            .unwrap()
            .ordinal,
        5
    );

    // Delete removes the group and its tabs; other session is untouched.
    db.put_tab_group("other", &mk("x/home", 0)).unwrap();
    db.put_group_tab("other", &mktab("x/home", 0)).unwrap();
    db.delete_tab_group(sess, "app/feat").unwrap();
    assert_eq!(db.groups_for_session(sess).unwrap().len(), 1);
    assert_eq!(db.group_tabs_for_session(sess).unwrap().len(), 1);
    assert_eq!(db.groups_for_session("other").unwrap().len(), 1);

    // clear_session_layout wipes one session only.
    db.clear_session_layout(sess).unwrap();
    assert!(db.groups_for_session(sess).unwrap().is_empty());
    assert!(db.group_tabs_for_session(sess).unwrap().is_empty());
    assert_eq!(db.groups_for_session("other").unwrap().len(), 1);
}

#[test]
fn group_tab_pane_cwds_column_roundtrips() {
    use crate::models::GroupTabRow;
    let db = db();
    let sess = "s-cwd";
    db.put_group_tab(
        sess,
        &GroupTabRow {
            group_name: "app/home".into(),
            ordinal: 0,
            title: "1".into(),
            pane_tree: r#"{"leaf":0}"#.into(),
            focused_pane: 0,
            pane_cwds: r#"{"0":"/home/u/repo"}"#.into(),
            pane_cmds: r#"{"0":{"argv":["nvim"],"cwd":"/home/u/repo"}}"#.into(),
            pane_sessions: r#"{"0":{"provider":"sprites","id":"dev","session":"s-9"}}"#.into(),
            scrollback_snapshot: r#"{"0":"$ echo hi\nhi"}"#.into(),
        },
    )
    .unwrap();
    let tabs = db.group_tabs_for_session(sess).unwrap();
    assert_eq!(tabs.len(), 1);
    assert_eq!(tabs[0].pane_cwds, r#"{"0":"/home/u/repo"}"#);
    assert_eq!(
        tabs[0].pane_cmds,
        r#"{"0":{"argv":["nvim"],"cwd":"/home/u/repo"}}"#
    );
    assert_eq!(
        tabs[0].pane_sessions,
        r#"{"0":{"provider":"sprites","id":"dev","session":"s-9"}}"#
    );
    assert_eq!(tabs[0].scrollback_snapshot, r#"{"0":"$ echo hi\nhi"}"#);

    // An upsert overwrites the cwd + cmd + session maps (no stale merge).
    db.put_group_tab(
        sess,
        &GroupTabRow {
            group_name: "app/home".into(),
            ordinal: 0,
            title: "1".into(),
            pane_tree: r#"{"leaf":0}"#.into(),
            focused_pane: 0,
            pane_cwds: String::new(),
            pane_cmds: String::new(),
            pane_sessions: String::new(),
            scrollback_snapshot: String::new(),
        },
    )
    .unwrap();
    let back = db.group_tabs_for_session(sess).unwrap();
    assert_eq!(back[0].pane_cwds, "");
    assert_eq!(back[0].pane_cmds, "");
    assert_eq!(back[0].pane_sessions, "");
    assert_eq!(back[0].scrollback_snapshot, "");
}

#[test]
fn active_workspace_pointer_roundtrips() {
    let db = db();
    assert_eq!(db.active_workspace().unwrap(), None);
    db.set_active_workspace("/home/u/repo-a").unwrap();
    assert_eq!(
        db.active_workspace().unwrap().as_deref(),
        Some("/home/u/repo-a")
    );
    // Pointer is a single global slot: a later switch overwrites it.
    db.set_active_workspace("/home/u/repo-b").unwrap();
    assert_eq!(
        db.active_workspace().unwrap().as_deref(),
        Some("/home/u/repo-b")
    );
}

#[test]
fn split_page_suffix_cases() {
    assert_eq!(split_page_suffix("app/feat"), ("app/feat", None));
    assert_eq!(split_page_suffix("app/feat ·2"), ("app/feat", Some(2)));
    assert_eq!(split_page_suffix("app/feat ·x"), ("app/feat ·x", None));
    assert_eq!(split_page_suffix(" ·2"), (" ·2", None));
}

/// Build a legacy v5 DB file by hand (raw SQL, no Db API), then open it via
/// `Db::open_at` and assert the v6 transform.
#[test]
fn migrates_v5_tab_layout_into_groups() {
    let dir = std::env::temp_dir().join(format!("sz-db-mig-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("db.sqlite");
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            r#"
            PRAGMA user_version = 5;
            CREATE TABLE tab_layout (
              session_name TEXT, tab_name TEXT, kind TEXT, worktree TEXT,
              pane_tree TEXT, ordinal INTEGER, focused_pane INTEGER,
              PRIMARY KEY (session_name, tab_name));
            CREATE TABLE session_state (
              session_name TEXT PRIMARY KEY, active_tab TEXT, updated_at INTEGER);
            INSERT INTO tab_layout VALUES
              ('/r', 'app/home',    'home',     '/r',        '{"leaf":0}', 0, 0),
              ('/r', 'app/feat',    'worktree', '/wt/feat',  '{"leaf":1}', 1, 1),
              ('/r', 'app/feat ·2', 'worktree', '/wt/feat',  '{"leaf":2}', 2, 2),
              ('/r', 'scratch',     'extra',    '',          '{"leaf":3}', 3, 0),
              ('/q', 'q/home',      'home',     '/q',        '{"leaf":0}', 0, 0);
            INSERT INTO session_state VALUES ('/r', 'app/feat ·2', 1);
            "#,
        )
        .unwrap();
    }
    let db = Db::open_at(&path).unwrap();

    // Legacy table is gone; groups exist per base name.
    let groups = db.groups_for_session("/r").unwrap();
    assert_eq!(
        groups.iter().map(|g| g.name.as_str()).collect::<Vec<_>>(),
        vec!["app/home", "app/feat", "scratch"]
    );
    let feat = groups.iter().find(|g| g.name == "app/feat").unwrap();
    assert_eq!(feat.kind, "branch");
    assert_eq!(feat.worktree, "/wt/feat");
    assert_eq!(feat.active_tab, 1, "active page ·2 became tab index 1");
    assert_eq!(groups[0].kind, "home");

    let tabs = db.group_tabs_for_session("/r").unwrap();
    let feat_tabs: Vec<_> = tabs.iter().filter(|t| t.group_name == "app/feat").collect();
    assert_eq!(feat_tabs.len(), 2);
    assert_eq!(feat_tabs[0].title, "1");
    assert_eq!(feat_tabs[0].pane_tree, r#"{"leaf":1}"#);
    assert_eq!(feat_tabs[1].pane_tree, r#"{"leaf":2}"#);
    assert_eq!(feat_tabs[1].focused_pane, 2);

    // The session's active marker now names the group.
    assert_eq!(db.active_tab("/r").unwrap().as_deref(), Some("app/feat"));
    // The untouched session migrated too.
    assert_eq!(db.groups_for_session("/q").unwrap().len(), 1);

    // Re-open: migration is idempotent (legacy table is gone).
    drop(db);
    let db = Db::open_at(&path).unwrap();
    assert_eq!(db.groups_for_session("/r").unwrap().len(), 3);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn active_tab_persists_per_session() {
    let db = db();
    assert_eq!(db.active_tab("s").unwrap(), None);
    db.set_active_tab("s", "app/feat", 100).unwrap();
    assert_eq!(db.active_tab("s").unwrap().as_deref(), Some("app/feat"));
    // Upsert moves it.
    db.set_active_tab("s", "app/home", 200).unwrap();
    assert_eq!(db.active_tab("s").unwrap().as_deref(), Some("app/home"));
}

#[test]
fn pin_state_persists_without_clobbering_active_tab() {
    let db = db();
    assert_eq!(db.pin_state("s").unwrap(), None);
    // active_tab and pin_state coexist in the same row, set independently.
    db.set_active_tab("s", "app/home", 10).unwrap();
    db.set_pin_state("s", r#"[{"name":"mail","placement":"float"}]"#, 20)
        .unwrap();
    assert_eq!(db.active_tab("s").unwrap().as_deref(), Some("app/home"));
    assert_eq!(
        db.pin_state("s").unwrap().as_deref(),
        Some(r#"[{"name":"mail","placement":"float"}]"#)
    );
    // Updating pin_state leaves active_tab intact.
    db.set_pin_state("s", "[]", 30).unwrap();
    assert_eq!(db.active_tab("s").unwrap().as_deref(), Some("app/home"));
    assert_eq!(db.pin_state("s").unwrap().as_deref(), Some("[]"));
}

#[test]
fn palette_usage_accumulates_and_reports() {
    let db = db();
    assert!(db.palette_usage().unwrap().is_empty());
    // First bump inserts; subsequent bumps increment the count in place.
    db.bump_palette_usage("new-worktree").unwrap();
    db.bump_palette_usage("new-worktree").unwrap();
    db.bump_palette_usage("diff").unwrap();
    let usage = db.palette_usage().unwrap();
    assert_eq!(usage.len(), 2, "one row per distinct key");
    let by_key: std::collections::HashMap<_, _> = usage
        .iter()
        .map(|(k, c, l)| (k.as_str(), (*c, *l)))
        .collect();
    assert_eq!(
        by_key["new-worktree"].0, 2,
        "repeated bump increments count"
    );
    assert_eq!(by_key["diff"].0, 1);
    // last_used is stamped (non-zero) on every key.
    assert!(by_key["new-worktree"].1 > 0 && by_key["diff"].1 > 0);
}

#[test]
fn repos_recents_order_by_seq() {
    let db = db();
    db.touch_repo("/a", "a").unwrap();
    db.touch_repo("/b", "b").unwrap();
    db.touch_repo("/a", "a").unwrap(); // re-open bumps seq
    let recents = db.recent_repos(10).unwrap();
    assert_eq!(recents, vec!["/a".to_string(), "/b".to_string()]);
    assert!(db.recent_repos(1).unwrap().len() == 1);
    assert!(db.is_known_repo("/a").unwrap());
    assert!(!db.is_known_repo("/nope").unwrap());
    assert!(db.known_repos().unwrap().contains(&"/b".to_string()));
}

#[test]
fn workspaces_roundtrip() {
    let db = db();
    db.put_workspace("/repo", "repo", "repo").unwrap();
    db.put_workspace("/repo", "repo2", "repo").unwrap(); // upsert renames
    let ws = db.workspaces().unwrap();
    assert_eq!(ws.len(), 1);
    assert_eq!(ws[0].repo_path, "/repo");
    assert_eq!(ws[0].name, "repo2");
    assert_eq!(ws[0].kind, "repo");
    assert!(db.is_known_repo("/repo").unwrap());
}

#[test]
fn del_workspace_forgets_workspace_and_its_worktrees() {
    let db = db();
    db.put_workspace("/repo", "repo", "repo").unwrap();
    db.put_worktree("repo/main", "/repo", "/repo", "main", None, None)
        .unwrap();
    db.put_worktree("repo/feat", "/repo", "/repo/wt-feat", "feat", None, None)
        .unwrap();
    // A second, unrelated workspace must survive the removal.
    db.put_workspace("/other", "other", "repo").unwrap();
    db.put_worktree("other/main", "/other", "/other", "main", None, None)
        .unwrap();
    let slug = db.slug_for_repo("/repo", "repo").unwrap();

    db.del_worktrees_for_repo("/repo").unwrap();
    db.del_workspace("/repo").unwrap();
    db.del_repo_slug("/repo").unwrap();

    let ws = db.workspaces().unwrap();
    assert_eq!(ws.len(), 1);
    assert_eq!(ws[0].repo_path, "/other");
    assert!(!db.is_known_repo("/repo").unwrap());
    // The other repo's worktree row is untouched; the removed repo's are gone.
    let wts = db.worktrees().unwrap();
    assert!(wts.iter().all(|w| w.repo_root != "/repo"));
    assert!(wts.iter().any(|w| w.repo_root == "/other"));
    // A reopened repo re-derives a fresh slug rather than a stale one.
    assert_eq!(slug, "repo");
}

#[test]
fn workspace_kind_is_insert_only() {
    let db = db();
    db.put_workspace("/d", "d", "dir").unwrap();
    // A later refresh passing "repo" must not downgrade an existing dir.
    db.put_workspace("/d", "d", "repo").unwrap();
    assert_eq!(db.workspaces().unwrap()[0].kind, "dir");
}

#[test]
fn workspace_position_default_is_insert_order() {
    let db = db();
    // Inserted a, b, c — `workspaces()` returns them in that insert order
    // (the appending MAX+1 position), independent of last_active.
    db.put_workspace("/a", "a", "repo").unwrap();
    db.put_workspace("/b", "b", "repo").unwrap();
    db.put_workspace("/c", "c", "repo").unwrap();
    let order: Vec<String> = db
        .workspaces()
        .unwrap()
        .into_iter()
        .map(|w| w.repo_path)
        .collect();
    assert_eq!(order, vec!["/a", "/b", "/c"]);

    // Re-registering an existing workspace (upsert) keeps its position — a
    // metadata refresh must never reshuffle the sidebar.
    db.put_workspace("/a", "a-renamed", "repo").unwrap();
    let order: Vec<String> = db
        .workspaces()
        .unwrap()
        .into_iter()
        .map(|w| w.repo_path)
        .collect();
    assert_eq!(
        order,
        vec!["/a", "/b", "/c"],
        "upsert must preserve position"
    );
}

#[test]
fn swap_workspace_positions_reorders() {
    let db = db();
    db.put_workspace("/a", "a", "repo").unwrap();
    db.put_workspace("/b", "b", "repo").unwrap();
    db.put_workspace("/c", "c", "repo").unwrap();

    // Swap the first two: order becomes b, a, c.
    db.swap_workspace_positions("/a", "/b").unwrap();
    let order: Vec<String> = db
        .workspaces()
        .unwrap()
        .into_iter()
        .map(|w| w.repo_path)
        .collect();
    assert_eq!(order, vec!["/b", "/a", "/c"]);

    // set_workspace_position is the persist-side primitive; floating c to a
    // fresh min puts it first.
    db.set_workspace_position("/c", -1).unwrap();
    let first = db
        .workspaces()
        .unwrap()
        .into_iter()
        .next()
        .unwrap()
        .repo_path;
    assert_eq!(first, "/c");
}

#[test]
fn set_workspace_order_writes_exact_sequence_even_from_null_positions() {
    // The sidebar reorder persists the whole on-screen order (not a two-position
    // swap): `db.workspaces()` must then return that exact sequence, so the
    // Shift+Alt nav ring — rebuilt from `workspaces()` on hydration — walks the
    // order the user arranged. This must hold even when rows start with NULL /
    // tied positions (migrate_brand / db_zones inserts), the case where a
    // swap+normalize could seed a different tiebreak order than the tree shows.
    let db = db();
    for p in ["/a", "/b", "/c"] {
        db.conn()
            .execute("INSERT INTO workspaces (repo_path) VALUES (?1)", params![p])
            .unwrap();
    }
    let order = |db: &Db| -> Vec<String> {
        db.workspaces()
            .unwrap()
            .into_iter()
            .map(|w| w.repo_path)
            .collect()
    };

    let arranged = vec!["/c".to_string(), "/a".to_string(), "/b".to_string()];
    db.set_workspace_order(&arranged).unwrap();
    assert_eq!(
        order(&db),
        arranged,
        "reload matches the arranged order verbatim"
    );

    // A second arrangement over the now-contiguous positions round-trips too.
    let arranged2 = vec!["/b".to_string(), "/c".to_string(), "/a".to_string()];
    db.set_workspace_order(&arranged2).unwrap();
    assert_eq!(order(&db), arranged2);
}

#[test]
fn swap_workspace_positions_heals_null_and_duplicate_positions() {
    // Regression: some insert paths (migrate_brand, db_zones) create workspace
    // rows without a `position` (NULL), and ties in the backfill can leave
    // duplicates. `swap_workspace_positions` used to silently no-op when either
    // side was NULL or the two positions were equal, so a sidebar reorder never
    // reached the DB — and the nav ring, rebuilt from `db.workspaces()` order on
    // the next hydration, kept walking the original order. It must instead
    // normalize to contiguous distinct positions and actually swap.
    let db = db();
    // Insert bypassing `put_workspace`, mirroring migrate_brand.rs — no position.
    for p in ["/a", "/b", "/c"] {
        db.conn()
            .execute("INSERT INTO workspaces (repo_path) VALUES (?1)", params![p])
            .unwrap();
    }
    let order = |db: &Db| -> Vec<String> {
        db.workspaces()
            .unwrap()
            .into_iter()
            .map(|w| w.repo_path)
            .collect()
    };
    let before = order(&db);

    // Swapping two NULL-position rows must reorder them, not no-op.
    db.swap_workspace_positions(&before[0], &before[1]).unwrap();
    let after = order(&db);
    assert_eq!(
        after,
        vec![before[1].clone(), before[0].clone(), before[2].clone()],
        "NULL-position workspaces must still swap"
    );

    // And the persisted positions are now contiguous + distinct, so subsequent
    // swaps keep working.
    db.swap_workspace_positions(&after[1], &after[2]).unwrap();
    assert_eq!(
        order(&db),
        vec![after[0].clone(), after[2].clone(), after[1].clone()],
        "second swap on healed positions reorders too"
    );

    // Duplicate (equal, non-NULL) positions are the same hazard — a value-swap
    // is a no-op — and must heal + reorder just the same.
    let db = Db::open_memory().unwrap();
    for p in ["/x", "/y", "/z"] {
        db.conn()
            .execute(
                "INSERT INTO workspaces (repo_path, position) VALUES (?1, 0)",
                params![p],
            )
            .unwrap();
    }
    let dup_before = order(&db);
    db.swap_workspace_positions(&dup_before[0], &dup_before[1])
        .unwrap();
    assert_eq!(
        order(&db),
        vec![
            dup_before[1].clone(),
            dup_before[0].clone(),
            dup_before[2].clone()
        ],
        "duplicate-position workspaces must still swap"
    );
}

#[test]
fn swap_worktree_positions_heals_null_and_duplicate_positions() {
    // Worktree parity with the workspace swap: the same NULL/duplicate hazard
    // (v8-added column, inserts that miss it) must not make a manual reorder
    // silently no-op.
    let db = db();
    db.put_workspace("/repo", "repo", "repo").unwrap();
    for wt in ["/wt/a", "/wt/b", "/wt/c"] {
        db.put_worktree("repo/x", "/repo", wt, "x", None, None)
            .unwrap();
    }
    // Blank the positions to the legacy NULL state a manual reorder used to
    // fail to persist against.
    db.conn()
        .execute("UPDATE worktrees SET position = NULL", [])
        .unwrap();
    let order = |db: &Db| -> Vec<String> {
        db.worktrees()
            .unwrap()
            .into_iter()
            .map(|w| w.worktree)
            .collect()
    };
    let before = order(&db);
    db.swap_worktree_positions(&before[0], &before[1]).unwrap();
    assert_eq!(
        order(&db),
        vec![before[1].clone(), before[0].clone(), before[2].clone()],
        "NULL-position worktrees must still swap"
    );
}

#[test]
fn migrates_workspaces_position_from_recency() {
    // A pre-v16 `workspaces` table (no `position` column): the migration
    // ALTERs the column in and backfills it so the most-recently-active
    // workspace sorts first — preserving the old recency order on the first
    // launch after upgrade.
    let dir = std::env::temp_dir().join(format!("sz-db-ws-mig-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("db.sqlite");
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            r#"
            PRAGMA user_version = 15;
            CREATE TABLE workspaces (
              repo_path TEXT PRIMARY KEY, name TEXT,
              created_at INTEGER, last_active INTEGER);
            INSERT INTO workspaces(repo_path,name,created_at,last_active) VALUES
              ('/old',    'old',    1, 100),
              ('/newest', 'newest', 1, 300),
              ('/mid',    'mid',    1, 200);
            "#,
        )
        .unwrap();
    }
    let db = Db::open_at(&path).unwrap();
    let order: Vec<String> = db
        .workspaces()
        .unwrap()
        .into_iter()
        .map(|w| w.repo_path)
        .collect();
    assert_eq!(
        order,
        vec!["/newest", "/mid", "/old"],
        "backfill must rank position 0 = most-recently-active"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn registers_roundtrip_on_fresh_db() {
    let db = Db::open_memory().unwrap();
    assert!(db.all_registers().unwrap().is_empty());
    db.put_register('a', "hello").unwrap();
    db.put_register('1', "world").unwrap();
    db.put_register('a', "hello again").unwrap(); // upsert
    let mut got = db.all_registers().unwrap();
    got.sort();
    assert_eq!(
        got,
        vec![('1', "world".to_string()), ('a', "hello again".to_string())]
    );
}

#[test]
fn migrates_registers_additive_from_v26() {
    // A pre-v27 DB (no `registers` table): opening it creates the table
    // additively without touching existing data.
    let dir = std::env::temp_dir().join(format!("sz-db-reg-mig-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("db.sqlite");
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            r#"
            PRAGMA user_version = 26;
            CREATE TABLE repos (path TEXT PRIMARY KEY, name TEXT);
            INSERT INTO repos(path,name) VALUES ('/keep','keep');
            "#,
        )
        .unwrap();
    }
    let db = Db::open_at(&path).unwrap();
    // Existing data survives …
    let kept: i64 = db
        .conn
        .query_row("SELECT COUNT(*) FROM repos WHERE path='/keep'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(kept, 1);
    // … and the new registers table is usable.
    db.put_register('z', "migrated").unwrap();
    assert_eq!(
        db.all_registers().unwrap(),
        vec![('z', "migrated".to_string())]
    );
    let ver: i64 = db
        .conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(ver, SCHEMA_VERSION);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn slug_reuse_and_collision_suffix() {
    let db = db();
    // First repo takes the bare base.
    assert_eq!(db.slug_for_repo("/x/app", "app").unwrap(), "app");
    // Same repo reuses its slug.
    assert_eq!(db.slug_for_repo("/x/app", "app").unwrap(), "app");
    // Different repo, same basename → suffixed.
    assert_eq!(db.slug_for_repo("/y/app", "app").unwrap(), "app-2");
    assert_eq!(db.slug_for_repo("/z/app", "app").unwrap(), "app-3");
}

#[test]
fn pr_branch_cache_roundtrip_and_upsert() {
    let db = db();
    assert!(db.get_pr_branch_cache("/repo").unwrap().is_none());
    db.put_pr_branch_cache("/repo", "[{\"number\":1}]").unwrap();
    let (json, at) = db.get_pr_branch_cache("/repo").unwrap().unwrap();
    assert_eq!(json, "[{\"number\":1}]");
    assert!(at > 0);
    db.put_pr_branch_cache("/repo", "[]").unwrap();
    assert_eq!(db.get_pr_branch_cache("/repo").unwrap().unwrap().0, "[]");
}

#[test]
fn worktree_disk_roundtrip_upsert_bulk_and_delete() {
    let db = db();
    assert!(db.get_worktree_disk("/wt/a").unwrap().is_none());
    assert!(db.all_worktree_disk().unwrap().is_empty());

    db.put_worktree_disk("/wt/a", 5_000, 4_200).unwrap();
    db.put_worktree_disk("/wt/b", 1_000, 900).unwrap();
    let (size, target, at) = db.get_worktree_disk("/wt/a").unwrap().unwrap();
    assert_eq!((size, target), (5_000, 4_200));
    assert!(at > 0);

    // Upsert overwrites in place.
    db.put_worktree_disk("/wt/a", 9_000, 8_000).unwrap();
    assert_eq!(db.get_worktree_disk("/wt/a").unwrap().unwrap().0, 9_000);

    let all = db.all_worktree_disk().unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(all.get("/wt/a"), Some(&(9_000, 8_000)));
    assert_eq!(all.get("/wt/b"), Some(&(1_000, 900)));

    db.delete_worktree_disk("/wt/a").unwrap();
    assert!(db.get_worktree_disk("/wt/a").unwrap().is_none());
    assert_eq!(db.all_worktree_disk().unwrap().len(), 1);
}

#[test]
fn open_pr_counts_by_branch_counts_only_open_prs() {
    let db = db();
    // No cache yet → empty map.
    assert!(db.get_open_pr_counts_by_branch("/repo").unwrap().is_empty());

    // Two open PRs on `feat`, one merged on `feat`, one open on `fix`.
    let json = r#"[
        {"number":1,"headRefName":"feat","state":"OPEN","url":"u1","isDraft":false},
        {"number":2,"headRefName":"feat","state":"OPEN","url":"u2","isDraft":false},
        {"number":3,"headRefName":"feat","state":"MERGED","url":"u3","isDraft":false},
        {"number":4,"headRefName":"fix","state":"OPEN","url":"u4","isDraft":false}
    ]"#;
    db.put_pr_branch_cache("/repo", json).unwrap();
    let counts = db.get_open_pr_counts_by_branch("/repo").unwrap();
    assert_eq!(counts.get("feat"), Some(&2), "two OPEN PRs on feat");
    assert_eq!(counts.get("fix"), Some(&1), "one OPEN PR on fix");
    assert_eq!(counts.len(), 2, "merged/closed PRs are excluded");
}

#[test]
fn open_pr_counts_by_branch_handles_garbled_cache() {
    let db = db();
    db.put_pr_branch_cache("/repo", "not json").unwrap();
    assert!(db.get_open_pr_counts_by_branch("/repo").unwrap().is_empty());
}

#[test]
fn undo_marks_record_dedupe_and_cap() {
    let db = db();
    assert!(db.undo_marks("/wt").unwrap().is_empty());
    db.add_undo_mark("/wt", "aaa").unwrap();
    db.add_undo_mark("/wt", "bbb").unwrap();
    db.add_undo_mark("/wt", "aaa").unwrap(); // refresh, not duplicate
    let marks = db.undo_marks("/wt").unwrap();
    assert_eq!(marks.len(), 2);
    // Other worktrees are isolated.
    assert!(db.undo_marks("/other").unwrap().is_empty());
    // Cap: 110 inserts keep only the freshest 100.
    for i in 0..110 {
        db.add_undo_mark("/cap", &format!("sha{i:03}")).unwrap();
    }
    assert_eq!(db.undo_marks("/cap").unwrap().len(), 100);
}

#[test]
fn pr_and_diff_caches() {
    let db = db();
    assert!(db.get_pr_cache("/wt").unwrap().is_none());
    db.put_pr_cache("/wt", "br", "{\"k\":1}").unwrap();
    let (json, at) = db.get_pr_cache("/wt").unwrap().unwrap();
    assert_eq!(json, "{\"k\":1}");
    assert!(at > 0);
    db.put_pr_cache("/wt", "br", "{\"k\":2}").unwrap(); // upsert
    assert_eq!(db.get_pr_cache("/wt").unwrap().unwrap().0, "{\"k\":2}");

    assert!(db.get_diff_cache("/wt").unwrap().is_none());
    db.put_diff_cache("/wt", "M\tfile.rs").unwrap();
    assert_eq!(db.get_diff_cache("/wt").unwrap().unwrap().0, "M\tfile.rs");

    assert!(db.get_test_cache("/wt").unwrap().is_none());
    db.put_test_cache("/wt", "{\"summary\":\"ok\"}").unwrap();
    assert_eq!(
        db.get_test_cache("/wt").unwrap().unwrap().0,
        "{\"summary\":\"ok\"}"
    );
    db.put_test_cache("/wt", "{\"summary\":\"fail\"}").unwrap();
    assert_eq!(
        db.get_test_cache("/wt").unwrap().unwrap().0,
        "{\"summary\":\"fail\"}"
    );

    // ci cache: miss → insert → upsert.
    assert!(db.get_ci_cache("/wt").unwrap().is_none());
    db.put_ci_cache("/wt", "br", "[{\"id\":\"1\"}]").unwrap();
    let (cj, cat) = db.get_ci_cache("/wt").unwrap().unwrap();
    assert_eq!(cj, "[{\"id\":\"1\"}]");
    assert!(cat > 0);
    db.put_ci_cache("/wt", "br", "[{\"id\":\"2\"}]").unwrap(); // upsert
    assert_eq!(
        db.get_ci_cache("/wt").unwrap().unwrap().0,
        "[{\"id\":\"2\"}]"
    );

    // loc cache: miss → insert → upsert (the report JSON round-trips).
    let loc_json = |db: &Db| db.get_loc_cache_entry("/wt").unwrap().map(|(j, _)| j);
    assert!(loc_json(&db).is_none());
    db.put_loc_cache("/wt", 123, "{\"c\":123}").unwrap();
    assert_eq!(loc_json(&db).as_deref(), Some("{\"c\":123}"));
    db.put_loc_cache("/wt", 456, "{\"c\":456}").unwrap();
    assert_eq!(loc_json(&db).as_deref(), Some("{\"c\":456}"));
}

#[test]
fn worktree_crud() {
    let db = db();
    db.put_worktree("app/feat", "/x/app", "/wt/feat", "sz/feat", None, None)
        .unwrap();
}

#[test]
fn folder_crud() {
    let db = db();
    db.put_workspace("/x/app", "app", "repo").unwrap();

    let f1 = db.create_folder("/x/app", "Features").unwrap();
    let f2 = db.create_folder("/x/app", "Bugs").unwrap();

    let folders = db.folders_for_workspace("/x/app").unwrap();
    assert_eq!(folders.len(), 2);
    assert_eq!(folders[0].name, "Features");
    assert_eq!(folders[0].folder_id, f1);
    assert_eq!(folders[1].name, "Bugs");
    assert_eq!(folders[1].folder_id, f2);
    assert!(folders[0].position < folders[1].position);

    db.rename_folder(f1, "Feat").unwrap();
    let folders2 = db.folders_for_workspace("/x/app").unwrap();
    assert_eq!(folders2[0].name, "Feat");

    db.del_folder(f2).unwrap();
    let folders3 = db.folders_for_workspace("/x/app").unwrap();
    assert_eq!(folders3.len(), 1);
    assert_eq!(folders3[0].folder_id, f1);
}

#[test]
fn ensure_folder_creates_then_reuses() {
    let db = db();
    db.put_workspace("/x/app", "app", "repo").unwrap();

    let a = db.ensure_folder("/x/app", "Ready to merge").unwrap();
    // Same name (case/whitespace-insensitive) reuses the row, never dups.
    let b = db.ensure_folder("/x/app", "  ready TO merge ").unwrap();
    assert_eq!(a, b);
    assert_eq!(db.folders_for_workspace("/x/app").unwrap().len(), 1);

    // A different name creates a second folder.
    let c = db.ensure_folder("/x/app", "PRing").unwrap();
    assert_ne!(a, c);
    assert_eq!(db.folders_for_workspace("/x/app").unwrap().len(), 2);
}

#[test]
fn set_worktree_folder_round_trips() {
    let db = db();
    db.put_workspace("/x/app", "app", "repo").unwrap();
    db.put_worktree("app/feat", "/x/app", "/wt/feat", "sz/feat", None, None)
        .unwrap();
    let fid = db.ensure_folder("/x/app", "Ready to merge").unwrap();

    db.set_worktree_folder("/wt/feat", Some(fid)).unwrap();
    let row = db
        .worktrees()
        .unwrap()
        .into_iter()
        .find(|w| w.worktree == "/wt/feat")
        .unwrap();
    assert_eq!(row.folder_id, Some(fid));

    // Unfiling clears it.
    db.set_worktree_folder("/wt/feat", None).unwrap();
    let row = db
        .worktrees()
        .unwrap()
        .into_iter()
        .find(|w| w.worktree == "/wt/feat")
        .unwrap();
    assert_eq!(row.folder_id, None);
}

#[test]
fn worktree_crud2() {
    let db = db();
    db.put_worktree("app/feat", "/x/app", "/wt/feat", "sz/feat", None, None)
        .unwrap();
    db.set_worktree_sandbox("/wt/feat", "podman").unwrap();
    let sb = db.worktree_sandbox("/wt/feat").unwrap();
    assert_eq!(sb, Some("podman".to_string()));

    // metadata round-trips
    let all = db.worktrees().unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].worktree, "/wt/feat");
    assert_eq!(all[0].branch, "sz/feat");
    assert_eq!(all[0].repo_root, "/x/app");
    // tab → worktree mapping uses the recorded session.
    let sess = session();
    assert_eq!(
        db.worktree_for_tab(&sess, "app/feat").unwrap().as_deref(),
        Some("/wt/feat")
    );
    assert_eq!(
        db.repo_root_for("/wt/feat").unwrap().as_deref(),
        Some("/x/app")
    );
    // agent: empty → None, then set → Some.
    assert!(db.worktree_agent("/wt/feat").unwrap().is_none());
    db.set_worktree_agent("/wt/feat", "claude").unwrap();
    assert_eq!(
        db.worktree_agent("/wt/feat").unwrap().as_deref(),
        Some("claude")
    );
    // location: none by default; set via upsert.
    assert!(
        db.location_for("/wt/feat")
            .unwrap()
            .map(|s| s.is_empty())
            .unwrap_or(true)
    );
    db.put_worktree(
        "app/feat",
        "/x/app",
        "/wt/feat-renamed-on-disk",
        "sz/feat",
        Some("{\"host\":\"box\"}"),
        None,
    )
    .unwrap();
    db.del_worktree_for_tab("/x/app", "app/feat").unwrap();
    assert!(
        db.worktrees().unwrap().is_empty(),
        "closing/deleting a worktree group must forget registry rows even if the path changed"
    );

    db.put_worktree("app/other", "/x/app", "/wt/other", "sz/other", None, None)
        .unwrap();
    // delete
    db.del_worktree("/wt/other").unwrap();
    assert!(db.worktrees().unwrap().is_empty());
}

#[test]
fn layouts_crud_roundtrip() {
    let db = db();
    assert!(db.list_layouts().unwrap().is_empty());
    assert!(db.get_layout("dev").unwrap().is_none());

    db.put_layout("dev", "{\"k\":1}").unwrap();
    db.put_layout("review", "{\"k\":2}").unwrap();
    assert_eq!(db.get_layout("dev").unwrap().as_deref(), Some("{\"k\":1}"));
    // Alphabetical listing.
    assert_eq!(db.list_layouts().unwrap(), vec!["dev", "review"]);

    // Upsert replaces the spec in place.
    db.put_layout("dev", "{\"k\":9}").unwrap();
    assert_eq!(db.get_layout("dev").unwrap().as_deref(), Some("{\"k\":9}"));
    assert_eq!(db.list_layouts().unwrap().len(), 2);

    db.delete_layout("dev").unwrap();
    assert_eq!(db.list_layouts().unwrap(), vec!["review"]);
    db.delete_layout("missing").unwrap(); // no-op
}

#[test]
fn rename_worktree_rekeys_path_tab_and_branch() {
    let db = db();
    db.put_worktree("app/old", "/x/app", "/wt/old", "old", None, None)
        .unwrap();
    db.set_worktree_position("/wt/old", 7).unwrap();
    db.rename_worktree("/wt/old", "/wt/new", "app/new", "new")
        .unwrap();
    let rows = db.worktrees().unwrap();
    assert_eq!(rows.len(), 1);
    let w = &rows[0];
    assert_eq!(w.worktree, "/wt/new");
    assert_eq!(w.tab_name, "app/new");
    assert_eq!(w.branch, "new");
    assert_eq!(w.position, 7, "position is preserved across rename");
    // Renaming a missing row is a no-op (no panic, no insert).
    db.rename_worktree("/wt/missing", "/wt/x", "app/x", "x")
        .unwrap();
    assert_eq!(db.worktrees().unwrap().len(), 1);
}

#[test]
fn worktree_position_default_is_creation_order() {
    let db = db();
    // Inserted a, b, c — `worktrees()` returns them in that creation order
    // regardless of branch name (no alphabetizing), and positions are the
    // dense 0,1,2 the appending MAX+1 insert assigns.
    db.put_worktree("app/c", "/x/app", "/wt/c", "sz/c", None, None)
        .unwrap();
    db.put_worktree("app/a", "/x/app", "/wt/a", "sz/a", None, None)
        .unwrap();
    db.put_worktree("app/b", "/x/app", "/wt/b", "sz/b", None, None)
        .unwrap();
    let order: Vec<_> = db
        .worktrees()
        .unwrap()
        .into_iter()
        .map(|w| (w.worktree, w.position))
        .collect();
    assert_eq!(
        order,
        vec![
            ("/wt/c".into(), 0),
            ("/wt/a".into(), 1),
            ("/wt/b".into(), 2),
        ]
    );

    // Re-registering an existing worktree (upsert) keeps its position — a
    // metadata refresh must never reshuffle the list.
    db.put_worktree("app/c", "/x/app", "/wt/c", "sz/c-renamed", None, None)
        .unwrap();
    let pos_c = db
        .worktrees()
        .unwrap()
        .into_iter()
        .find(|w| w.worktree == "/wt/c")
        .unwrap()
        .position;
    assert_eq!(pos_c, 0, "upsert must preserve position");
}

#[test]
fn swap_worktree_positions_reorders() {
    let db = db();
    db.put_worktree("app/a", "/x/app", "/wt/a", "sz/a", None, None)
        .unwrap();
    db.put_worktree("app/b", "/x/app", "/wt/b", "sz/b", None, None)
        .unwrap();
    db.put_worktree("app/c", "/x/app", "/wt/c", "sz/c", None, None)
        .unwrap();

    // Swap the first two: order becomes b, a, c.
    db.swap_worktree_positions("/wt/a", "/wt/b").unwrap();
    let order: Vec<String> = db
        .worktrees()
        .unwrap()
        .into_iter()
        .map(|w| w.worktree)
        .collect();
    assert_eq!(order, vec!["/wt/b", "/wt/a", "/wt/c"]);

    // set_worktree_position is the persist-side primitive; moving c to the
    // front (a fresh min) floats it above the rest.
    db.set_worktree_position("/wt/c", -1).unwrap();
    let first = db.worktrees().unwrap().into_iter().next().unwrap().worktree;
    assert_eq!(first, "/wt/c");
}

#[test]
fn empty_and_miss_paths() {
    let db = db();
    // Fresh DB: queries return empty / None rather than erroring.
    assert!(db.recent_repos(5).unwrap().is_empty());
    assert!(db.known_repos().unwrap().is_empty());
    assert!(db.workspaces().unwrap().is_empty());
    assert!(db.worktrees().unwrap().is_empty());
    assert!(db.worktree_for_tab("s", "t").unwrap().is_none());
    assert!(db.location_for("/missing").unwrap().is_none());
    assert!(db.repo_root_for("/missing").unwrap().is_none());
    assert!(db.worktree_agent("/missing").unwrap().is_none());
    assert!(!db.is_known_repo("/missing").unwrap());
    // session() honors the env (defaults to "default").
    assert!(!session().is_empty());
}

// Cover the real file-backed open() path (db_path + dir creation + on-disk
// connection + migration) by pointing XDG_STATE_HOME at a temp dir.
#[test]
fn open_on_disk() {
    let dir = std::env::temp_dir().join(format!("sz-db-disk-{}-{:p}", std::process::id(), &0u8));
    let _ = std::fs::remove_dir_all(&dir);
    // Open at an explicit path rather than mutating the global XDG_STATE_HOME
    // (which other parallel tests read via Db::open()/db_path()).
    let path = dir.join("thegn/thegn.db");
    {
        let db = Db::open_at(&path).unwrap();
        db.touch_repo("/r", "r").unwrap();
        assert_eq!(db.recent_repos(5).unwrap(), vec!["/r".to_string()]);
    }
    // Reopen the persisted file: migration is idempotent, data survives.
    {
        let db = Db::open_at(&path).unwrap();
        assert!(db.is_known_repo("/r").unwrap());
    }
    let _ = std::fs::remove_dir_all(&dir);
    // db_path() still derives the default location from XDG_STATE_HOME.
    assert!(db_path().ends_with("thegn/thegn.db"));
}

#[test]
fn ui_state_roundtrip_upsert_and_scope_isolation() {
    let db = db();
    // Unset reads as None.
    assert_eq!(db.get_ui_state("s1", "sort_mode").unwrap(), None);

    // Insert, then read back.
    db.set_ui_state("s1", "sort_mode", "name").unwrap();
    assert_eq!(
        db.get_ui_state("s1", "sort_mode").unwrap(),
        Some("name".to_string())
    );

    // Upsert replaces in place (no duplicate row).
    db.set_ui_state("s1", "sort_mode", "recent").unwrap();
    assert_eq!(
        db.get_ui_state("s1", "sort_mode").unwrap(),
        Some("recent".to_string())
    );

    // A different scope with the same key is isolated.
    db.set_ui_state("s2", "sort_mode", "activity").unwrap();
    assert_eq!(
        db.get_ui_state("s1", "sort_mode").unwrap(),
        Some("recent".to_string())
    );

    // Bulk read of a scope returns only that scope's keys.
    db.set_ui_state("s1", "collapse:app", "1").unwrap();
    let mut pairs = db.ui_state_in_scope("s1").unwrap();
    pairs.sort();
    assert_eq!(
        pairs,
        vec![
            ("collapse:app".to_string(), "1".to_string()),
            ("sort_mode".to_string(), "recent".to_string()),
        ]
    );

    // Delete removes just that key.
    db.del_ui_state("s1", "collapse:app").unwrap();
    assert_eq!(db.get_ui_state("s1", "collapse:app").unwrap(), None);
    assert_eq!(
        db.get_ui_state("s1", "sort_mode").unwrap(),
        Some("recent".to_string())
    );
}

#[test]
fn issue_cache_roundtrips_and_updates() {
    let db = db();
    // Cold cache returns None.
    assert!(db.get_issue_cache("/repo", "linear", "").unwrap().is_none());
    // Write and read back.
    db.put_issue_cache("/repo", "linear", "", r#"[{"id":"linear:A-1"}]"#)
        .unwrap();
    let (json, ts) = db.get_issue_cache("/repo", "linear", "").unwrap().unwrap();
    assert_eq!(json, r#"[{"id":"linear:A-1"}]"#);
    assert!(ts > 0);
    // Different provider is independent.
    assert!(db.get_issue_cache("/repo", "github", "").unwrap().is_none());
    // A different account of the same provider is independent.
    assert!(
        db.get_issue_cache("/repo", "linear", "work")
            .unwrap()
            .is_none()
    );
    db.put_issue_cache("/repo", "linear", "work", r#"[{"id":"linear:W-1"}]"#)
        .unwrap();
    assert_eq!(
        db.get_issue_cache("/repo", "linear", "work")
            .unwrap()
            .unwrap()
            .0,
        r#"[{"id":"linear:W-1"}]"#
    );
    // Upsert overwrites the same (provider, account).
    db.put_issue_cache("/repo", "linear", "", r#"[{"id":"linear:A-2"}]"#)
        .unwrap();
    let (json2, _) = db.get_issue_cache("/repo", "linear", "").unwrap().unwrap();
    assert_eq!(json2, r#"[{"id":"linear:A-2"}]"#);
}

#[test]
fn issue_links_crud() {
    let db = db();
    // No links initially.
    assert!(db.linked_issues("/wt/a").unwrap().is_empty());
    // Link two issues.
    db.link_issue("/wt/a", "linear:A-1").unwrap();
    db.link_issue("/wt/a", "github:42").unwrap();
    let links = db.linked_issues("/wt/a").unwrap();
    assert_eq!(links.len(), 2);
    assert!(links.contains(&"linear:A-1".to_string()));
    assert!(links.contains(&"github:42".to_string()));
    // Another worktree is isolated.
    assert!(db.linked_issues("/wt/b").unwrap().is_empty());
    // Unlink removes exactly one.
    db.unlink_issue("/wt/a", "linear:A-1").unwrap();
    let links = db.linked_issues("/wt/a").unwrap();
    assert_eq!(links.len(), 1);
    assert_eq!(links[0], "github:42");
    // Linking twice is idempotent (no duplicate).
    db.link_issue("/wt/a", "github:42").unwrap();
    assert_eq!(db.linked_issues("/wt/a").unwrap().len(), 1);
}

#[test]
fn notifications_put_and_read_and_mark_read() {
    let db = db();
    // No notifications initially.
    assert!(db.get_unread_notifications().unwrap().is_empty());
    // Add two notifications.
    db.put_notification("status_changed", "linear:A-1", "A-1 moved to Done", "/wt/x")
        .unwrap();
    db.put_notification("assigned", "linear:A-2", "A-2 assigned to you", "/wt/x")
        .unwrap();
    let unread = db.get_unread_notifications().unwrap();
    assert_eq!(unread.len(), 2);
    // Mark one read by id.
    let first_id = unread[0].id;
    db.mark_notification_read(first_id).unwrap();
    assert_eq!(db.get_unread_notifications().unwrap().len(), 1);
    // Mark all read clears the rest.
    db.mark_all_notifications_read().unwrap();
    assert!(db.get_unread_notifications().unwrap().is_empty());
}

#[test]
fn agent_dispatch_roundtrip() {
    let db = db();
    // No dispatch for unknown path.
    assert!(db.dispatch_for_worktree("/wt/issue").unwrap().is_none());
    // Insert a dispatch.
    let id = db
        .put_agent_dispatch("linear:A-1", "/wt/issue", "claude")
        .unwrap();
    assert!(id > 0);
    // Retrieve by worktree path.
    let found = db.dispatch_for_worktree("/wt/issue").unwrap();
    assert_eq!(found, Some(id));
    // Update status.
    db.update_dispatch_status(id, "running").unwrap();
    // A different worktree is isolated.
    assert!(db.dispatch_for_worktree("/wt/other").unwrap().is_none());
}

#[test]
fn dispatch_dispatched_at_ms_reads_latest_timestamp() {
    let db = db();
    // No dispatch → no timestamp (the age computation falls back to the
    // activity snapshot at restore).
    assert!(db.dispatch_dispatched_at_ms("/wt/issue").unwrap().is_none());
    db.put_agent_dispatch("linear:A-1", "/wt/issue", "claude")
        .unwrap();
    let at = db.dispatch_dispatched_at_ms("/wt/issue").unwrap();
    assert!(at.is_some_and(|t| t > 0), "dispatched_at_ms is populated");
}

#[test]
fn dispatch_info_for_worktree_returns_id_and_issue_id() {
    let db = db();
    // No result for unknown path.
    assert!(db.dispatch_info_for_worktree("/wt/x").unwrap().is_none());
    // Insert dispatch.
    let id = db
        .put_agent_dispatch("linear:B-7", "/wt/x", "claude")
        .unwrap();
    // Info returns both id and issue id.
    let info = db.dispatch_info_for_worktree("/wt/x").unwrap();
    assert_eq!(info, Some((id, "linear:B-7".to_string())));
    // Multiple dispatches: most recent wins.
    let id2 = db
        .put_agent_dispatch("linear:B-8", "/wt/x", "claude")
        .unwrap();
    let info2 = db.dispatch_info_for_worktree("/wt/x").unwrap();
    assert_eq!(info2, Some((id2, "linear:B-8".to_string())));
}

#[test]
fn get_all_notifications_returns_read_and_unread() {
    let db = db();
    // 2 read + 1 unread.
    let id1 = db
        .put_notification("assigned", "linear:A-1", "msg1", "/wt")
        .unwrap();
    let id2 = db
        .put_notification("status_changed", "linear:A-2", "msg2", "/wt")
        .unwrap();
    db.put_notification("test_failed", "/wt", "msg3", "/wt")
        .unwrap();
    db.mark_notification_read(id1).unwrap();
    db.mark_notification_read(id2).unwrap();
    // get_all_notifications returns all 3.
    let all = db.get_all_notifications(100).unwrap();
    assert_eq!(all.len(), 3);
    // get_unread_notifications returns only 1.
    let unread = db.get_unread_notifications().unwrap();
    assert_eq!(unread.len(), 1);
}

#[test]
fn get_all_notifications_respects_limit() {
    let db = db();
    for i in 0..60 {
        db.put_notification("assigned", &format!("ref:{i}"), "msg", "/wt")
            .unwrap();
    }
    let capped = db.get_all_notifications(50).unwrap();
    assert_eq!(capped.len(), 50);
    let all = db.get_all_notifications(100).unwrap();
    assert_eq!(all.len(), 60);
}

#[test]
fn delete_notification_removes_single_row() {
    let db = db();
    let id = db
        .put_notification("agent_done", "linear:A-1", "done", "/wt")
        .unwrap();
    db.put_notification("agent_done", "linear:A-2", "done", "/wt")
        .unwrap();
    assert_eq!(db.get_all_notifications(10).unwrap().len(), 2);
    db.delete_notification(id).unwrap();
    let remaining = db.get_all_notifications(10).unwrap();
    assert_eq!(remaining.len(), 1);
    assert_ne!(remaining[0].id, id);
}

#[test]
fn get_unread_counts_by_worktree_groups_by_path() {
    let db = db();
    // Create notifications for different worktrees
    db.put_notification("assigned", "ref:1", "msg", "/wt/app")
        .unwrap();
    db.put_notification("mentioned", "ref:2", "msg", "/wt/app")
        .unwrap();
    db.put_notification("status_changed", "ref:3", "msg", "/wt/other")
        .unwrap();
    // Read one to make it not count as unread
    let unread = db.get_unread_notifications().unwrap();
    assert_eq!(unread.len(), 3);
    db.mark_notification_read(unread[0].id).unwrap();

    let cfg = crate::config::NotificationsConfig::default();
    let counts = db
        .get_unread_counts_by_worktree(&cfg.counted_unread_kind_names())
        .unwrap();
    // /wt/app has 1 unread, /wt/other has 1 unread
    assert_eq!(counts.get("/wt/app"), Some(&1));
    assert_eq!(counts.get("/wt/other"), Some(&1));
}

#[test]
fn get_alert_counts_by_worktree_filters_by_kind() {
    let db = db();
    // Create various notification types
    db.put_notification("assigned", "ref:1", "msg", "/wt/app")
        .unwrap(); // not an alert
    db.put_notification("test_failed", "ref:2", "tests failed", "/wt/app")
        .unwrap();
    db.put_notification("agent_failed", "ref:3", "agent died", "/wt/app")
        .unwrap();
    db.put_notification("process_failed", "ref:4", "cargo died", "/wt/other")
        .unwrap();
    db.put_notification("log_error", "ref:5", "error log", "/wt/other")
        .unwrap(); // NOT an alert — log errors are quiet Info
    db.put_notification("assigned", "ref:6", "msg", "/wt/other")
        .unwrap(); // not an alert

    let cfg = crate::config::NotificationsConfig::default();
    let counts = db
        .get_alert_counts_by_worktree(&cfg.alert_kind_names())
        .unwrap();
    // /wt/app has 2 alerts (test_failed + agent_failed)
    // /wt/other has 1 alert (process_failed) — the log_error does NOT count.
    assert_eq!(counts.get("/wt/app"), Some(&2));
    assert_eq!(counts.get("/wt/other"), Some(&1));
}

#[test]
fn process_failed_alerts_process_exited_is_info_only() {
    let db = db();
    let cfg = crate::config::NotificationsConfig::default();
    // A clean task completion: Info — inbox-only, counted by neither badge.
    db.put_notification("process_exited", "make", "make finished", "/wt/app")
        .unwrap();
    // A failure: Alert — counted by both the unread and the alert badge.
    db.put_notification(
        "process_failed",
        "cargo",
        "cargo failed (exit 101)",
        "/wt/app",
    )
    .unwrap();

    let unread = db
        .get_unread_counts_by_worktree(&cfg.counted_unread_kind_names())
        .unwrap();
    assert_eq!(
        unread.get("/wt/app"),
        Some(&1),
        "only the Alert counts toward unread; process_exited is Info"
    );

    let alerts = db
        .get_alert_counts_by_worktree(&cfg.alert_kind_names())
        .unwrap();
    assert_eq!(
        alerts.get("/wt/app"),
        Some(&1),
        "only process_failed is an alert"
    );
}

#[test]
fn empty_kind_set_yields_no_counts() {
    let db = db();
    db.put_notification("test_failed", "ref", "boom", "/wt/app")
        .unwrap();
    assert!(
        db.get_alert_counts_by_worktree(&[]).unwrap().is_empty(),
        "no kinds → no flag"
    );
}

#[test]
fn config_demotion_reclassifies_counts_live() {
    let db = db();
    db.put_notification("test_failed", "ref", "boom", "/wt/app")
        .unwrap();
    let mut cfg = crate::config::NotificationsConfig::default();
    // Demote test_failed to notice: it drops out of the alert badge but stays
    // in the neutral unread count — no stored row changed.
    cfg.priority.insert("test_failed".into(), "notice".into());
    assert!(
        db.get_alert_counts_by_worktree(&cfg.alert_kind_names())
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        db.get_unread_counts_by_worktree(&cfg.counted_unread_kind_names())
            .unwrap()
            .get("/wt/app"),
        Some(&1)
    );
}

// ── Suite C: container_events audit trail ──────────────────────────────

#[test]
fn container_events_round_trip() {
    let db = db();
    db.insert_container_event("/wt/feat", 1000, "exec", Some("cargo build"), None)
        .unwrap();
    db.insert_container_event("/wt/feat", 2000, "exec", Some("git status"), Some(0))
        .unwrap();
    db.insert_container_event("/wt/other", 3000, "die", None, Some(1))
        .unwrap();

    let events = db.container_events("/wt/feat", 10).unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].ts, 2000, "newest first");
    assert_eq!(events[1].kind, "exec");
    assert_eq!(events[1].detail.as_deref(), Some("cargo build"));

    let other = db.container_events("/wt/other", 10).unwrap();
    assert_eq!(other.len(), 1);
    assert_eq!(other[0].exit_code, Some(1));
}

#[test]
fn container_events_prune_removes_old() {
    let db = db();
    let now = crate::util::now();
    db.insert_container_event("/wt/feat", now - 86400, "exec", Some("old"), None)
        .unwrap();
    db.insert_container_event("/wt/feat", now - 100, "exec", Some("recent"), None)
        .unwrap();
    db.insert_container_event("/wt/feat", now, "exec", Some("now"), None)
        .unwrap();
    db.prune_container_events(3600).unwrap();
    let remaining = db.container_events("/wt/feat", 10).unwrap();
    assert_eq!(remaining.len(), 2, "only the 24h-old row should be pruned");
    assert!(
        remaining.iter().all(|e| e.detail.as_deref() != Some("old")),
        "old event must not appear in results"
    );
}

#[test]
fn container_events_limit_honoured() {
    let db = db();
    for i in 0..15i64 {
        db.insert_container_event("/wt/feat", i, "exec", None, None)
            .unwrap();
    }
    let ten = db.container_events("/wt/feat", 10).unwrap();
    assert_eq!(ten.len(), 10);
}

#[test]
fn proxy_health_roundtrip_and_window() {
    let db = db();
    // A live marker (probe in the future) loads; a stale one (past) does not.
    db.put_proxy_health(
        "openrouter",
        "ds-pro",
        "rate_limit",
        "HTTP 429",
        1000,
        9_999_999,
        false,
        2,
        None,
        None,
    )
    .unwrap();
    db.put_proxy_health(
        "kilo", "ds-pro", "payment", "HTTP 402", 1000, 500, true, 1, None, None,
    )
    .unwrap();
    let live = db.load_proxy_health(1_000_000).unwrap();
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].backend, "openrouter");
    assert_eq!(live[0].consecutive_failures, 2);
    // Upsert overwrites in place.
    db.put_proxy_health(
        "openrouter",
        "ds-pro",
        "rate_limit",
        "HTTP 429",
        1000,
        9_999_999,
        false,
        5,
        None,
        None,
    )
    .unwrap();
    assert_eq!(
        db.load_proxy_health(1_000_000).unwrap()[0].consecutive_failures,
        5
    );
    db.clear_proxy_health("openrouter", "ds-pro").unwrap();
    assert!(db.load_proxy_health(1_000_000).unwrap().is_empty());
}

#[test]
fn proxy_budget_spend_caps_and_window() {
    let db = db();
    let (tokens, cost, killed) = db
        .add_proxy_spend("agent:reviewer", 100, 0.5, 1000)
        .unwrap();
    assert_eq!(tokens, 100);
    assert!((cost - 0.5).abs() < 1e-9);
    assert!(!killed);
    // A second add accumulates.
    let (tokens, _, _) = db
        .add_proxy_spend("agent:reviewer", 50, 0.25, 1000)
        .unwrap();
    assert_eq!(tokens, 150);
    // Kill switch flips and is visible on the budget row.
    db.set_proxy_kill_switch("agent:reviewer", true).unwrap();
    assert!(db.proxy_budget("agent:reviewer").unwrap().unwrap().killed);
}

#[test]
fn proxy_spend_window_resets_when_due() {
    let db = db();
    db.add_proxy_spend("global", 100, 1.0, 1000).unwrap();
    // Arm a rolling window that has already elapsed by `now_ms`.
    db.conn
        .execute(
            "UPDATE proxy_budgets SET reset_ms=2000 WHERE scope='global'",
            [],
        )
        .unwrap();
    // now_ms past reset → accumulators reset before the add.
    let (tokens, _, _) = db.add_proxy_spend("global", 10, 0.1, 3000).unwrap();
    assert_eq!(tokens, 10);
}

#[test]
fn proxy_virtual_key_lookup_and_revoke() {
    let db = db();
    db.put_proxy_virtual_key(
        "vk_1",
        "hash",
        "reviewer",
        "agent:reviewer",
        Some("anthropic"),
        1000,
    )
    .unwrap();
    let got = db.proxy_virtual_key("vk_1").unwrap().unwrap();
    assert_eq!(got.0, "agent:reviewer");
    assert_eq!(got.1.as_deref(), Some("anthropic"));
    db.revoke_proxy_virtual_key("vk_1", 2000).unwrap();
    assert!(db.proxy_virtual_key("vk_1").unwrap().is_none());
}

#[test]
fn loc_cache_entry_returns_value_and_timestamp() {
    let db = db();
    // Cold cache misses.
    assert!(db.get_loc_cache_entry("/wt").unwrap().is_none());
    let report = crate::loc::LocReport::total_only(4242);
    let json = serde_json::to_string(&report).unwrap();
    db.put_loc_cache("/wt", report.total_code, &json).unwrap();
    let (got_json, fetched_at) = db.get_loc_cache_entry("/wt").unwrap().unwrap();
    assert_eq!(
        serde_json::from_str::<crate::loc::LocReport>(&got_json).unwrap(),
        report
    );
    assert!(fetched_at > 0, "fetch timestamp is stamped for TTL refresh");
    // A different worktree is isolated.
    assert!(db.get_loc_cache_entry("/other").unwrap().is_none());
}

#[test]
fn set_proxy_budget_limits_creates_and_updates_caps() {
    let db = db();
    // No budget row yet.
    assert!(db.proxy_budget("agent:r").unwrap().is_none());

    // Setting limits creates the row without touching (zero) spend.
    db.set_proxy_budget_limits("agent:r", "weekly", Some(1_000), Some(2.5), 5000)
        .unwrap();
    let b = db.proxy_budget("agent:r").unwrap().unwrap();
    assert_eq!(b.period, "weekly");
    assert_eq!(b.limit_tokens, Some(1_000));
    assert_eq!(b.limit_cost, Some(2.5));
    assert_eq!(b.reset_ms, 5000);
    assert_eq!(b.spent_tokens, 0);
    assert!((b.spent_cost).abs() < 1e-9);
    assert!(!b.killed);

    // Accumulate spend, then re-set caps: spend must be preserved, caps updated.
    db.add_proxy_spend("agent:r", 300, 0.9, 100).unwrap();
    db.set_proxy_budget_limits("agent:r", "monthly", None, None, 9000)
        .unwrap();
    let b = db.proxy_budget("agent:r").unwrap().unwrap();
    assert_eq!(b.period, "monthly");
    assert_eq!(b.limit_tokens, None, "None means no cap");
    assert_eq!(b.limit_cost, None);
    assert_eq!(b.reset_ms, 9000);
    assert_eq!(b.spent_tokens, 300, "re-setting caps preserves spend");
    assert!((b.spent_cost - 0.9).abs() < 1e-9);
}

#[test]
fn put_worktree_records_folder_id_and_remote_location() {
    let db = db();
    db.put_workspace("/x/app", "app", "repo").unwrap();
    let folder = db.create_folder("/x/app", "Features").unwrap();

    // Inserting with a folder_id + remote location persists both.
    db.put_worktree(
        "app/feat",
        "/x/app",
        "/wt/feat",
        "sz/feat",
        Some(r#"{"host":"box"}"#),
        Some(folder),
    )
    .unwrap();
    assert_eq!(
        db.location_for("/wt/feat").unwrap().as_deref(),
        Some(r#"{"host":"box"}"#)
    );

    // COALESCE(?8, folder_id): a later upsert with folder_id=None keeps the
    // existing folder association rather than clearing it.
    db.put_worktree("app/feat", "/x/app", "/wt/feat", "sz/feat", None, None)
        .unwrap();
    let fid: Option<i64> = db
        .conn
        .query_row(
            "SELECT folder_id FROM worktrees WHERE worktree=?1",
            params!["/wt/feat"],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(fid, Some(folder), "upsert with None preserves folder_id");
}

#[test]
fn env_name_set_get_and_effective_precedence() {
    let db = db();
    db.put_workspace("/x/app", "app", "repo").unwrap();
    db.put_worktree("app/feat", "/x/app", "/wt/feat", "sz/feat", None, None)
        .unwrap();

    // Unset → None at every level.
    assert_eq!(db.worktree_env("/wt/feat").unwrap(), None);
    assert_eq!(db.workspace_env("/x/app").unwrap(), None);
    assert_eq!(db.effective_env("/wt/feat", "/x/app"), None);

    // Workspace-level selection is the fallback.
    db.set_workspace_env("/x/app", "company-k8s").unwrap();
    assert_eq!(
        db.workspace_env("/x/app").unwrap().as_deref(),
        Some("company-k8s")
    );
    assert_eq!(
        db.effective_env("/wt/feat", "/x/app").as_deref(),
        Some("company-k8s")
    );

    // A worktree-level selection wins over the workspace default.
    db.set_worktree_env("/wt/feat", "datonya").unwrap();
    assert_eq!(
        db.effective_env("/wt/feat", "/x/app").as_deref(),
        Some("datonya")
    );

    // Clearing the worktree falls back to the workspace; whitespace clears.
    db.set_worktree_env("/wt/feat", "   ").unwrap();
    assert_eq!(db.worktree_env("/wt/feat").unwrap(), None);
    assert_eq!(
        db.effective_env("/wt/feat", "/x/app").as_deref(),
        Some("company-k8s")
    );

    // Clearing the workspace too → fully unset.
    db.set_workspace_env("/x/app", "").unwrap();
    assert_eq!(db.effective_env("/wt/feat", "/x/app"), None);
}

#[test]
fn proxy_kill_switch_set_clear_creates_row() {
    let db = db();
    // Setting the kill switch on an unknown scope creates the row.
    db.set_proxy_kill_switch("worktree:/wt", true).unwrap();
    assert!(db.proxy_budget("worktree:/wt").unwrap().unwrap().killed);
    // Clearing it flips back.
    db.set_proxy_kill_switch("worktree:/wt", false).unwrap();
    assert!(!db.proxy_budget("worktree:/wt").unwrap().unwrap().killed);
}

#[test]
fn proxy_virtual_key_upsert_unrevokes() {
    let db = db();
    db.put_proxy_virtual_key("vk", "h1", "lbl", "global", None, 1)
        .unwrap();
    db.revoke_proxy_virtual_key("vk", 2).unwrap();
    assert!(db.proxy_virtual_key("vk").unwrap().is_none());
    // Re-registering the same key id clears the revocation (revoked_at=NULL).
    db.put_proxy_virtual_key("vk", "h2", "lbl2", "agent:x", Some("kilo"), 3)
        .unwrap();
    let got = db.proxy_virtual_key("vk").unwrap().unwrap();
    assert_eq!(got.0, "agent:x");
    assert_eq!(got.1.as_deref(), Some("kilo"));
}

#[test]
fn migrate_v6_skips_extra_kind_rows_with_empty_name() {
    // A legacy tab_layout where one row has an empty tab_name (the `continue`
    // branch) and another session has no recorded active_tab (active_idx
    // defaults to 0). Exercises the migration's edge branches.
    let dir = std::env::temp_dir().join(format!("sz-db-mig6e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("db.sqlite");
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            r#"
            PRAGMA user_version = 5;
            CREATE TABLE tab_layout (
              session_name TEXT, tab_name TEXT, kind TEXT, worktree TEXT,
              pane_tree TEXT, ordinal INTEGER, focused_pane INTEGER,
              PRIMARY KEY (session_name, tab_name));
            CREATE TABLE session_state (
              session_name TEXT PRIMARY KEY, active_tab TEXT, updated_at INTEGER);
            INSERT INTO tab_layout VALUES
              ('/r', '',         'home',     '/r',       '{"leaf":0}', 0, 0),
              ('/r', 'app/home', 'home',     '/r',       '{"leaf":1}', 1, 0);
            "#,
        )
        .unwrap();
    }
    let db = Db::open_at(&path).unwrap();
    let groups = db.groups_for_session("/r").unwrap();
    // Only the named row produced a group; the empty-name row was skipped.
    assert_eq!(
        groups.iter().map(|g| g.name.as_str()).collect::<Vec<_>>(),
        vec!["app/home"]
    );
    // No active marker recorded → group active_tab defaulted to 0.
    assert_eq!(groups[0].active_tab, 0);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn migrates_v2_drops_and_recreates_session_tables() {
    // A pre-v3 DB (user_version < 3) with the old per-session schema: the
    // v2→v3 remap drops worktrees/workspaces but preserves the `repos`
    // recents history (the only irreplaceable data).
    let dir = std::env::temp_dir().join(format!("sz-db-v2-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("db.sqlite");
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            r#"
            PRAGMA user_version = 2;
            CREATE TABLE repos (
              path TEXT PRIMARY KEY, name TEXT, first_seen INTEGER,
              last_opened INTEGER, open_count INTEGER, seq INTEGER);
            INSERT INTO repos(path,name,first_seen,last_opened,open_count,seq)
              VALUES ('/keep','keep',1,1,1,1);
            CREATE TABLE worktrees (worktree TEXT PRIMARY KEY, session_name TEXT);
            CREATE TABLE workspaces (session_name TEXT PRIMARY KEY, name TEXT);
            INSERT INTO worktrees VALUES ('/old','sess');
            "#,
        )
        .unwrap();
    }
    let db = Db::open_at(&path).unwrap();
    // repos recents survived the remap.
    assert!(db.is_known_repo("/keep").unwrap());
    assert_eq!(db.recent_repos(5).unwrap(), vec!["/keep".to_string()]);
    // The pre-v3 worktrees/workspaces rows were dropped & recreated empty.
    assert!(db.worktrees().unwrap().is_empty());
    assert!(db.workspaces().unwrap().is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn proxy_request_audit_insert() {
    let db = db();
    let row = ProxyRequestRow {
        ts_ms: 1234,
        protocol: "openai".into(),
        route: "standard".into(),
        agent: Some("reviewer".into()),
        worktree: Some("/wt/feat".into()),
        client_model: "model-proxy/standard".into(),
        backend: "openrouter".into(),
        backend_model: "ds-pro".into(),
        input_tokens: 100,
        output_tokens: 50,
        cost_usd: 0.01,
        cost_source: "estimate".into(),
        outcome: "ok".into(),
        ..Default::default()
    };
    let id = db.put_proxy_request(&row).unwrap();
    assert!(id > 0);
}

// --- migration ladder ---------------------------------------------------
// A systematic upgrade harness. Each rung reconstructs a historical DB
// shape that is *derivable from the migration code itself* (the `ver < 3`
// remap, the additive ALTER/CREATE comments, `migrate_tab_layout_v6`),
// opens it through the normal `Db::open_at` path, and asserts:
//   (a) the version stamp reached SCHEMA_VERSION,
//   (b) seeded data survived wherever the migration preserves it,
//   (c) the migrated schema converged EXACTLY to a fresh DB's schema
//       (tables + column sets) — so "migration drift" (a migrated DB
//       missing a column a fresh DB has, or a legacy table lingering)
//       fails loudly.

/// table → column-name set (`sqlite_*` internals excluded). Compared as
/// sets, not ordered lists: additive ALTERs append columns in a different
/// order than a fresh CREATE, which is fine.
fn schema_snapshot(
    conn: &Connection,
) -> std::collections::BTreeMap<String, std::collections::BTreeSet<String>> {
    let mut stmt = conn
        .prepare(
            "SELECT name FROM sqlite_master
             WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
        )
        .unwrap();
    let tables: Vec<String> = stmt
        .query_map([], |r| r.get(0))
        .unwrap()
        .filter_map(|r| r.ok())
        .collect();
    tables
        .into_iter()
        .map(|t| {
            let mut stmt = conn.prepare(&format!("PRAGMA table_info({t})")).unwrap();
            let cols: std::collections::BTreeSet<String> = stmt
                .query_map([], |r| r.get::<_, String>(1))
                .unwrap()
                .filter_map(|r| r.ok())
                .collect();
            (t, cols)
        })
        .collect()
}

/// Build a legacy DB file from raw SQL (no Db API), open it via the normal
/// migration path, and assert the version stamp + schema convergence with
/// a fresh DB. Returns the tempdir + migrated Db for rung-specific data
/// checks; the caller removes the dir when done.
fn open_ladder_fixture(tag: &str, seed_sql: &str) -> (std::path::PathBuf, Db) {
    let dir = std::env::temp_dir().join(format!("sz-db-ladder-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("db.sqlite");
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(seed_sql).unwrap();
    }
    let db = Db::open_at(&path).unwrap();
    // (a) the version stamp reached the current schema version.
    let ver: i64 = db
        .conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(ver, SCHEMA_VERSION, "rung {tag}: version stamp");
    // (c) the migration-drift gate: exactly the fresh tables + columns.
    let fresh = Db::open_memory().unwrap();
    assert_eq!(
        schema_snapshot(&db.conn),
        schema_snapshot(&fresh.conn),
        "rung {tag}: migrated schema must converge to the fresh schema"
    );
    (dir, db)
}

#[test]
fn ladder_v2_per_session_schema_preserves_repos() {
    // Oldest rung: the pre-v3 per-session schema. The `ver < 3` branch in
    // `init` documents the old shape (worktrees/workspaces keyed by
    // session, repos without `session_name`): the remap drops the session
    // tables and preserves only the `repos` recents history.
    let (dir, db) = open_ladder_fixture(
        "v2",
        r#"
        PRAGMA user_version = 2;
        CREATE TABLE repos (
          path TEXT PRIMARY KEY, name TEXT, first_seen INTEGER,
          last_opened INTEGER, open_count INTEGER, seq INTEGER);
        INSERT INTO repos VALUES ('/keep','keep',1,2,3,4);
        CREATE TABLE worktrees (worktree TEXT PRIMARY KEY, session_name TEXT);
        CREATE TABLE workspaces (session_name TEXT PRIMARY KEY, name TEXT);
        INSERT INTO worktrees VALUES ('/old','sess');
        INSERT INTO workspaces VALUES ('sess','old');
        "#,
    );
    assert!(db.is_known_repo("/keep").unwrap());
    assert_eq!(db.recent_repos(5).unwrap(), vec!["/keep".to_string()]);
    // The per-session tables were dropped and recreated empty.
    assert!(db.worktrees().unwrap().is_empty());
    assert!(db.workspaces().unwrap().is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn ladder_v5_flat_tab_layout_becomes_groups() {
    // The v5 shape (flat `tab_layout`, extra pages as " ·N" suffixes) from
    // the existing v5 fixture. The v6 transform regroups it; the drift
    // gate additionally proves the legacy table is gone (a fresh DB has
    // no `tab_layout`). Detailed transform semantics stay covered by
    // `migrates_v5_tab_layout_into_groups`.
    let (dir, db) = open_ladder_fixture(
        "v5",
        r#"
        PRAGMA user_version = 5;
        CREATE TABLE tab_layout (
          session_name TEXT, tab_name TEXT, kind TEXT, worktree TEXT,
          pane_tree TEXT, ordinal INTEGER, focused_pane INTEGER,
          PRIMARY KEY (session_name, tab_name));
        CREATE TABLE session_state (
          session_name TEXT PRIMARY KEY, active_tab TEXT, updated_at INTEGER);
        INSERT INTO tab_layout VALUES
          ('/r', 'app/home', 'home',     '/r',       '{"leaf":0}', 0, 0),
          ('/r', 'app/feat', 'worktree', '/wt/feat', '{"leaf":1}', 1, 1);
        INSERT INTO session_state VALUES ('/r', 'app/feat', 1);
        "#,
    );
    let groups = db.groups_for_session("/r").unwrap();
    assert_eq!(
        groups.iter().map(|g| g.name.as_str()).collect::<Vec<_>>(),
        vec!["app/home", "app/feat"]
    );
    assert_eq!(db.active_tab("/r").unwrap().as_deref(), Some("app/feat"));
    let tabs = db.group_tabs_for_session("/r").unwrap();
    assert_eq!(tabs.len(), 2, "one migrated tab per legacy row");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn ladder_v7_worktrees_position_backfilled_from_creation_order() {
    // Pre-v8 worktrees: the v3-era column set — the `location`,
    // `sandbox_backend` and `position` ALTER comments in `init` document
    // that those columns did not exist yet. The migration must ALTER the
    // missing columns in and backfill `position` by (created_at, worktree)
    // without losing any row data.
    let (dir, db) = open_ladder_fixture(
        "v7",
        r#"
        PRAGMA user_version = 7;
        CREATE TABLE worktrees (
          worktree TEXT PRIMARY KEY, session_name TEXT, tab_name TEXT,
          repo_path TEXT, branch TEXT, agent TEXT, created_at INTEGER);
        INSERT INTO worktrees VALUES
          ('/wt/b', 's', 'app/b', '/r', 'sz/b', '', 200),
          ('/wt/c', 's', 'app/c', '/r', 'sz/c', '', 100),
          ('/wt/a', 's', 'app/a', '/r', 'sz/a', '', 100);
        "#,
    );
    let wts = db.worktrees().unwrap();
    let order: Vec<&str> = wts.iter().map(|w| w.worktree.as_str()).collect();
    assert_eq!(
        order,
        vec!["/wt/a", "/wt/c", "/wt/b"],
        "backfill ranks by created_at with path as the tie-breaker"
    );
    assert_eq!(
        wts.iter().map(|w| w.position).collect::<Vec<_>>(),
        vec![0, 1, 2],
        "backfill assigns dense, collision-free positions"
    );
    // Pre-existing row data survived the ALTERs.
    assert_eq!(wts[0].branch, "sz/a");
    assert_eq!(wts[0].repo_root, "/r");
    assert_eq!(wts[0].tab_name, "app/a");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn ladder_v13_group_tabs_gain_pane_columns() {
    // Pre-v14 group_tabs: the current CREATE minus the four additive
    // columns (`pane_cwds` v14, `pane_cmds` v15, `pane_sessions` v23,
    // `scrollback_snapshot` v29) — each ALTER comment documents that the
    // column is absent on older rows. Legacy rows must survive with the new
    // columns reading empty (the "old snapshot restores unchanged" path).
    let (dir, db) = open_ladder_fixture(
        "v13",
        r#"
        PRAGMA user_version = 13;
        CREATE TABLE tab_groups (
          session_name TEXT NOT NULL, name TEXT NOT NULL, kind TEXT NOT NULL,
          worktree TEXT NOT NULL, ordinal INTEGER NOT NULL,
          active_tab INTEGER NOT NULL DEFAULT 0,
          PRIMARY KEY (session_name, name));
        CREATE TABLE group_tabs (
          session_name TEXT NOT NULL, group_name TEXT NOT NULL,
          ordinal INTEGER NOT NULL, title TEXT NOT NULL,
          pane_tree TEXT NOT NULL, focused_pane INTEGER NOT NULL DEFAULT 0,
          PRIMARY KEY (session_name, group_name, ordinal));
        INSERT INTO tab_groups VALUES ('/r', 'app/feat', 'branch', '/wt/feat', 0, 0);
        INSERT INTO group_tabs VALUES ('/r', 'app/feat', 0, '1', '{"leaf":7}', 7);
        "#,
    );
    let groups = db.groups_for_session("/r").unwrap();
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].worktree, "/wt/feat");
    let tabs = db.group_tabs_for_session("/r").unwrap();
    assert_eq!(tabs.len(), 1);
    assert_eq!(tabs[0].pane_tree, r#"{"leaf":7}"#);
    assert_eq!(tabs[0].focused_pane, 7);
    // The ALTERed columns read back as empty (NULL → default) on legacy rows.
    assert_eq!(tabs[0].pane_cwds, "");
    assert_eq!(tabs[0].pane_cmds, "");
    assert_eq!(tabs[0].pane_sessions, "");
    assert_eq!(tabs[0].scrollback_snapshot, "");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn ladder_v15_workspaces_position_backfilled_from_recency() {
    // Pre-v16 workspaces (the existing v15 fixture shape): no `position`,
    // `kind` or `env_name` — all three are additive ALTERs. `position`
    // backfills from `last_active DESC` so the first post-upgrade launch
    // keeps the old recency order.
    let (dir, db) = open_ladder_fixture(
        "v15",
        r#"
        PRAGMA user_version = 15;
        CREATE TABLE workspaces (
          repo_path TEXT PRIMARY KEY, name TEXT,
          created_at INTEGER, last_active INTEGER);
        INSERT INTO workspaces VALUES
          ('/old',    'old',    1, 100),
          ('/newest', 'newest', 1, 300),
          ('/mid',    'mid',    1, 200);
        "#,
    );
    let ws = db.workspaces().unwrap();
    let order: Vec<&str> = ws.iter().map(|w| w.repo_path.as_str()).collect();
    assert_eq!(
        order,
        vec!["/newest", "/mid", "/old"],
        "position 0 = most-recently-active"
    );
    // The ALTERed `kind` reads back as the repo default on legacy rows.
    assert!(ws.iter().all(|w| w.kind == "repo"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn ladder_v21_gains_merge_queue_forwards_pool_and_registers() {
    // Pre-v22: `merge_queue` (v22), `group_tabs.pane_sessions` (v23),
    // `forwards` (v24), the pool tables + `provider_sandbox_id` (v26) and
    // `registers` (v27) don't exist yet — every one is an additive
    // CREATE/ALTER, so the old shape is exactly "current minus those".
    // Seed cache rows to prove data survives, then exercise each
    // newly-created table through the normal API.
    let (dir, db) = open_ladder_fixture(
        "v21",
        r#"
        PRAGMA user_version = 21;
        CREATE TABLE repos (
          path TEXT PRIMARY KEY, name TEXT, first_seen INTEGER,
          last_opened INTEGER, open_count INTEGER DEFAULT 0,
          seq INTEGER DEFAULT 0, session_name TEXT);
        INSERT INTO repos(path,name,first_seen,last_opened,open_count,seq)
          VALUES ('/keep','keep',1,1,1,1);
        CREATE TABLE pr_cache (
          worktree TEXT PRIMARY KEY, branch TEXT, json TEXT, fetched_at INTEGER);
        INSERT INTO pr_cache VALUES ('/wt/x','sz/x','{"number":1}',42);
        "#,
    );
    // Seeded data survived.
    assert!(db.is_known_repo("/keep").unwrap());
    let (json, at) = db.get_pr_cache("/wt/x").unwrap().unwrap();
    assert_eq!((json.as_str(), at), (r#"{"number":1}"#, 42));
    // Every post-v21 table is usable through the normal API.
    db.enqueue_merge("/wt/x", "sz/x", "main").unwrap();
    assert_eq!(db.list_merge_queue().unwrap().len(), 1);
    db.upsert_forward("/wt/x", 3000, 3000, "http://127.0.0.1:3000")
        .unwrap();
    assert_eq!(db.list_forwards().unwrap().len(), 1);
    db.insert_pool_spare("keep-pool-1", "/keep", "sprites")
        .unwrap();
    assert_eq!(db.pool_spares_for("/keep", "sprites").unwrap().len(), 1);
    db.put_register('a', "migrated").unwrap();
    assert_eq!(
        db.all_registers().unwrap(),
        vec![('a', "migrated".to_string())]
    );
    let _ = std::fs::remove_dir_all(&dir);
}
