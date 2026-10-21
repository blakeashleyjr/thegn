use super::*;
use crate::ci_log::CiLogCandidate;
use crate::db::{Db, PrRebind};
use crate::pr_queue::{PrAgentAdmission, PrAgentAdmit, PrAgentRefusal, PrqStatus};
use crate::store::WorktreeAuxStore;
use std::sync::{Arc, Barrier};

fn enqueue(db: &Db, n: u64) -> String {
    db.enqueue_bound_pr(
        "/repo",
        n,
        Some("/repo/wt"),
        "feature",
        "main",
        "github",
        &format!("https://github.com/me/repo/pull/{n}"),
    )
    .unwrap();
    db.list_pr_queue()
        .unwrap()
        .into_iter()
        .find(|row| row.number == n)
        .unwrap()
        .key
}

fn request<'a>(
    key: &'a str,
    attempts: u32,
    candidate: Option<&'a CiLogCandidate>,
) -> PrAgentAdmit<'a> {
    PrAgentAdmit {
        key,
        expected_attempts: attempts,
        expected_checkout_gen: 0,
        max_attempts: 3,
        head: "head-a",
        blocker: "ci",
        detail: "dispatch",
        lease_until_ms: crate::util::now_ms() + 60_000,
        owner_hint: Some("test owner"),
        ci_candidate: candidate,
    }
}

fn admitted(db: &Db, key: &str, attempts: u32) -> crate::pr_queue::PrAgentLease {
    match db.admit_pr_agent(&request(key, attempts, None)).unwrap() {
        PrAgentAdmission::Admitted(lease) => lease,
        result => panic!("expected admission, got {result:?}"),
    }
}

#[test]
fn admit_spends_one_attempt_and_bumps_generation() {
    let db = Db::open_memory().unwrap();
    let key = enqueue(&db, 1);
    let lease = admitted(&db, &key, 0);
    let row = db.list_pr_queue().unwrap().remove(0);
    assert_eq!(
        (row.agent_attempts, row.status.as_str()),
        (1, "agent_running")
    );
    assert_eq!(lease.generation, 1);
    assert_eq!(lease.attempt, 1);
    let debug = format!("{lease:?}");
    assert!(debug.contains("[redacted]"));
    assert!(!debug.contains(&lease.owner_token));
}

#[test]
fn refusal_variants_have_operator_facing_messages() {
    let cases = [
        (PrAgentRefusal::Missing, "no longer exists"),
        (PrAgentRefusal::InvalidLease, "lease is invalid"),
        (
            PrAgentRefusal::Settled {
                status: "merged".into(),
            },
            "settled (merged)",
        ),
        (
            PrAgentRefusal::Held {
                generation: 2,
                lease_until_ms: 10,
            },
            "generation 2",
        ),
        (
            PrAgentRefusal::Stale {
                reason: "checkout differs".into(),
            },
            "checkout differs",
        ),
        (
            PrAgentRefusal::BudgetExhausted {
                attempts: 3,
                max: 3,
            },
            "(3/3)",
        ),
        (PrAgentRefusal::DuplicateCandidate, "already dispatched"),
    ];
    for (refusal, expected) in cases {
        assert!(refusal.to_string().contains(expected), "{refusal:?}");
    }
    let admission = PrAgentAdmission::Refused(PrAgentRefusal::DuplicateCandidate);
    assert!(format!("{admission:?}").contains("DuplicateCandidate"));
}

#[test]
fn second_admission_while_lease_live_is_refused_held() {
    let db = Db::open_memory().unwrap();
    let key = enqueue(&db, 2);
    admitted(&db, &key, 0);
    assert!(matches!(
        db.admit_pr_agent(&request(&key, 1, None)).unwrap(),
        PrAgentAdmission::Refused(PrAgentRefusal::Held { .. })
    ));
}

#[test]
fn two_handles_racing_admit_exactly_one() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("race.db");
    let db = Db::open_at(&path).unwrap();
    let key = enqueue(&db, 3);
    let barrier = Arc::new(Barrier::new(2));
    let mut handles = Vec::new();
    for _ in 0..2 {
        let path = path.clone();
        let key = key.clone();
        let barrier = Arc::clone(&barrier);
        handles.push(std::thread::spawn(move || {
            let db = Db::open_at(&path).unwrap();
            let req = request(&key, 0, None);
            barrier.wait();
            matches!(db.admit_pr_agent(&req), Ok(PrAgentAdmission::Admitted(_)))
        }));
    }
    let admitted_count = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .filter(|won| *won)
        .count();
    assert_eq!(admitted_count, 1);
    assert_eq!(db.list_pr_queue().unwrap()[0].agent_attempts, 1);
}

