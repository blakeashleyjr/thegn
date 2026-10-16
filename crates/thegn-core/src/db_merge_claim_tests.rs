use super::*;
use crate::store::WorkspaceStore;
use crate::store::{MergeStatusFields, WorktreeAuxStore};
use std::sync::{Arc, Barrier};

const WT: &str = "/private/repo/feature";

fn fixture(path: &std::path::Path) -> Db {
    let db = Db::open_at(path).unwrap();
    db.put_worktree("feature", "/private/repo", WT, "feature", None, None)
        .unwrap();
    db.enqueue_merge(WT, "feature", "main").unwrap();
    db
}

fn request() -> MergeClaimRequest {
    MergeClaimRequest {
        worktree: WT.into(),
        branch: "feature".into(),
        repo_root: "/private/repo".into(),
        target_branch: "main".into(),
        location: String::new(),
        expected_status: "queued".into(),
        lease_until_ms: util::now_ms() + 60_000,
        owner_hint: Some("worker-a".into()),
    }
}

fn clone_claim(claim: &MergeClaim) -> MergeClaim {
    MergeClaim {
        worktree: claim.worktree.clone(),
        branch: claim.branch.clone(),
        repo_root: claim.repo_root.clone(),
        target_branch: claim.target_branch.clone(),
        location: claim.location.clone(),
        generation: claim.generation,
        phase: claim.phase,
        lease_until_ms: claim.lease_until_ms,
        owner_hint: claim.owner_hint.clone(),
        owner_token: claim.owner_token.clone(),
    }
}

#[test]
fn racing_admissions_on_two_connections_yield_one_owner() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("claims.sqlite");
    drop(fixture(&path));
    let db_a = Db::open_at(&path).unwrap();
    let db_b = Db::open_at(&path).unwrap();
    let barrier = Arc::new(Barrier::new(2));
    let request_a = request();
    let request_b = request();
    let barrier_a = barrier.clone();
    let a = std::thread::spawn(move || {
        barrier_a.wait();
        db_a.admit_merge_claim(&request_a).unwrap()
    });
    let b = std::thread::spawn(move || {
        barrier.wait();
        db_b.admit_merge_claim(&request_b).unwrap()
    });
    let results = [a.join().unwrap(), b.join().unwrap()];
    assert_eq!(results.iter().filter(|claim| claim.is_some()).count(), 1);
    let db = Db::open_at(&path).unwrap();
    assert_eq!(db.observe_merge_claim(WT).unwrap().unwrap().generation, 1);
}

#[test]
fn admission_rechecks_a_lease_after_waiting_for_the_write_lock() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("claims.sqlite");
    let lock_owner = fixture(&path);
    let waiter = Db::open_at(&path).unwrap();
    lock_owner.conn().execute_batch("BEGIN IMMEDIATE").unwrap();
    let lease_until_ms = util::now_ms() + 100;
    let mut request = request();
    request.lease_until_ms = lease_until_ms;
    // rusqlite's busy handler is a plain `fn` pointer, so the "the waiter is now
    // blocked on the write lock" signal goes through a static.
    static BUSY_SEEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    fn note_busy(_: i32) -> bool {
        BUSY_SEEN.store(true, std::sync::atomic::Ordering::Release);
        true
    }
    waiter.conn().busy_handler(Some(note_busy)).unwrap();
    let admission = std::thread::spawn(move || waiter.admit_merge_claim(&request).unwrap());
    while !BUSY_SEEN.load(std::sync::atomic::Ordering::Acquire) {
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    while util::now_ms() < lease_until_ms {
        std::hint::spin_loop();
    }
    lock_owner.conn().execute_batch("COMMIT").unwrap();
    assert!(admission.join().unwrap().is_none());
}

#[test]
fn expired_claim_is_recovered_with_attempts_and_fences_the_late_owner() {
    let dir = tempfile::tempdir().unwrap();
    let db = fixture(&dir.path().join("claims.sqlite"));
    let old = db.admit_merge_claim(&request()).unwrap().unwrap();
    assert_eq!(
        db.consume_merge_agent_attempt(&old, 3).unwrap(),
        MergeAttemptResult::Consumed
    );
    db.conn()
        .execute(
            "UPDATE merge_queue SET claim_lease_until_ms=?1 WHERE worktree=?2",
            params![util::now_ms() - 1, WT],
        )
        .unwrap();
    assert_eq!(
        db.consume_merge_agent_attempt(&old, 3).unwrap(),
        MergeAttemptResult::ClaimLost
    );
    let new = db.admit_merge_claim(&request()).unwrap().unwrap();
    assert_eq!(new.generation, old.generation + 1);
    assert_eq!(db.list_merge_queue().unwrap()[0].agent_attempts, 1);
    let new = db.renew_merge_claim(&new, util::now_ms() + 30_000).unwrap();
    let lost = db.replace_merge_status_claimed(&old, "deferred", &MergeStatusFields::default());
    assert!(lost.unwrap_err().to_string().contains(MERGE_CLAIM_LOST));
    assert_eq!(
        db.consume_merge_agent_attempt(&old, 3).unwrap(),
        MergeAttemptResult::ClaimLost
    );
    assert!(
        db.renew_merge_claim(&old, util::now_ms() + 10_000)
            .unwrap_err()
            .to_string()
            .contains(MERGE_CLAIM_LOST)
    );
    assert!(
        db.settle_merge_claim_no_child(&old)
            .unwrap_err()
            .to_string()
            .contains(MERGE_CLAIM_LOST)
    );
    assert!(
        db.release_merge_claim(&old)
            .unwrap_err()
            .to_string()
            .contains(MERGE_CLAIM_LOST)
    );
    db.replace_merge_status_claimed(&new, "deferred", &MergeStatusFields::default())
        .unwrap();
}

