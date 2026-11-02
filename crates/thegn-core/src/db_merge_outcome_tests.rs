use super::*;
use crate::store::WorkspaceStore;

const WORKTREE: &str = "/private/repo/feature";
const REPO: &str = "/private/repo";

fn outcome(status: MergeFinalStatus) -> MergeFinalOutcome<'static> {
    MergeFinalOutcome {
        worktree: WORKTREE,
        branch: "feature",
        repo_root: REPO,
        target_branch: "main",
        location: "",
        status,
        operation: MergeOutcomeOperation::Land,
        result_oid: (status == MergeFinalStatus::Landed).then_some("verified-commit"),
        conflict_paths: None,
        error_detail: None,
    }
}

fn registered(db: &Db) {
    db.put_worktree("repo-feature", REPO, WORKTREE, "feature", None, None)
        .unwrap();
}

#[test]
fn registry_identity_accessor_borrows_the_captured_observation() {
    let db = Db::open_memory().unwrap();
    let absent = db.observe_merge_outcome(WORKTREE).unwrap();
    assert!(absent.registry_identity().is_none());
    registered(&db);
    let observed = db.observe_merge_outcome(WORKTREE).unwrap();
    let identity = observed.registry_identity().unwrap();
    assert_eq!(identity.branch.as_deref(), Some("feature"));
    assert_eq!(identity.repo_path.as_deref(), Some(REPO));
    db.set_worktree_location(WORKTREE, "changed").unwrap();
    assert_eq!(observed.registry_identity(), Some(identity));
    assert_ne!(
        db.observe_merge_outcome(WORKTREE)
            .unwrap()
            .registry_identity(),
        Some(identity)
    );
}

fn row(db: &Db) -> MergeQueueRow {
    let rows = db.list_merge_queue().unwrap();
    assert_eq!(rows.len(), 1);
    rows.into_iter().next().unwrap()
}

#[test]
fn claim_aware_persist_writes_without_claim_columns() {
    let db = Db::open_memory().unwrap();
    registered(&db);
    db.conn()
        .execute_batch("ALTER TABLE merge_queue DROP COLUMN claim_owner")
        .unwrap();
    let observed = db.observe_merge_outcome(WORKTREE).unwrap();

    assert_eq!(
        db.persist_merge_outcome(&observed, &outcome(MergeFinalStatus::Landed))
            .unwrap(),
        MergeOutcomeWrite::Written
    );
    assert_eq!(row(&db).status, "landed");
}

fn forbid_queued(db: &Db) {
    db.conn()
        .execute_batch(
            "CREATE TRIGGER forbid_queued_insert BEFORE INSERT ON merge_queue \
             WHEN NEW.status='queued' BEGIN SELECT RAISE(ABORT,'intermediate queued insert'); END; \
             CREATE TRIGGER forbid_queued_update BEFORE UPDATE ON merge_queue \
             WHEN NEW.status='queued' BEGIN SELECT RAISE(ABORT,'intermediate queued update'); END;",
        )
        .unwrap();
}

#[test]
fn final_outcomes_never_insert_or_update_queued() {
    for status in [
        MergeFinalStatus::Landed,
        MergeFinalStatus::Deferred,
        MergeFinalStatus::GateFailed,
        MergeFinalStatus::GateError,
    ] {
        let db = Db::open_memory().unwrap();
        forbid_queued(&db);
        let mut final_row = outcome(status);
        final_row.error_detail = Some("final detail");
        for _ in 0..2 {
            let observed = db.observe_merge_outcome(WORKTREE).unwrap();
            assert_eq!(
                db.persist_merge_outcome(&observed, &final_row).unwrap(),
                MergeOutcomeWrite::Written
            );
            let saved = row(&db);
            assert_eq!(saved.status, status.as_str());
            assert_eq!(saved.result_oid.as_deref(), final_row.result_oid);
            assert_eq!(saved.error_detail.as_deref(), Some("final detail"));
            assert!(saved.conflict_paths.is_none());
        }
        // Prove the tripwire is active, rather than silently accepting queued.
        assert!(db.enqueue_merge(WORKTREE, "feature", "main").is_err());
    }
}

