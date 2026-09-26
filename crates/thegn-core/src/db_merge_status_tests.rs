use super::*;

const WORKTREE: &str = "/private/status-only";

fn seeded() -> Db {
    let db = Db::open_memory().unwrap();
    db.enqueue_merge(WORKTREE, "feature", "target").unwrap();
    db.set_merge_agent_attempts(WORKTREE, 2).unwrap();
    db.update_merge_status(
        WORKTREE,
        "deferred",
        Some("old-oid"),
        Some("old/path"),
        Some("old diagnostic"),
    )
    .unwrap();
    db
}

fn row(db: &Db) -> MergeQueueRow {
    let rows = db.list_merge_queue().unwrap();
    assert_eq!(rows.len(), 1);
    rows.into_iter().next().unwrap()
}

#[test]
fn exact_replacement_clears_stale_columns_without_renominating() {
    for status in [
        "folding",
        "ready",
        "landed",
        "deferred",
        "gate_failed",
        "gate_error",
        "needs_human",
        "agent_running",
        "agent_blocked",
    ] {
        let db = seeded();
        let before = row(&db);
        let fields = MergeStatusFields {
            result_oid: matches!(status, "ready" | "landed").then(|| "full-result-oid".into()),
            conflict_paths: None,
            error_detail: (status != "folding").then(|| "new diagnostic".into()),
        };
        db.replace_merge_status(WORKTREE, status, &fields).unwrap();
        let after = row(&db);
        assert_eq!(after.status, status);
        assert_eq!(after.result_oid, fields.result_oid);
        assert_eq!(after.conflict_paths, fields.conflict_paths);
        assert_eq!(after.error_detail, fields.error_detail);
        assert_eq!(after.worktree, before.worktree);
        assert_eq!(after.branch, before.branch);
        assert_eq!(after.target_branch, before.target_branch);
        assert_eq!(after.location, before.location);
        assert_eq!(after.agent_attempts, before.agent_attempts);
        assert_eq!(after.queued_at, before.queued_at);
    }
}

#[test]
fn conflict_retry_then_gate_error_has_only_current_error_detail() {
    let db = seeded();
    db.replace_merge_status(
        WORKTREE,
        "deferred",
        &MergeStatusFields {
            conflict_paths: Some("src/a.rs\nmodules/lib".into()),
            error_detail: Some("submodule pointer context".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(db.retry_merge_entry(WORKTREE).unwrap());
    let retry = row(&db);
    assert_eq!(retry.status, "queued");
    assert_eq!(retry.agent_attempts, 0);
    assert!(
        retry.result_oid.is_none()
            && retry.conflict_paths.is_none()
            && retry.error_detail.is_none()
    );
    db.replace_merge_status(
        WORKTREE,
        "gate_error",
        &MergeStatusFields {
            error_detail: Some("gate binary unavailable".into()),
            ..Default::default()
        },
    )
    .unwrap();
    let held = row(&db);
    assert_eq!(held.status, "gate_error");
    assert!(held.result_oid.is_none() && held.conflict_paths.is_none());
    assert_eq!(
        held.error_detail.as_deref(),
        Some("gate binary unavailable")
    );
    assert_eq!(held.agent_attempts, 0);
}

#[test]
fn null_clears_but_empty_string_remains_a_value_and_legacy_none_preserves() {
    let db = seeded();
    db.replace_merge_status(
        WORKTREE,
        "deferred",
        &MergeStatusFields {
            error_detail: Some(String::new()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(row(&db).error_detail.as_deref(), Some(""));
    db.update_merge_status(WORKTREE, "folding", None, None, None)
        .unwrap();
    assert_eq!(row(&db).error_detail.as_deref(), Some(""));
    db.replace_merge_status(WORKTREE, "folding", &MergeStatusFields::default())
        .unwrap();
    assert!(row(&db).error_detail.is_none());
}

#[test]
fn missing_row_and_sql_failure_are_visible_without_other_row_changes() {
    let db = seeded();
    let before = row(&db);
    assert!(
        db.replace_merge_status("/missing", "landed", &MergeStatusFields::default())
            .is_err()
    );
    assert_eq!(row(&db), before);
    db.conn()
        .execute_batch(
            "CREATE TRIGGER refuse_status BEFORE UPDATE ON merge_queue
        BEGIN SELECT RAISE(ABORT,'private fixture refusal'); END;",
        )
        .unwrap();
    assert!(
        db.replace_merge_status(WORKTREE, "landed", &MergeStatusFields::default())
            .is_err()
    );
    assert_eq!(row(&db), before);
    assert!(
        db.replace_merge_status(
            WORKTREE,
            "landed",
            &MergeStatusFields {
                result_oid: Some(String::new()),
                ..Default::default()
            },
        )
        .is_err()
    );
    assert_eq!(row(&db), before);
}

#[test]
fn landed_status_requires_an_effective_nonempty_result_oid() {
    let db = seeded();
    let before = row(&db);

    assert!(
        db.replace_merge_status(WORKTREE, "landed", &MergeStatusFields::default())
            .is_err()
    );
    assert_eq!(row(&db), before);

    // The legacy COALESCE form may re-stamp a landed row while preserving its
    // existing identity, but it cannot create or overwrite one with empty text.
    db.update_merge_status(WORKTREE, "landed", None, None, None)
        .unwrap();
    assert_eq!(row(&db).result_oid.as_deref(), Some("old-oid"));
    let before_empty = row(&db);
    assert!(
        db.update_merge_status(WORKTREE, "landed", Some(""), None, None)
            .is_err()
    );
    assert_eq!(row(&db), before_empty);

    db.replace_merge_status(
        WORKTREE,
        "deferred",
        &MergeStatusFields {
            result_oid: None,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(
        db.update_merge_status(WORKTREE, "landed", None, None, None)
            .is_err()
    );
    assert_eq!(row(&db).status, "deferred");
}

#[test]
fn landed_identity_backfill_is_exact_and_rejects_stale_rows() {
    let db = seeded();
    // Simulate the pre-THE-687 rows that the guarded writers can no longer
    // create; the sweep's repair seam must still be able to recover them.
    db.conn()
        .execute(
            "UPDATE merge_queue SET status='landed', result_oid=NULL",
            [],
        )
        .unwrap();
    let expected = row(&db);
    assert!(
        db.backfill_landed_result_oid(&expected, "derived-oid")
            .unwrap()
    );
    assert_eq!(row(&db).result_oid.as_deref(), Some("derived-oid"));

    db.conn()
        .execute("UPDATE merge_queue SET result_oid=NULL", [])
        .unwrap();
    db.update_merge_status(WORKTREE, "deferred", None, None, None)
        .unwrap();
    assert!(
        !db.backfill_landed_result_oid(&expected, "derived-again")
            .unwrap()
    );
    assert_eq!(row(&db).status, "deferred");
}