#[test]
fn live_and_unknown_phase_claims_are_held_and_attempts_are_capped() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("claims.sqlite");
    let db = fixture(&path);
    let claim = db.admit_merge_claim(&request()).unwrap().unwrap();
    assert!(db.admit_merge_claim(&request()).unwrap().is_none());
    db.conn().execute("UPDATE merge_queue SET claim_phase='launch_preparing',claim_lease_until_ms=?1 WHERE worktree=?2", params![util::now_ms() - 1, WT]).unwrap();
    assert!(db.admit_merge_claim(&request()).unwrap().is_none());
    db.conn().execute("UPDATE merge_queue SET claim_phase='claimed_no_child',claim_lease_until_ms=?1 WHERE worktree=?2", params![util::now_ms() + 60_000, WT]).unwrap();
    let barrier = Arc::new(Barrier::new(8));
    let mut workers = Vec::new();
    for _ in 0..8 {
        let worker_db = Db::open_at(&path).unwrap();
        let worker_claim = clone_claim(&claim);
        let worker_barrier = barrier.clone();
        workers.push(std::thread::spawn(move || {
            worker_barrier.wait();
            worker_db
                .consume_merge_agent_attempt(&worker_claim, 3)
                .unwrap()
        }));
    }
    let results = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        results
            .iter()
            .filter(|result| **result == MergeAttemptResult::Consumed)
            .count(),
        3
    );
    assert_eq!(
        results
            .iter()
            .filter(|result| **result == MergeAttemptResult::BudgetExhausted)
            .count(),
        5
    );
    assert_eq!(db.list_merge_queue().unwrap()[0].agent_attempts, 3);
    assert_eq!(
        db.consume_merge_agent_attempt(&claim, 0).unwrap(),
        MergeAttemptResult::BudgetExhausted
    );
}

#[test]
fn release_and_legacy_writers_obey_claim_state() {
    let dir = tempfile::tempdir().unwrap();
    let db = fixture(&dir.path().join("claims.sqlite"));
    let claim = db.admit_merge_claim(&request()).unwrap().unwrap();
    let before = db.list_merge_queue().unwrap();
    assert!(
        db.enqueue_merge(WT, "feature", "other")
            .unwrap_err()
            .to_string()
            .contains(MERGE_CLAIM_ACTIVE)
    );
    assert!(
        db.enqueue_merge_if_worktree_matches(WT, "feature", "/private/repo", "", "other")
            .unwrap_err()
            .to_string()
            .contains(MERGE_CLAIM_ACTIVE)
    );
    assert!(
        db.update_merge_status(WT, "deferred", None, None, None)
            .unwrap_err()
            .to_string()
            .contains(MERGE_CLAIM_ACTIVE)
    );
    assert!(
        db.replace_merge_status(WT, "deferred", &MergeStatusFields::default())
            .unwrap_err()
            .to_string()
            .contains(MERGE_CLAIM_ACTIVE)
    );
    assert!(
        db.set_merge_target(WT, "other")
            .unwrap_err()
            .to_string()
            .contains(MERGE_CLAIM_ACTIVE)
    );
    assert!(
        db.set_merge_agent_attempts(WT, 4)
            .unwrap_err()
            .to_string()
            .contains(MERGE_CLAIM_ACTIVE)
    );
    assert!(!db.retry_merge_entry(WT).unwrap());
    assert!(
        db.remove_merge_entry(WT)
            .unwrap_err()
            .to_string()
            .contains(MERGE_CLAIM_ACTIVE)
    );
    assert_eq!(db.list_merge_queue().unwrap(), before);
    assert!(
        db.release_merge_claim(&claim)
            .unwrap_err()
            .to_string()
            .contains(MERGE_CLAIM_LOST)
    );
    let settled = db.settle_merge_claim_no_child(&claim).unwrap();
    db.replace_merge_status_claimed(&settled, "deferred", &MergeStatusFields::default())
        .unwrap();
    db.release_merge_claim(&settled).unwrap();
    assert!(
        db.release_merge_claim(&settled)
            .unwrap_err()
            .to_string()
            .contains(MERGE_CLAIM_LOST)
    );
    let view = db.observe_merge_claim(WT).unwrap().unwrap();
    assert_eq!(view.phase, "unclaimed");
    assert_eq!(view.owner_hint, None);
    assert_eq!(view.generation, claim.generation);
    db.enqueue_merge(WT, "feature", "main").unwrap();
    db.set_merge_target(WT, "main").unwrap();
    db.set_merge_agent_attempts(WT, 2).unwrap();
    assert_eq!(
        db.admit_merge_claim(&request())
            .unwrap()
            .unwrap()
            .generation,
        claim.generation + 1
    );
}

