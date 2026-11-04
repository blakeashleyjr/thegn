use super::*;
use crate::store::MERGE_QUEUE_EVENTS_PER_WORKTREE;

const WORKTREE: &str = "/private/repo/feature";

fn queued_db() -> Db {
    let db = Db::open_memory().unwrap();
    db.enqueue_merge_candidate(
        WORKTREE,
        "feature",
        "main",
        &MergeCandidate {
            branch_ref: "refs/heads/feature".into(),
            candidate_oid: "a".repeat(40),
        },
    )
    .unwrap();
    // The enqueue already sets this status; this adds a result OID for the event assertion below.
    db.update_merge_status(WORKTREE, "queued", Some("result-oid"), None, None)
        .unwrap();
    db
}

#[test]
fn refusal_on_an_existing_row_records_one_event_and_changes_nothing() {
    let db = queued_db();
    let before = db.list_merge_queue().unwrap().remove(0);
    let candidate_before = db.merge_candidate(WORKTREE).unwrap();
    assert_eq!(
        candidate_before
            .as_ref()
            .map(|candidate| candidate.candidate_oid.as_str()),
        Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
    );

    assert!(
        db.record_merge_refusal(WORKTREE, "land", "not_ready")
            .unwrap()
    );

    let events = db.merge_queue_events(WORKTREE, 10).unwrap();
    assert_eq!(events.len(), 1);
    let event = &events[0];
    assert_eq!(event.operation, "land");
    assert_eq!(event.outcome, "not_ready");
    assert_eq!(event.old_status, "queued");
    assert_eq!(event.new_status, None);
    assert_eq!(event.row_updated_at, before.updated_at);
    assert_eq!(
        event.candidate_oid,
        candidate_before
            .as_ref()
            .map(|candidate| candidate.candidate_oid.clone())
    );
    assert_eq!(event.result_oid, before.result_oid);
    assert!(event.actor.starts_with("thegn pid "));

    let after = db.list_merge_queue().unwrap().remove(0);
    assert_eq!(after, before);
    assert_eq!(db.merge_candidate(WORKTREE).unwrap(), candidate_before);
}

#[test]
fn refusal_without_a_row_records_nothing() {
    let db = Db::open_memory().unwrap();

    assert!(
        !db.record_merge_refusal(WORKTREE, "land", "not_ready")
            .unwrap()
    );
    assert!(db.merge_queue_events(WORKTREE, 10).unwrap().is_empty());
}

#[test]
fn refusal_on_an_older_events_schema_is_not_recorded() {
    let db = queued_db();
    let before = db.list_merge_queue().unwrap();
    db.conn()
        .execute_batch("ALTER TABLE merge_queue_events DROP COLUMN target_base_oid")
        .unwrap();

    assert!(
        !db.record_merge_refusal(WORKTREE, "land", "not_ready")
            .unwrap()
    );

    let event_count: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM merge_queue_events", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(event_count, 0);
    assert_eq!(db.list_merge_queue().unwrap(), before);
}

#[test]
fn refusal_events_stay_within_the_per_worktree_cap() {
    let db = queued_db();
    for _ in 0..(MERGE_QUEUE_EVENTS_PER_WORKTREE + 5) {
        assert!(
            db.record_merge_refusal(WORKTREE, "land", "not_ready")
                .unwrap()
        );
    }

    assert_eq!(
        db.merge_queue_events(WORKTREE, 1000).unwrap().len(),
        MERGE_QUEUE_EVENTS_PER_WORKTREE
    );
}
