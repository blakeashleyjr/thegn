use crate::db::Db;
use crate::pr_queue::{
    PrAgentAdmission, PrAgentAdmit, PrAgentLeaseView, PrqStatus, pr_agent_lease_live,
};
use crate::store::WorktreeAuxStore;

fn enqueue(db: &Db, number: u64) -> String {
    db.enqueue_bound_pr(
        "/repo",
        number,
        Some("/repo/wt"),
        "feature",
        "main",
        "github",
        &format!("https://github.com/me/repo/pull/{number}"),
    )
    .unwrap();
    db.list_pr_queue()
        .unwrap()
        .into_iter()
        .find(|row| row.number == number)
        .unwrap()
        .key
}

fn request(key: &str) -> PrAgentAdmit<'_> {
    PrAgentAdmit {
        key,
        expected_attempts: 0,
        expected_checkout_gen: 0,
        max_attempts: 3,
        head: "admitted-head",
        blocker: "ci",
        detail: "dispatch",
        lease_until_ms: crate::util::now_ms() + 60_000,
        owner_hint: Some("fence test"),
        ci_candidate: None,
    }
}

fn admitted(db: &Db, key: &str) -> crate::pr_queue::PrAgentLease {
    match db.admit_pr_agent(&request(key)).unwrap() {
        PrAgentAdmission::Admitted(lease) => lease,
        refusal => panic!("expected lease admission, got {refusal:?}"),
    }
}

#[test]
fn live_lease_fences_status_and_attempt_writes() {
    let db = Db::open_memory().unwrap();
    let key = enqueue(&db, 1);
    let lease = admitted(&db, &key);

    db.update_pr_status(
        &key,
        PrqStatus::Merged.as_str(),
        None,
        Some("concurrent refresh"),
        Some("other-head"),
    )
    .unwrap();
    db.set_pr_agent_attempts(&key, 0).unwrap();

    let row = db.list_pr_queue().unwrap().remove(0);
    assert_eq!(row.status, PrqStatus::AgentRunning.as_str());
    assert_eq!(row.last_head_oid.as_deref(), Some("admitted-head"));
    assert_eq!(row.agent_attempts, 1);
    assert_eq!(lease.generation, 1);
}

#[test]
fn expired_or_released_lease_does_not_fence() {
    let db = Db::open_memory().unwrap();
    let expired_key = enqueue(&db, 2);
    admitted(&db, &expired_key);
    db.conn()
        .execute(
            "UPDATE pr_queue SET agent_claim_lease_until_ms=0 WHERE key=?1",
            [&expired_key],
        )
        .unwrap();
    db.update_pr_status(&expired_key, PrqStatus::Merged.as_str(), None, None, None)
        .unwrap();
    db.set_pr_agent_attempts(&expired_key, 9).unwrap();
    let expired = db.list_pr_queue().unwrap().remove(0);
    assert_eq!(expired.status, PrqStatus::Merged.as_str());
    assert_eq!(expired.agent_attempts, 9);

    let released_key = enqueue(&db, 3);
    let lease = admitted(&db, &released_key);
    assert!(db.release_pr_agent(&lease, None).unwrap());
    db.update_pr_status(
        &released_key,
        PrqStatus::BlockedCi.as_str(),
        None,
        None,
        None,
    )
    .unwrap();
    db.set_pr_agent_attempts(&released_key, 7).unwrap();
    let released = db
        .list_pr_queue()
        .unwrap()
        .into_iter()
        .find(|row| row.key == released_key)
        .unwrap();
    assert_eq!(released.status, PrqStatus::BlockedCi.as_str());
    assert_eq!(released.agent_attempts, 7);
}

#[test]
fn lease_view_reports_only_owned_live_rows() {
    let db = Db::open_memory().unwrap();
    let key = enqueue(&db, 4);
    assert_eq!(db.pr_agent_lease_view(&key).unwrap(), None);
    let lease = admitted(&db, &key);
    let view = db.pr_agent_lease_view(&key).unwrap().unwrap();
    assert_eq!(view.generation, lease.generation);
    assert_eq!(view.owner_hint.as_deref(), Some("fence test"));
    assert_eq!(db.pr_agent_lease_view("missing").unwrap(), None);
    assert!(db.release_pr_agent(&lease, None).unwrap());
    assert_eq!(db.pr_agent_lease_view(&key).unwrap(), None);
}

#[test]
fn lease_liveness_matches_admission_boundary() {
    let view = PrAgentLeaseView {
        generation: 4,
        lease_until_ms: 100,
        owner_hint: None,
    };
    assert!(pr_agent_lease_live(Some(&view), view.lease_until_ms));
    assert!(!pr_agent_lease_live(Some(&view), view.lease_until_ms + 1));
    assert!(!pr_agent_lease_live(None, view.lease_until_ms));
}