#[test]
fn admission_validates_identity_status_and_lease_and_debug_redacts_token() {
    let dir = tempfile::tempdir().unwrap();
    let db = fixture(&dir.path().join("claims.sqlite"));
    let mut bad = request();
    bad.repo_root = "/wrong".into();
    assert!(db.admit_merge_claim(&bad).unwrap().is_none());
    let mut bad = request();
    bad.branch = "wrong".into();
    assert!(db.admit_merge_claim(&bad).unwrap().is_none());
    let mut bad = request();
    bad.location = "remote-descriptor".into();
    assert!(db.admit_merge_claim(&bad).unwrap().is_none());
    let mut bad = request();
    bad.expected_status = "folding".into();
    assert!(db.admit_merge_claim(&bad).is_err());
    let mut bad = request();
    bad.lease_until_ms = util::now_ms() - 1;
    assert!(db.admit_merge_claim(&bad).unwrap().is_none());
    let mut bad = request();
    bad.lease_until_ms = util::now_ms() + MERGE_CLAIM_MAX_LEASE_MS + 1;
    assert!(db.admit_merge_claim(&bad).unwrap().is_none());
    let mut bad = request();
    bad.owner_hint = Some("x".repeat(MERGE_CLAIM_OWNER_HINT_MAX + 1));
    assert!(db.admit_merge_claim(&bad).is_err());
    let mut bad = request();
    bad.owner_hint = Some("worker\nname".into());
    assert!(db.admit_merge_claim(&bad).is_err());
    let mut bad = request();
    bad.worktree = "bad\npath".into();
    assert!(db.admit_merge_claim(&bad).is_err());
    db.put_worktree(
        "feature",
        "/private/repo",
        WT,
        "feature",
        Some("local"),
        None,
    )
    .unwrap();
    db.conn()
        .execute(
            "UPDATE merge_queue SET location='local' WHERE worktree=?1",
            [WT],
        )
        .unwrap();
    let claim = db.admit_merge_claim(&request()).unwrap().unwrap();
    assert_eq!(claim.location, "local");
    assert!(
        db.renew_merge_claim(&claim, util::now_ms() - 1)
            .unwrap_err()
            .to_string()
            .contains("invalid merge claim lease")
    );
    assert!(
        db.renew_merge_claim(&claim, util::now_ms() + MERGE_CLAIM_MAX_LEASE_MS + 60_000)
            .unwrap_err()
            .to_string()
            .contains("invalid merge claim lease")
    );
    assert!(
        db.replace_merge_status_claimed(&claim, "landed", &MergeStatusFields::default())
            .unwrap_err()
            .to_string()
            .contains("landed merge status requires a nonempty result OID")
    );
    let debug = format!("{claim:?}");
    assert!(debug.contains("[redacted]"));
    assert!(!debug.contains(&claim.owner_token));
    assert_eq!(
        MergeClaimPhase::parse("claimed_no_child"),
        Some(MergeClaimPhase::ClaimedNoChild)
    );
    assert_eq!(MergeClaimPhase::parse("future_phase"), None);
}

#[test]
fn expired_claim_recovers_after_owner_writes_active_status() {
    let dir = tempfile::tempdir().unwrap();
    let db = fixture(&dir.path().join("claims.sqlite"));
    let claim = db.admit_merge_claim(&request()).unwrap().unwrap();
    db.replace_merge_status_claimed(&claim, "folding", &MergeStatusFields::default())
        .unwrap();
    db.conn()
        .execute(
            "UPDATE merge_queue SET claim_lease_until_ms=?1 WHERE worktree=?2",
            params![util::now_ms() - 1, WT],
        )
        .unwrap();
    let recovered = db.admit_merge_claim(&request()).unwrap().unwrap();
    assert_eq!(recovered.generation, 2);
    assert_eq!(db.list_merge_queue().unwrap()[0].status, "folding");
}