#[test]
fn budget_exhausted_refuses_without_spend() {
    let db = Db::open_memory().unwrap();
    let key = enqueue(&db, 4);
    db.set_pr_agent_attempts(&key, 3).unwrap();
    let mut req = request(&key, 3, None);
    req.max_attempts = 3;
    assert!(matches!(
        db.admit_pr_agent(&req).unwrap(),
        PrAgentAdmission::Refused(PrAgentRefusal::BudgetExhausted { .. })
    ));
    assert_eq!(db.list_pr_queue().unwrap()[0].agent_attempts, 3);
}

#[test]
fn missing_and_invalid_lease_are_typed_refusals() {
    let db = Db::open_memory().unwrap();
    let mut missing = request("missing", 0, None);
    assert!(matches!(
        db.admit_pr_agent(&missing).unwrap(),
        PrAgentAdmission::Refused(PrAgentRefusal::Missing)
    ));
    let key = enqueue(&db, 14);
    missing.key = &key;
    missing.lease_until_ms = crate::util::now_ms();
    assert!(matches!(
        db.admit_pr_agent(&missing).unwrap(),
        PrAgentAdmission::Refused(PrAgentRefusal::InvalidLease)
    ));
}

#[test]
fn renewal_is_fenced_and_updates_the_returned_lease() {
    let db = Db::open_memory().unwrap();
    let key = enqueue(&db, 15);
    let lease = admitted(&db, &key, 0);
    let renewed = db
        .renew_pr_agent(&lease, crate::util::now_ms() + 90_000)
        .unwrap();
    assert!(renewed.lease_until_ms > lease.lease_until_ms);
    assert!(db.renew_pr_agent(&renewed, crate::util::now_ms()).is_err());
}

#[test]
fn stale_attempt_or_checkout_snapshot_refuses() {
    let db = Db::open_memory().unwrap();
    let key = enqueue(&db, 5);
    assert!(matches!(
        db.admit_pr_agent(&request(&key, 1, None)).unwrap(),
        PrAgentAdmission::Refused(PrAgentRefusal::Stale { .. })
    ));
    let mut req = request(&key, 0, None);
    req.expected_checkout_gen = 8;
    assert!(matches!(
        db.admit_pr_agent(&req).unwrap(),
        PrAgentAdmission::Refused(PrAgentRefusal::Stale { .. })
    ));
}

#[test]
fn settled_row_is_refused() {
    let db = Db::open_memory().unwrap();
    let key = enqueue(&db, 6);
    db.update_pr_status(&key, PrqStatus::Merged.as_str(), None, None, None)
        .unwrap();
    assert!(matches!(
        db.admit_pr_agent(&request(&key, 0, None)).unwrap(),
        PrAgentAdmission::Refused(PrAgentRefusal::Settled { .. })
    ));
}

#[test]
fn expired_lease_is_taken_over_and_old_owner_cannot_renew_or_release() {
    let db = Db::open_memory().unwrap();
    let key = enqueue(&db, 7);
    let old = admitted(&db, &key, 0);
    db.conn()
        .execute(
            "UPDATE pr_queue SET agent_claim_lease_until_ms=0 WHERE key=?1",
            [&key],
        )
        .unwrap();
    let new = admitted(&db, &key, 1);
    assert!(new.generation > old.generation);
    assert!(
        db.renew_pr_agent(&old, crate::util::now_ms() + 60_000)
            .unwrap_err()
            .to_string()
            .contains(crate::pr_queue::PR_AGENT_LEASE_LOST)
    );
    assert!(!db.release_pr_agent(&old, None).unwrap());
}

#[test]
fn duplicate_ci_candidate_refuses_and_spends_nothing() {
    let db = Db::open_memory().unwrap();
    let first_key = enqueue(&db, 8);
    let second_key = enqueue(&db, 9);
    let candidate = CiLogCandidate {
        worktree: "/repo/wt".into(),
        run_id: "run".into(),
        job_id: "job".into(),
        head_sha: "head-a".into(),
    };
    assert!(matches!(
        db.admit_pr_agent(&request(&first_key, 0, Some(&candidate)))
            .unwrap(),
        PrAgentAdmission::Admitted(_)
    ));
    assert!(matches!(
        db.admit_pr_agent(&request(&second_key, 0, Some(&candidate)))
            .unwrap(),
        PrAgentAdmission::Refused(PrAgentRefusal::DuplicateCandidate)
    ));
    assert_eq!(
        db.list_pr_queue()
            .unwrap()
            .into_iter()
            .find(|row| row.key == second_key)
            .unwrap()
            .agent_attempts,
        0
    );
}