#[test]
fn repeated_landed_finalization_preserves_the_existing_row_and_grace_clock() {
    let db = Db::open_memory().unwrap();
    let observed = db.observe_merge_outcome(WORKTREE).unwrap();
    db.persist_merge_outcome(&observed, &outcome(MergeFinalStatus::Landed))
        .unwrap();
    db.conn()
        .execute(
            "UPDATE merge_queue SET queued_at=11, updated_at=12, result_oid='existing-commit', error_detail='keep-me'",
            [],
        )
        .unwrap();
    let before = row(&db);
    let observed = db.observe_merge_outcome(WORKTREE).unwrap();
    assert_eq!(
        db.persist_merge_outcome(&observed, &outcome(MergeFinalStatus::Landed))
            .unwrap(),
        MergeOutcomeWrite::Written
    );
    assert_eq!(row(&db), before);
}

#[test]
fn final_update_clears_nullable_fields_preserves_nomination_and_attempts() {
    let db = Db::open_memory().unwrap();
    registered(&db);
    db.enqueue_merge(WORKTREE, "feature", "old-target").unwrap();
    db.update_merge_status(
        WORKTREE,
        "gate_failed",
        Some("stale-result"),
        Some("stale-path"),
        Some("stale-error"),
    )
    .unwrap();
    db.set_merge_agent_attempts(WORKTREE, 7).unwrap();
    db.conn()
        .execute("UPDATE merge_queue SET queued_at=11,updated_at=12", [])
        .unwrap();
    forbid_queued(&db);
    let observed = db.observe_merge_outcome(WORKTREE).unwrap();
    assert_eq!(
        db.persist_merge_outcome(&observed, &outcome(MergeFinalStatus::GateError))
            .unwrap(),
        MergeOutcomeWrite::Written
    );
    let saved = row(&db);
    assert_eq!(saved.queued_at, 11);
    assert!(saved.updated_at > 12);
    assert_eq!(saved.agent_attempts, 7);
    assert_eq!(saved.target_branch, "main");
    assert!(saved.result_oid.is_none());
    assert!(saved.conflict_paths.is_none());
    assert!(saved.error_detail.is_none());
}

#[test]
fn final_write_error_rolls_back_without_leaving_a_transaction() {
    for existing in [false, true] {
        let db = Db::open_memory().unwrap();
        if existing {
            db.enqueue_merge(WORKTREE, "feature", "main").unwrap();
        }
        let observed = db.observe_merge_outcome(WORKTREE).unwrap();
        db.conn()
            .execute_batch(
                "CREATE TRIGGER fail_final AFTER INSERT ON merge_queue \
                 BEGIN SELECT RAISE(ABORT,'injected final insert failure'); END; \
                 CREATE TRIGGER fail_final_update AFTER UPDATE ON merge_queue \
                 BEGIN SELECT RAISE(ABORT,'injected final update failure'); END;",
            )
            .unwrap();
        assert!(
            db.persist_merge_outcome(&observed, &outcome(MergeFinalStatus::GateError))
                .is_err()
        );
        assert!(db.conn().is_autocommit());
        assert_eq!(db.observe_merge_outcome(WORKTREE).unwrap(), observed);
        db.conn()
            .execute_batch("DROP TRIGGER fail_final; DROP TRIGGER fail_final_update;")
            .unwrap();
        assert_eq!(
            db.persist_merge_outcome(&observed, &outcome(MergeFinalStatus::GateError))
                .unwrap(),
            MergeOutcomeWrite::Written
        );
    }
}

