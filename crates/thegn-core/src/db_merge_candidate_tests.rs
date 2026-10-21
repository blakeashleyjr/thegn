use super::*;
use crate::store::{MergeCandidate, MergeClaimRequest, MergeFinalOutcome, MergeFinalStatus};
use crate::store::{WorkspaceStore, WorktreeAuxStore};

const WORKTREE: &str = "/private/repo/feature";
const REPO: &str = "/private/repo";
const OID_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const OID_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const OID_SHA256: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";

fn candidate(oid: &str) -> MergeCandidate {
    MergeCandidate {
        branch_ref: "refs/heads/feature".into(),
        candidate_oid: oid.into(),
    }
}

fn bound(db: &Db, oid: &str) {
    db.enqueue_merge_candidate(WORKTREE, "feature", "main", &candidate(oid))
        .unwrap();
}

#[test]
fn bound_enqueue_records_ref_and_oid() {
    let db = Db::open_memory().unwrap();
    bound(&db, OID_A);
    assert_eq!(
        db.merge_candidate(WORKTREE).unwrap(),
        Some(candidate(OID_A))
    );
    bound(&db, OID_SHA256);
    assert_eq!(
        db.merge_candidate(WORKTREE).unwrap(),
        Some(candidate(OID_SHA256))
    );
}

#[test]
fn unbound_enqueue_clears_a_bound_pair() {
    let db = Db::open_memory().unwrap();
    bound(&db, OID_A);
    db.enqueue_merge(WORKTREE, "feature", "main").unwrap();
    assert_eq!(db.merge_candidate(WORKTREE).unwrap(), None);
}

#[test]
fn reenqueue_adopts_the_new_oid() {
    let db = Db::open_memory().unwrap();
    bound(&db, OID_A);
    bound(&db, OID_B);
    assert_eq!(
        db.merge_candidate(WORKTREE).unwrap(),
        Some(candidate(OID_B))
    );
}

#[test]
fn held_candidate_adoption_is_compare_and_swap_and_requires_folding() {
    let db = Db::open_memory().unwrap();
    bound(&db, OID_A);
    assert!(
        !db.adopt_merge_candidate(WORKTREE, &candidate(OID_A), OID_B)
            .unwrap()
    );
    db.replace_merge_status(
        WORKTREE,
        "folding",
        &crate::store::MergeStatusFields::default(),
    )
    .unwrap();
    assert!(
        db.adopt_merge_candidate(WORKTREE, &candidate(OID_A), OID_B)
            .unwrap()
    );
    assert_eq!(
        db.merge_candidate(WORKTREE).unwrap(),
        Some(candidate(OID_B))
    );
    assert!(
        !db.adopt_merge_candidate(WORKTREE, &candidate(OID_A), OID_B)
            .unwrap()
    );
}

#[test]
fn malformed_candidate_is_refused_and_writes_nothing() {
    let db = Db::open_memory().unwrap();
    let uppercase = format!("{}A", "a".repeat(39));
    let mut non_hex = OID_A.to_owned();
    non_hex.replace_range(..1, "g");
    for invalid in [
        MergeCandidate {
            branch_ref: "feature".into(),
            candidate_oid: OID_A.into(),
        },
        MergeCandidate {
            branch_ref: "refs/heads/feature".into(),
            candidate_oid: OID_A[..4].into(),
        },
        MergeCandidate {
            branch_ref: "refs/heads/feature".into(),
            candidate_oid: uppercase,
        },
        MergeCandidate {
            branch_ref: "refs/heads/feature".into(),
            candidate_oid: non_hex,
        },
    ] {
        assert!(
            db.enqueue_merge_candidate(WORKTREE, "feature", "main", &invalid)
                .is_err()
        );
        assert!(db.list_merge_queue().unwrap().is_empty());
        assert_eq!(db.merge_candidate(WORKTREE).unwrap(), None);
    }
}

#[test]
fn outcome_retry_and_claim_preserve_the_pair() {
    let db = Db::open_memory().unwrap();
    db.put_worktree("feature", REPO, WORKTREE, "feature", None, None)
        .unwrap();
    bound(&db, OID_A);
    let observed = db.observe_merge_outcome(WORKTREE).unwrap();
    assert_eq!(
        db.persist_merge_outcome(
            &observed,
            &MergeFinalOutcome {
                worktree: WORKTREE,
                branch: "feature",
                repo_root: REPO,
                target_branch: "main",
                location: "",
                status: MergeFinalStatus::Deferred,
                result_oid: None,
                conflict_paths: None,
                error_detail: Some("retry me"),
            },
        )
        .unwrap(),
        crate::store::MergeOutcomeWrite::Written
    );
    assert_eq!(db.list_merge_queue().unwrap()[0].status, "deferred");
    assert!(db.retry_merge_entry(WORKTREE).unwrap());
    assert_eq!(
        db.merge_candidate(WORKTREE).unwrap(),
        Some(candidate(OID_A))
    );
    let claim = db
        .admit_merge_claim(&MergeClaimRequest {
            worktree: WORKTREE.into(),
            branch: "feature".into(),
            repo_root: REPO.into(),
            target_branch: "main".into(),
            location: String::new(),
            expected_status: "queued".into(),
            lease_until_ms: crate::util::now_ms() + 60_000,
            owner_hint: Some("candidate-test".into()),
        })
        .unwrap()
        .unwrap();
    assert_eq!(
        db.merge_candidate(WORKTREE).unwrap(),
        Some(candidate(OID_A))
    );
    let settled = db.settle_merge_claim_no_child(&claim).unwrap();
    assert_eq!(
        db.merge_candidate(WORKTREE).unwrap(),
        Some(candidate(OID_A))
    );
    db.release_merge_claim(&settled).unwrap();
    assert_eq!(
        db.merge_candidate(WORKTREE).unwrap(),
        Some(candidate(OID_A))
    );
}

#[test]
fn remote_matched_enqueue_is_unbound() {
    let db = Db::open_memory().unwrap();
    db.put_worktree(
        "feature",
        REPO,
        WORKTREE,
        "feature",
        Some("remote-location"),
        None,
    )
    .unwrap();
    bound(&db, OID_A);
    assert!(
        db.enqueue_merge_if_worktree_matches(WORKTREE, "feature", REPO, "remote-location", "main")
            .unwrap()
    );
    assert_eq!(db.merge_candidate(WORKTREE).unwrap(), None);
}