#[test]
fn refused_admission_leaves_ci_candidate_unclaimed() {
    let db = Db::open_memory().unwrap();
    let key = enqueue(&db, 10);
    db.set_pr_agent_attempts(&key, 3).unwrap();
    let candidate = CiLogCandidate {
        worktree: "/repo/wt".into(),
        run_id: "run".into(),
        job_id: "budget".into(),
        head_sha: "head-a".into(),
    };
    assert!(matches!(
        db.admit_pr_agent(&request(&key, 3, Some(&candidate)))
            .unwrap(),
        PrAgentAdmission::Refused(PrAgentRefusal::BudgetExhausted { .. })
    ));
    assert!(db.claim_ci_autofix(&candidate).unwrap());
}

#[test]
fn database_failure_rolls_back_ci_candidate_and_attempt() {
    let db = Db::open_memory().unwrap();
    let key = enqueue(&db, 16);
    db.conn().execute_batch("CREATE TRIGGER deny_pr_agent BEFORE UPDATE OF agent_attempts ON pr_queue BEGIN SELECT RAISE(ABORT, 'denied'); END;").unwrap();
    let candidate = CiLogCandidate {
        worktree: "/repo/wt".into(),
        run_id: "run".into(),
        job_id: "failure".into(),
        head_sha: "head-a".into(),
    };
    assert!(
        db.admit_pr_agent(&request(&key, 0, Some(&candidate)))
            .is_err()
    );
    assert_eq!(db.list_pr_queue().unwrap()[0].agent_attempts, 0);
    assert!(db.claim_ci_autofix(&candidate).unwrap());
}

#[test]
fn unchanged_admission_update_is_an_error_and_rolls_back_candidate() {
    let db = Db::open_memory().unwrap();
    let key = enqueue(&db, 17);
    db.conn().execute_batch("CREATE TRIGGER ignore_pr_agent BEFORE UPDATE ON pr_queue BEGIN SELECT RAISE(IGNORE); END;").unwrap();
    let candidate = CiLogCandidate {
        worktree: "/repo/wt".into(),
        run_id: "run".into(),
        job_id: "ignored".into(),
        head_sha: "head-a".into(),
    };
    assert!(
        db.admit_pr_agent(&request(&key, 0, Some(&candidate)))
            .is_err()
    );
    assert!(db.claim_ci_autofix(&candidate).unwrap());
}

#[test]
fn release_restores_status_only_while_agent_running() {
    let db = Db::open_memory().unwrap();
    let key = enqueue(&db, 11);
    let lease = admitted(&db, &key, 0);
    db.update_pr_status(
        &key,
        PrqStatus::Merged.as_str(),
        None,
        None,
        Some("newer-head"),
    )
    .unwrap();
    assert!(
        db.release_pr_agent(&lease, Some((PrqStatus::BlockedCi.as_str(), "done")))
            .unwrap()
    );
    let row = db.list_pr_queue().unwrap().remove(0);
    assert_eq!(row.status, "merged");
    assert_eq!(row.last_head_oid.as_deref(), Some("newer-head"));

    let key = enqueue(&db, 12);
    let lease = admitted(&db, &key, 0);
    assert!(
        db.release_pr_agent(&lease, Some((PrqStatus::BlockedCi.as_str(), "done")))
            .unwrap()
    );
    let row = db
        .list_pr_queue()
        .unwrap()
        .into_iter()
        .find(|row| row.key == key)
        .unwrap();
    assert_eq!(row.status, "blocked_ci");
}

#[test]
fn rebind_refuses_while_lease_live() {
    let db = Db::open_memory().unwrap();
    let key = enqueue(&db, 13);
    admitted(&db, &key, 0);
    let binding = PrRebind {
        worktree: Some("/repo/wt2".into()),
        branch: "other".into(),
        base_branch: "main".into(),
        forge: "github".into(),
        pr_url: "https://github.com/me/repo/pull/13".into(),
    };
    assert!(
        db.rebind_pr(&key, &binding)
            .unwrap_err()
            .to_string()
            .contains("a PR agent is running")
    );
}