#[test]
fn independent_connection_registry_reassignments_win() {
    for mutation in [
        "UPDATE worktrees SET branch='replacement'",
        "UPDATE worktrees SET repo_path='/other/repo'",
        "UPDATE worktrees SET location='remote:other'",
        "UPDATE worktrees SET location=''",
        "DELETE FROM worktrees",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.sqlite");
        let a = Db::open_at(&path).unwrap();
        let b = Db::open_at(&path).unwrap();
        registered(&a);
        a.enqueue_merge(WORKTREE, "feature", "main").unwrap();
        let observed = a.observe_merge_outcome(WORKTREE).unwrap();
        b.conn().execute_batch(mutation).unwrap();
        let newer = b.observe_merge_outcome(WORKTREE).unwrap();
        assert_eq!(
            a.persist_merge_outcome(&observed, &outcome(MergeFinalStatus::Landed))
                .unwrap(),
            MergeOutcomeWrite::RegistryChanged,
            "{mutation}"
        );
        assert!(a.conn().is_autocommit());
        assert_eq!(b.observe_merge_outcome(WORKTREE).unwrap(), newer);
    }
}

#[test]
fn independent_connection_queue_reassignments_win() {
    for mutation in [
        "UPDATE merge_queue SET branch='replacement'",
        "UPDATE merge_queue SET target_branch='release'",
        "UPDATE merge_queue SET location='remote:other'",
        "UPDATE merge_queue SET location=''",
        "UPDATE merge_queue SET agent_attempts=8",
        "UPDATE merge_queue SET status='agent_running'",
        "UPDATE merge_queue SET error_detail='new failure'",
        "UPDATE merge_queue SET result_oid='new result'",
        "UPDATE merge_queue SET conflict_paths='new path'",
        "UPDATE merge_queue SET queued_at=queued_at+1",
        "UPDATE merge_queue SET updated_at=updated_at+1",
        "DELETE FROM merge_queue",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.sqlite");
        let a = Db::open_at(&path).unwrap();
        let b = Db::open_at(&path).unwrap();
        registered(&a);
        a.enqueue_merge(WORKTREE, "feature", "main").unwrap();
        let observed = a.observe_merge_outcome(WORKTREE).unwrap();
        b.conn().execute_batch(mutation).unwrap();
        let newer = b.observe_merge_outcome(WORKTREE).unwrap();
        assert_eq!(
            a.persist_merge_outcome(&observed, &outcome(MergeFinalStatus::Landed))
                .unwrap(),
            MergeOutcomeWrite::QueueChanged,
            "{mutation}"
        );
        assert!(a.conn().is_autocommit());
        assert_eq!(b.observe_merge_outcome(WORKTREE).unwrap(), newer);
    }
}

#[test]
fn observed_absence_does_not_authorize_overwriting_a_new_owner() {
    for registry in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.sqlite");
        let a = Db::open_at(&path).unwrap();
        let b = Db::open_at(&path).unwrap();
        let observed = a.observe_merge_outcome(WORKTREE).unwrap();
        if registry {
            registered(&b);
        } else {
            b.enqueue_merge(WORKTREE, "feature", "main").unwrap();
        }
        let newer = b.observe_merge_outcome(WORKTREE).unwrap();
        assert_eq!(
            a.persist_merge_outcome(&observed, &outcome(MergeFinalStatus::GateError))
                .unwrap(),
            if registry {
                MergeOutcomeWrite::RegistryChanged
            } else {
                MergeOutcomeWrite::QueueChanged
            }
        );
        assert_eq!(b.observe_merge_outcome(WORKTREE).unwrap(), newer);
    }
}

