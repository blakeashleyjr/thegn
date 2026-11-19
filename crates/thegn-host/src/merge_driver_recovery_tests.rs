use super::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use thegn_core::store::{MergeCandidate, MergeClaimRequest, MergeStatusFields, WorktreeAuxStore};

const CANDIDATE_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const CURRENT_TIP_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn registered_candidate_row(db: &Db, root: &Path) -> QueueItem {
    let worktree = root.join("feature").to_string_lossy().into_owned();
    db.put_worktree(
        "feature",
        &root.to_string_lossy(),
        &worktree,
        "feature",
        None,
        None,
    )
    .unwrap();
    db.enqueue_merge_candidate(
        &worktree,
        "feature",
        "main",
        &MergeCandidate {
            branch_ref: "refs/heads/feature".into(),
            candidate_oid: CANDIDATE_A.into(),
        },
    )
    .unwrap();
    QueueItem {
        worktree,
        branch: "feature".into(),
        location: String::new(),
    }
}

fn registered_config() -> MergeQueueConfig {
    MergeQueueConfig {
        target_branch: "main".into(),
        organize_folders: false,
        conflict_handoff: ConflictHandoff::Notify,
        ..Default::default()
    }
}

fn ready_actions<A, F, R>(attempt: A, floor: F, runner: R) -> DriveActions<A, F, R>
where
    A: FnMut(&str, &thegn_core::remote::GitLoc, bool) -> anyhow::Result<AttemptOutcome>,
    F: FnMut(&str) -> crate::agent_run::AgentDispatch,
    R: FnMut(
        &str,
        &str,
        &str,
        &str,
        &Failure,
        Option<thegn_core::sandbox::SandboxSpec>,
    ) -> crate::agent_run::AgentRunVerdict,
{
    DriveActions {
        attempt,
        floor,
        runner,
    }
}

fn unexpected_agent_handoff(_: &str) -> crate::agent_run::AgentDispatch {
    panic!("unexpected agent handoff")
}

fn unused_agent_runner(
    _: &str,
    _: &str,
    _: &str,
    _: &str,
    _: &Failure,
    _: Option<thegn_core::sandbox::SandboxSpec>,
) -> crate::agent_run::AgentRunVerdict {
    crate::agent_run::AgentRunVerdict::Exited(Some(1))
}

fn expired_claim_fixture(
    status: &str,
    result_oid: Option<&str>,
) -> (
    tempfile::TempDir,
    std::path::PathBuf,
    Db,
    QueueItem,
    Db,
    thegn_core::store::MergeClaim,
) {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("queue.sqlite");
    let root = dir.path().join("repo");
    std::fs::create_dir(&root).unwrap();
    let db = Db::open_at(&db_path).unwrap();
    let item = registered_candidate_row(&db, &root);
    let former_owner = Db::open_at(&db_path).unwrap();
    let claim = former_owner
        .admit_merge_claim(&MergeClaimRequest {
            worktree: item.worktree.clone(),
            branch: item.branch.clone(),
            repo_root: root.to_string_lossy().into_owned(),
            target_branch: "main".into(),
            location: String::new(),
            expected_status: "queued".into(),
            lease_until_ms: util::now_ms() + 60_000,
            owner_hint: Some("former owner".into()),
        })
        .unwrap()
        .unwrap();
    former_owner
        .replace_merge_status_claimed(
            &claim,
            status,
            &MergeStatusFields {
                result_oid: result_oid.map(str::to_owned),
                ..Default::default()
            },
        )
        .unwrap();
    let sql = rusqlite::Connection::open(&db_path).unwrap();
    sql.execute(
        "UPDATE merge_queue SET claim_lease_until_ms=?2 WHERE worktree=?1",
        rusqlite::params![item.worktree, util::now_ms() - 1],
    )
    .unwrap();
    drop(sql);
    (dir, root, db, item, former_owner, claim)
}

#[test]
fn recovered_folding_row_regates_the_current_tip() {
    let (_dir, root, db, item, _former_owner, _claim) =
        expired_claim_fixture("folding", Some(CANDIDATE_A));
    let attempts = Arc::new(AtomicUsize::new(0));
    let called = Arc::clone(&attempts);
    let outcome = drive_queue_with(
        &registered_config(),
        &Config::default(),
        &root,
        &db,
        vec![item.clone()],
        |_| {},
        ready_actions(
            move |branch, _, _| {
                assert_eq!(branch, "feature");
                called.fetch_add(1, Ordering::Relaxed);
                Ok(AttemptOutcome::Ready {
                    tip: CURRENT_TIP_B.into(),
                })
            },
            unexpected_agent_handoff,
            unused_agent_runner,
        ),
    );

    let row = db.list_merge_queue().unwrap().remove(0);
    assert_eq!(attempts.load(Ordering::Relaxed), 1);
    assert_eq!(outcome.ready, ["feature"]);
    assert_eq!(row.status, "ready");
    assert_eq!(row.result_oid.as_deref(), Some(CURRENT_TIP_B));
    assert_ne!(row.status, "landed");
    assert_eq!(
        db.merge_candidate(&item.worktree)
            .unwrap()
            .unwrap()
            .candidate_oid,
        CANDIDATE_A
    );
}

#[test]
fn recovered_verifying_row_never_lands_without_an_attempt() {
    let (_dir, root, db, item, _former_owner, _claim) =
        expired_claim_fixture("verifying", Some(CANDIDATE_A));
    let attempts = Arc::new(AtomicUsize::new(0));
    let called = Arc::clone(&attempts);
    let outcome = drive_queue_with(
        &registered_config(),
        &Config::default(),
        &root,
        &db,
        vec![item],
        |_| {},
        ready_actions(
            move |_, _, _| {
                called.fetch_add(1, Ordering::Relaxed);
                Ok(AttemptOutcome::Ready {
                    tip: CURRENT_TIP_B.into(),
                })
            },
            unexpected_agent_handoff,
            unused_agent_runner,
        ),
    );

    let row = db.list_merge_queue().unwrap().remove(0);
    assert_eq!(attempts.load(Ordering::Relaxed), 1);
    assert_eq!(outcome.ready, ["feature"]);
    assert_eq!(row.status, "ready");
    assert_ne!(row.status, "landed");
    assert_eq!(row.result_oid.as_deref(), Some(CURRENT_TIP_B));
}

#[test]
fn expired_claim_recovery_advances_the_generation_and_fences_the_old_owner_write() {
    let (_dir, root, db, item, former_owner, claim) =
        expired_claim_fixture("folding", Some(CANDIDATE_A));
    let before = db.observe_merge_claim(&item.worktree).unwrap().unwrap();
    let outcome = drive_queue_with(
        &registered_config(),
        &Config::default(),
        &root,
        &db,
        vec![item.clone()],
        |_| {},
        ready_actions(
            |_, _, _| {
                Ok(AttemptOutcome::Ready {
                    tip: CURRENT_TIP_B.into(),
                })
            },
            unexpected_agent_handoff,
            unused_agent_runner,
        ),
    );

    let after = db.observe_merge_claim(&item.worktree).unwrap().unwrap();
    let stale_write = former_owner.replace_merge_status_claimed(
        &claim,
        "gate_failed",
        &MergeStatusFields::default(),
    );
    assert!(
        matches!(stale_write, Err(error) if error.to_string().contains(thegn_core::store::MERGE_CLAIM_LOST))
    );
    assert_eq!(after.generation, before.generation + 1);
    assert_eq!(after.phase, "unclaimed");
    assert_eq!(db.list_merge_queue().unwrap()[0].status, "ready");
    assert_eq!(outcome.ready, ["feature"]);
}
