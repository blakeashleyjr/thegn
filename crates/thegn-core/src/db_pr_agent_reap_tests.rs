use crate::db::Db;
use crate::pr_queue::{PrAgentAdmission, PrAgentAdmit, PrqStatus};
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

fn request(key: &str, expected_attempts: u32) -> PrAgentAdmit<'_> {
    PrAgentAdmit {
        key,
        expected_attempts,
        expected_checkout_gen: 0,
        max_attempts: 3,
        head: "admitted-head",
        blocker: "ci",
        detail: "dispatch",
        lease_until_ms: crate::util::now_ms() + 60_000,
        owner_hint: Some("reap test"),
        ci_candidate: None,
    }
}

fn admit(db: &Db, key: &str, expected_attempts: u32) -> crate::pr_queue::PrAgentLease {
    match db.admit_pr_agent(&request(key, expected_attempts)).unwrap() {
        PrAgentAdmission::Admitted(lease) => lease,
        refusal => panic!("expected lease admission, got {refusal:?}"),
    }
}

fn expire(db: &Db, key: &str) {
    db.conn()
        .execute(
            "UPDATE pr_queue SET agent_claim_lease_until_ms=0 WHERE key=?1",
            [key],
        )
        .unwrap();
}

#[test]
fn expired_running_claim_becomes_watching_and_the_old_owner_is_fenced() {
    let db = Db::open_memory().unwrap();
    let key = enqueue(&db, 1);
    let lease = admit(&db, &key, 0);
    expire(&db, &key);

    assert!(
        db.reap_expired_pr_agent(&key, lease.generation, "d")
            .unwrap()
    );
    assert!(
        !db.reap_expired_pr_agent(&key, lease.generation, "duplicate")
            .unwrap()
    );
    let row = db.list_pr_queue().unwrap().remove(0);
    assert_eq!(row.status, PrqStatus::Watching.as_str());
    assert_eq!(row.detail.as_deref(), Some("d"));
    assert_eq!(row.agent_attempts, 1);
    assert_eq!(db.pr_agent_lease_view(&key).unwrap(), None);
    assert!(
        !db.release_pr_agent(&lease, Some(("blocked_ci", "x")))
            .unwrap()
    );
    assert!(
        db.renew_pr_agent(&lease, crate::util::now_ms() + 60_000)
            .is_err()
    );
    assert!(
        !db.record_pr_agent_head(&lease, "admitted-head", &"b".repeat(40))
            .unwrap()
    );
}

#[test]
fn live_claim_is_not_reaped() {
    let db = Db::open_memory().unwrap();
    let key = enqueue(&db, 2);
    let lease = admit(&db, &key, 0);

    assert!(
        !db.reap_expired_pr_agent(&key, lease.generation, "stale")
            .unwrap()
    );
    assert_eq!(
        db.list_pr_queue().unwrap()[0].status,
        PrqStatus::AgentRunning.as_str()
    );
}

#[test]
fn reap_is_fenced_to_the_observed_generation() {
    let db = Db::open_memory().unwrap();
    let key = enqueue(&db, 3);
    let old_lease = admit(&db, &key, 0);
    expire(&db, &key);

    assert!(
        !db.reap_expired_pr_agent(&key, old_lease.generation + 1, "stale")
            .unwrap()
    );
    let new_lease = admit(&db, &key, 1);
    assert_eq!(new_lease.generation, old_lease.generation + 1);
    assert!(
        !db.reap_expired_pr_agent(&key, old_lease.generation, "stale")
            .unwrap()
    );
    assert_eq!(
        db.pr_agent_lease_view(&key).unwrap().unwrap().generation,
        new_lease.generation
    );
}

#[test]
fn reap_keeps_a_non_running_status() {
    let db = Db::open_memory().unwrap();
    let key = enqueue(&db, 4);
    let lease = admit(&db, &key, 0);
    expire(&db, &key);
    db.conn()
        .execute(
            "UPDATE pr_queue SET status='blocked_ci' WHERE key=?1",
            [&key],
        )
        .unwrap();

    assert!(
        db.reap_expired_pr_agent(&key, lease.generation, "stale")
            .unwrap()
    );
    let row = db.list_pr_queue().unwrap().remove(0);
    assert_eq!(row.status, PrqStatus::BlockedCi.as_str());
    assert_eq!(db.pr_agent_lease_view(&key).unwrap(), None);
}