#[test]
fn final_outcome_rejects_mismatched_observed_context() {
    let db = Db::open_memory().unwrap();
    registered(&db);
    db.enqueue_merge(WORKTREE, "feature", "main").unwrap();
    let observed = db.observe_merge_outcome(WORKTREE).unwrap();
    for (field, value) in [
        ("worktree", "/other/worktree"),
        ("branch", "other"),
        ("repo_root", "/other/repo"),
        ("location", "remote:other"),
        ("target_branch", ""),
    ] {
        let mut final_row = outcome(MergeFinalStatus::GateError);
        match field {
            "worktree" => final_row.worktree = value,
            "branch" => final_row.branch = value,
            "repo_root" => final_row.repo_root = value,
            "location" => final_row.location = value,
            "target_branch" => final_row.target_branch = value,
            _ => unreachable!(),
        }
        assert!(db.persist_merge_outcome(&observed, &final_row).is_err());
        assert_eq!(db.observe_merge_outcome(WORKTREE).unwrap(), observed);
    }
}

#[test]
fn queue_branch_and_location_are_checked_even_without_registry() {
    for (branch, location) in [("other", ""), ("feature", "remote:other")] {
        let db = Db::open_memory().unwrap();
        db.enqueue_merge(WORKTREE, branch, "main").unwrap();
        db.conn()
            .execute("UPDATE merge_queue SET location=?1", [location])
            .unwrap();
        let observed = db.observe_merge_outcome(WORKTREE).unwrap();
        assert!(
            db.persist_merge_outcome(&observed, &outcome(MergeFinalStatus::Landed))
                .is_err()
        );
        assert_eq!(db.observe_merge_outcome(WORKTREE).unwrap(), observed);
    }
}

#[test]
fn incomplete_registry_and_unobserved_remote_location_are_refused() {
    let db = Db::open_memory().unwrap();
    let observed = db.observe_merge_outcome(WORKTREE).unwrap();
    let mut final_row = outcome(MergeFinalStatus::GateError);
    final_row.location = "remote:other";
    assert!(db.persist_merge_outcome(&observed, &final_row).is_err());
    db.conn()
        .execute("INSERT INTO worktrees(worktree) VALUES(?1)", [WORKTREE])
        .unwrap();
    let observed = db.observe_merge_outcome(WORKTREE).unwrap();
    assert!(
        db.persist_merge_outcome(&observed, &outcome(MergeFinalStatus::GateError))
            .is_err()
    );
    assert!(db.list_merge_queue().unwrap().is_empty());
}

#[test]
fn local_aliases_and_remote_locations_keep_verified_representation() {
    for location in [None, Some(""), Some("local"), Some("remote:verified")] {
        let db = Db::open_memory().unwrap();
        db.put_worktree("repo-feature", REPO, WORKTREE, "feature", location, None)
            .unwrap();
        let observed = db.observe_merge_outcome(WORKTREE).unwrap();
        let mut final_row = outcome(MergeFinalStatus::GateError);
        final_row.location = match location {
            Some("remote:verified") => "remote:verified",
            _ => "local",
        };
        assert_eq!(
            db.persist_merge_outcome(&observed, &final_row).unwrap(),
            MergeOutcomeWrite::Written
        );
        let raw: Option<String> = db
            .conn()
            .query_row("SELECT location FROM merge_queue", [], |r| r.get(0))
            .unwrap();
        assert_eq!(raw.as_deref(), location);
    }
}

#[test]
fn result_oid_requires_landed_and_landed_requires_a_result() {
    let db = Db::open_memory().unwrap();
    let observed = db.observe_merge_outcome(WORKTREE).unwrap();
    for (status, oid) in [
        (MergeFinalStatus::Landed, None),
        (MergeFinalStatus::Landed, Some("")),
        (MergeFinalStatus::GateError, Some("speculative")),
    ] {
        let mut final_row = outcome(status);
        final_row.result_oid = oid;
        assert!(db.persist_merge_outcome(&observed, &final_row).is_err());
    }
    assert!(db.list_merge_queue().unwrap().is_empty());
}

