use crate::ci_log::CiLogCandidate;
use crate::db::Db;
use crate::pr_queue::{PrAgentAdmission, PrAgentAdmit};
use crate::store::WorktreeAuxStore;

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

fn request<'a>(key: &'a str, head: &'a str, attempts: u32) -> PrAgentAdmit<'a> {
    PrAgentAdmit {
        key,
        expected_attempts: attempts,
        expected_checkout_gen: 0,
        max_attempts: 3,
        head,
        blocker: "ci",
        detail: "dispatch",
        lease_until_ms: crate::util::now_ms() + 60_000,
        owner_hint: Some("test owner"),
        ci_candidate: None::<&'a CiLogCandidate>,
    }
}

fn admitted(db: &Db, key: &str, head: &str) -> crate::pr_queue::PrAgentLease {
    match db.admit_pr_agent(&request(key, head, 0)).unwrap() {
        PrAgentAdmission::Admitted(lease) => lease,
        other => panic!("expected admission, got {other:?}"),
    }
}

#[test]
fn record_head_is_fenced_on_owner_generation_and_admitted_head() {
    let db = Db::open_memory().unwrap();
    let key = enqueue(&db, 1);
    let lease = admitted(&db, &key, "head-a");
    db.conn()
        .execute(
            "UPDATE pr_queue SET ci_rerun_head='head-a' WHERE key=?1",
            [&key],
        )
        .unwrap();
    assert!(db.record_pr_agent_head(&lease, "head-a", "head-b").unwrap());
    let row = db.list_pr_queue().unwrap().remove(0);
    assert_eq!(row.last_head_oid.as_deref(), Some("head-b"));
    assert_eq!(row.ci_rerun_head, None);
    assert!(!db.record_pr_agent_head(&lease, "head-a", "head-c").unwrap());
}

#[test]
fn stale_generation_cannot_record_head() {
    let db = Db::open_memory().unwrap();
    let key = enqueue(&db, 2);
    let old = admitted(&db, &key, "head-a");
    db.conn()
        .execute(
            "UPDATE pr_queue SET agent_claim_lease_until_ms=0 WHERE key=?1",
            [&key],
        )
        .unwrap();
    let _new = match db.admit_pr_agent(&request(&key, "head-a", 1)).unwrap() {
        PrAgentAdmission::Admitted(lease) => lease,
        result => panic!("expected re-admission, got {result:?}"),
    };
    assert!(!db.record_pr_agent_head(&old, "head-a", "head-b").unwrap());
    assert_eq!(
        db.list_pr_queue().unwrap()[0].last_head_oid.as_deref(),
        Some("head-a")
    );
}

#[test]
fn released_lease_cannot_record_head() {
    let db = Db::open_memory().unwrap();
    let key = enqueue(&db, 3);
    let lease = admitted(&db, &key, "head-a");
    assert!(db.release_pr_agent(&lease, None).unwrap());
    assert!(!db.record_pr_agent_head(&lease, "head-a", "head-b").unwrap());
}

#[test]
fn recorded_own_head_is_neither_foreign_nor_a_budget_reset() {
    let db = Db::open_memory().unwrap();
    let key = enqueue(&db, 4);
    let lease = admitted(&db, &key, "head-a");
    assert!(db.record_pr_agent_head(&lease, "head-a", "head-b").unwrap());
    assert!(db.release_pr_agent(&lease, None).unwrap());
    let row = db.list_pr_queue().unwrap().remove(0);
    assert_eq!(row.last_head_oid.as_deref(), Some("head-b"));
    let cfg = crate::config::PrQueueConfig {
        reset_attempts_on_push: true,
        ..Default::default()
    };
    assert!(!crate::pr_queue::foreign_push(
        row.last_head_oid.as_deref(),
        "head-b"
    ));
    assert!(!crate::pr_queue::attempts_reset(
        row.last_head_oid.as_deref(),
        "head-b",
        &cfg
    ));
    assert!(crate::pr_queue::foreign_push(
        row.last_head_oid.as_deref(),
        "head-d"
    ));
}

#[test]
fn invalid_head_record_is_an_error() {
    let db = Db::open_memory().unwrap();
    let key = enqueue(&db, 5);
    let lease = admitted(&db, &key, "head-a");
    assert!(db.record_pr_agent_head(&lease, "", "head-b").is_err());
    assert!(db.record_pr_agent_head(&lease, "head-a", "head-a").is_err());
}

#[test]
fn record_head_database_error_leaves_row_unchanged() {
    let db = Db::open_memory().unwrap();
    let key = enqueue(&db, 6);
    let lease = admitted(&db, &key, "head-a");
    db.conn()
        .execute_batch(
            "CREATE TRIGGER reject_agent_head BEFORE UPDATE OF last_head_oid ON pr_queue
             BEGIN SELECT RAISE(ABORT, 'injected database failure'); END;",
        )
        .unwrap();

    assert!(db.record_pr_agent_head(&lease, "head-a", "head-b").is_err());
    let row = db.list_pr_queue().unwrap().remove(0);
    assert_eq!(row.last_head_oid.as_deref(), Some("head-a"));
}
