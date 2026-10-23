use crate::db::Db;
use crate::pr_queue::{PrAgentAdmission, PrAgentAdmit};
use crate::store::{PrClearOutcome, PrRemoval, WorktreeAuxStore};

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

fn admit(db: &Db, key: &str) {
    let request = PrAgentAdmit {
        key,
        expected_attempts: 0,
        expected_checkout_gen: 0,
        max_attempts: 3,
        head: "head-a",
        blocker: "ci",
        detail: "dispatch",
        lease_until_ms: crate::util::now_ms() + 60_000,
        owner_hint: Some("test owner"),
        ci_candidate: None,
    };
    assert!(matches!(
        db.admit_pr_agent(&request).unwrap(),
        PrAgentAdmission::Admitted(_)
    ));
}

#[test]
fn remove_refuses_a_row_with_a_live_agent_lease() {
    let db = Db::open_memory().unwrap();
    let key = enqueue(&db, 1);
    admit(&db, &key);

    assert_eq!(db.remove_pr_entry(&key).unwrap(), PrRemoval::AgentRunning);
    assert_eq!(db.list_pr_queue().unwrap().len(), 1);
}

#[test]
fn remove_deletes_a_row_whose_lease_expired() {
    let db = Db::open_memory().unwrap();
    let key = enqueue(&db, 1);
    admit(&db, &key);
    db.conn()
        .execute(
            "UPDATE pr_queue SET agent_claim_lease_until_ms=0 WHERE key=?1",
            [&key],
        )
        .unwrap();

    assert_eq!(db.remove_pr_entry(&key).unwrap(), PrRemoval::Removed);
    assert!(db.list_pr_queue().unwrap().is_empty());
}

#[test]
fn remove_deletes_a_row_without_an_agent_lease() {
    let db = Db::open_memory().unwrap();
    let key = enqueue(&db, 1);

    assert_eq!(db.remove_pr_entry(&key).unwrap(), PrRemoval::Removed);
    assert!(db.list_pr_queue().unwrap().is_empty());
}

#[test]
fn remove_reports_a_missing_row() {
    let db = Db::open_memory().unwrap();

    assert_eq!(db.remove_pr_entry("missing").unwrap(), PrRemoval::Missing);
}

#[test]
fn clear_keeps_rows_with_a_live_agent_lease() {
    let db = Db::open_memory().unwrap();
    enqueue(&db, 1);
    let key = enqueue(&db, 2);
    enqueue(&db, 3);
    admit(&db, &key);

    assert_eq!(
        db.clear_pr_queue("/repo").unwrap(),
        PrClearOutcome {
            removed: 2,
            kept_running: vec![2],
        }
    );
    let rows = db.list_pr_queue().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].number, 2);
}

#[test]
fn clear_removes_rows_whose_lease_expired() {
    let db = Db::open_memory().unwrap();
    let key = enqueue(&db, 1);
    admit(&db, &key);
    db.conn()
        .execute(
            "UPDATE pr_queue SET agent_claim_lease_until_ms=0 WHERE key=?1",
            [&key],
        )
        .unwrap();

    assert_eq!(
        db.clear_pr_queue("/repo").unwrap(),
        PrClearOutcome {
            removed: 1,
            kept_running: Vec::new(),
        }
    );
    assert!(db.list_pr_queue().unwrap().is_empty());
}