#[test]
fn independent_reader_snapshot_sees_old_or_final_never_synthetic_queued() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.sqlite");
    let writer = Db::open_at(&path).unwrap();
    let reader = Db::open_at(&path).unwrap();
    let observed = writer.observe_merge_outcome(WORKTREE).unwrap();
    writer
        .persist_merge_outcome(&observed, &outcome(MergeFinalStatus::Deferred))
        .unwrap();
    forbid_queued(&writer);
    let before = row(&reader);
    let read_tx = reader.conn().unchecked_transaction().unwrap();
    assert_eq!(row(&reader), before); // establish the independent WAL snapshot
    let observed = writer.observe_merge_outcome(WORKTREE).unwrap();
    writer
        .persist_merge_outcome(&observed, &outcome(MergeFinalStatus::Landed))
        .unwrap();
    assert_eq!(row(&reader), before);
    read_tx.commit().unwrap();
    assert_eq!(row(&reader).status, "landed");
}

#[test]
fn concurrent_distinct_finalizers_do_not_overwrite_the_winner() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.sqlite");
    let a = Db::open_at(&path).unwrap();
    let b = Db::open_at(&path).unwrap();
    let observed = a.observe_merge_outcome(WORKTREE).unwrap();
    assert_eq!(observed, b.observe_merge_outcome(WORKTREE).unwrap());
    let start = std::sync::Barrier::new(2);
    let (first, second) = std::thread::scope(|scope| {
        let left_observed = &observed;
        let right_observed = &observed;
        let start = &start;
        let left = scope.spawn(move || {
            start.wait();
            a.persist_merge_outcome(left_observed, &outcome(MergeFinalStatus::Deferred))
        });
        let right = scope.spawn(move || {
            start.wait();
            b.persist_merge_outcome(right_observed, &outcome(MergeFinalStatus::GateError))
        });
        (
            left.join().unwrap().unwrap(),
            right.join().unwrap().unwrap(),
        )
    });
    assert!(matches!(
        (first, second),
        (MergeOutcomeWrite::Written, MergeOutcomeWrite::QueueChanged)
            | (MergeOutcomeWrite::QueueChanged, MergeOutcomeWrite::Written)
    ));
    let read = Db::open_at(&path).unwrap();
    assert_eq!(
        row(&read).status,
        if first == MergeOutcomeWrite::Written {
            "deferred"
        } else {
            "gate_error"
        }
    );
}

#[test]
fn queue_aware_land_records_one_transition_event() {
    let db = Db::open_memory().unwrap();
    registered(&db);
    db.enqueue_merge(WORKTREE, "feature", "main").unwrap();
    db.update_merge_status(WORKTREE, "ready", None, None, None)
        .unwrap();
    let observed = db.observe_merge_outcome(WORKTREE).unwrap();

    assert_eq!(
        db.persist_merge_outcome(&observed, &outcome(MergeFinalStatus::Landed))
            .unwrap(),
        MergeOutcomeWrite::Written
    );
    let events = db.merge_queue_events(WORKTREE, 10).unwrap();
    assert_eq!(events.len(), 1);
    let event = &events[0];
    assert_eq!(event.operation, "land");
    assert_eq!(event.outcome, "transition");
    assert_eq!(event.old_status, "ready");
    assert_eq!(event.new_status.as_deref(), Some("landed"));
    assert_eq!(event.result_oid.as_deref(), Some("verified-commit"));
    assert!(event.actor.starts_with("thegn pid "));
}

#[test]
fn raw_land_of_an_unqueued_worktree_records_a_created_event() {
    let db = Db::open_memory().unwrap();
    let observed = db.observe_merge_outcome(WORKTREE).unwrap();
    let mut final_outcome = outcome(MergeFinalStatus::Landed);
    final_outcome.operation = MergeOutcomeOperation::RawLand;

    assert_eq!(
        db.persist_merge_outcome(&observed, &final_outcome).unwrap(),
        MergeOutcomeWrite::Written
    );
    let events = db.merge_queue_events(WORKTREE, 10).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].operation, "raw_land");
    assert_eq!(events[0].outcome, "created");
    assert_eq!(events[0].old_status, "");
    assert_eq!(events[0].new_status.as_deref(), Some("landed"));
}

