use super::*;
use crate::merge_sweep::CleanupHold;
use crate::store::WorkspaceStore;

#[test]
fn cleanup_tenancy_holds_raw_sandbox_and_worktree_associations() {
    let db = Db::open_memory().unwrap();
    let store: &dyn crate::store::PlacementStore = &db;
    assert!(!store.has_cleanup_tenancy("private/wt").unwrap());
    db.conn().execute(
        "INSERT INTO host_tenancy(sandbox,host_id,worktree,cpu_floor_milli,mem_floor_mb,state,reserved_at) VALUES('private/wt','invalid-host-id','private/associated',0,0,'released',0)",
        [],
    ).unwrap();
    assert!(store.has_cleanup_tenancy("private/wt").unwrap());
    assert!(store.has_cleanup_tenancy("private/associated").unwrap());
    assert!(
        store.tenancy_for("private/wt").unwrap().is_none(),
        "exercise lossy legacy decode"
    );
    assert!(!store.has_cleanup_tenancy("private/missing").unwrap());
    db.conn().execute_batch("DROP TABLE host_tenancy").unwrap();
    assert!(store.has_cleanup_tenancy("private/wt").is_err());
}

#[test]
fn cleanup_dispatch_holds_unknown_nonterminal_and_pending_rows() {
    let db = Db::open_memory().unwrap();
    let store: &dyn crate::store::NotificationStore = &db;
    assert!(!store.has_cleanup_dispatch("private/wt").unwrap());
    db.conn().execute(
        "INSERT INTO agent_dispatches(issue_id,worktree_path,agent_name,dispatched_at_ms,status) VALUES('private','private/wt','private',0,'unknown-future-state')",
        [],
    ).unwrap();
    assert!(store.has_cleanup_dispatch("private/wt").unwrap());
    assert!(
        db.worktrees_with_active_dispatch().unwrap().is_empty(),
        "exercise inactive legacy unknown"
    );
    for status in [
        "queued",
        "spawning",
        "running",
        "waiting_human",
        "pr_open",
        "parked",
    ] {
        db.conn()
            .execute("UPDATE agent_dispatches SET status=?1", [status])
            .unwrap();
        assert!(
            store.has_cleanup_dispatch("private/wt").unwrap(),
            "{status}"
        );
    }
    for status in ["done", "failed", "merged", "abandoned"] {
        db.conn()
            .execute("UPDATE agent_dispatches SET status=?1", [status])
            .unwrap();
        assert!(
            !store.has_cleanup_dispatch("private/wt").unwrap(),
            "{status}"
        );
    }
    db.conn()
        .execute(
            "UPDATE agent_dispatches SET pending_worktree_path='private/next'",
            [],
        )
        .unwrap();
    assert!(store.has_cleanup_dispatch("private/next").unwrap());
    assert!(!store.has_cleanup_dispatch("private/wt").unwrap());
    db.conn()
        .execute("UPDATE agent_dispatches SET status=x'00'", [])
        .unwrap();
    assert!(store.has_cleanup_dispatch("private/wt").is_err());
    db.conn()
        .execute_batch("DROP TABLE agent_dispatches")
        .unwrap();
    assert!(store.has_cleanup_dispatch("private/wt").is_err());
}

fn fixture() -> (Db, MergeQueueRow) {
    let db = Db::open_memory().unwrap();
    db.enqueue_merge("private/wt", "feature", "main").unwrap();
    db.update_merge_status(
        "private/wt",
        "landed",
        Some("private-commit"),
        Some("prior-path"),
        Some("prior-detail"),
    )
    .unwrap();
    db.conn()
        .execute(
            "UPDATE merge_queue SET queued_at=11, updated_at=22, agent_attempts=7, location=NULL",
            [],
        )
        .unwrap();
    let selected = db.list_merge_queue().unwrap().remove(0);
    (db, selected)
}

#[test]
fn cleanup_hold_exact_cas_preserves_all_other_fields_and_raw_aliases() {
    let (db, selected) = fixture();
    assert!(db.hold_merge_cleanup(&selected).unwrap());
    let mut expected = selected.clone();
    expected.error_detail = Some(CleanupHold::BranchRetained.marker().into());
    assert_eq!(db.list_merge_queue().unwrap(), [expected.clone()]);
    let raw: Option<String> = db
        .conn()
        .query_row("SELECT location FROM merge_queue", [], |row| row.get(0))
        .unwrap();
    assert!(
        raw.is_none(),
        "NULL location must not be rewritten to empty alias"
    );
    assert!(!db.hold_merge_cleanup(&selected).unwrap());
    assert!(!db.hold_merge_cleanup(&expected).unwrap());
    assert!(
        CleanupHold::from_detail(Some(&format!(
            "{} suffix",
            CleanupHold::BranchRetained.marker()
        )))
        .is_none()
    );
    db.enqueue_merge("private/wt", "feature", "main").unwrap();
    let retried = db.list_merge_queue().unwrap().remove(0);
    assert_eq!(retried.status, "queued");
    assert!(retried.error_detail.is_none() && retried.result_oid.is_none());
}

#[test]
fn cleanup_hold_revoked_changed_and_removed_rows_return_false() {
    let (db, selected) = fixture();
    db.conn()
        .execute("UPDATE merge_queue SET updated_at=23", [])
        .unwrap();
    assert!(!db.hold_merge_cleanup(&selected).unwrap());
    db.remove_merge_entry(&selected.worktree).unwrap();
    assert!(!db.hold_merge_cleanup(&selected).unwrap());
    db.enqueue_merge(
        &selected.worktree,
        &selected.branch,
        &selected.target_branch,
    )
    .unwrap();
    assert!(!db.hold_merge_cleanup(&selected).unwrap());
}

#[test]
fn cleanup_hold_survives_generic_reaping_until_explicit_queue_dismissal() {
    let (db, selected) = fixture();
    assert!(db.hold_merge_cleanup(&selected).unwrap());
    let held = db.list_merge_queue().unwrap();
    db.del_worktree(&selected.worktree).unwrap();
    db.del_worktree(&selected.worktree).unwrap();
    assert_eq!(db.list_merge_queue().unwrap(), held);
    db.remove_merge_entry(&selected.worktree).unwrap();
    assert!(db.list_merge_queue().unwrap().is_empty());
    db.enqueue_merge("private/ordinary", "branch", "main")
        .unwrap();
    db.del_worktree("private/ordinary").unwrap();
    assert!(
        db.list_merge_queue().unwrap().is_empty(),
        "ordinary cascade unchanged"
    );
}

#[test]
fn cleanup_hold_rollback_and_query_failure_are_not_success() {
    let (db, selected) = fixture();
    let rollback: Result<()> = db.transaction(|db| {
        assert!(db.hold_merge_cleanup(&selected)?);
        anyhow::bail!("injected post-hold bookkeeping failure")
    });
    assert!(rollback.is_err());
    assert_eq!(db.list_merge_queue().unwrap(), [selected.clone()]);
    db.conn().execute_batch("DROP TABLE merge_queue").unwrap();
    assert!(db.hold_merge_cleanup(&selected).is_err());
}