#[test]
fn repeated_land_records_no_second_event() {
    let db = Db::open_memory().unwrap();
    let observed = db.observe_merge_outcome(WORKTREE).unwrap();
    db.persist_merge_outcome(&observed, &outcome(MergeFinalStatus::Landed))
        .unwrap();
    let observed = db.observe_merge_outcome(WORKTREE).unwrap();
    db.persist_merge_outcome(&observed, &outcome(MergeFinalStatus::Landed))
        .unwrap();

    assert_eq!(db.merge_queue_events(WORKTREE, 10).unwrap().len(), 1);
}

#[test]
fn integrate_same_status_records_no_event() {
    let db = Db::open_memory().unwrap();
    let mut final_outcome = outcome(MergeFinalStatus::Deferred);
    final_outcome.operation = MergeOutcomeOperation::Integrate;
    let observed = db.observe_merge_outcome(WORKTREE).unwrap();
    db.persist_merge_outcome(&observed, &final_outcome).unwrap();
    let observed = db.observe_merge_outcome(WORKTREE).unwrap();
    db.persist_merge_outcome(&observed, &final_outcome).unwrap();

    let events = db.merge_queue_events(WORKTREE, 10).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].operation, "integrate");
    assert_eq!(events[0].outcome, "created");
}

#[test]
fn claimed_or_changed_outcome_records_no_event() {
    let db = Db::open_memory().unwrap();
    db.enqueue_merge(WORKTREE, "feature", "main").unwrap();
    let observed = db.observe_merge_outcome(WORKTREE).unwrap();
    db.update_merge_status(WORKTREE, "ready", None, None, None)
        .unwrap();

    assert_eq!(
        db.persist_merge_outcome(&observed, &outcome(MergeFinalStatus::Landed))
            .unwrap(),
        MergeOutcomeWrite::QueueChanged
    );
    assert!(db.merge_queue_events(WORKTREE, 10).unwrap().is_empty());

    let fresh_observation = db.observe_merge_outcome(WORKTREE).unwrap();
    db.persist_merge_outcome(&fresh_observation, &outcome(MergeFinalStatus::Landed))
        .unwrap();
    let events = db.merge_queue_events(WORKTREE, 10).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].old_status, "ready");
}

#[test]
fn event_failure_rolls_back_the_outcome() {
    let db = Db::open_memory().unwrap();
    db.enqueue_merge(WORKTREE, "feature", "main").unwrap();
    db.update_merge_status(WORKTREE, "ready", None, None, None)
        .unwrap();
    let observed = db.observe_merge_outcome(WORKTREE).unwrap();
    db.conn()
        .execute_batch(
            "CREATE TRIGGER reject_event BEFORE INSERT ON merge_queue_events \
             BEGIN SELECT RAISE(ABORT, 'event refused'); END;",
        )
        .unwrap();

    assert!(
        db.persist_merge_outcome(&observed, &outcome(MergeFinalStatus::Landed))
            .is_err()
    );
    assert_eq!(row(&db).status, "ready");
    assert!(db.conn().is_autocommit());
}

#[test]
fn compat_schema_without_event_column_still_writes_the_outcome() {
    let db = Db::open_memory().unwrap();
    db.conn()
        .execute_batch("ALTER TABLE merge_queue_events DROP COLUMN target_base_oid")
        .unwrap();
    let observed = db.observe_merge_outcome(WORKTREE).unwrap();

    assert_eq!(
        db.persist_merge_outcome(&observed, &outcome(MergeFinalStatus::Landed))
            .unwrap(),
        MergeOutcomeWrite::Written
    );
    assert_eq!(row(&db).status, "landed");
}
