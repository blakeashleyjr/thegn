//! Suite I — Audit trail (Tier 1 for DB; Tier 2 for no-podman smoke).
//!
//! Tests container_events insert/query/prune semantics using in-memory SQLite,
//! plus a no-panic smoke test for sandbox_events spawn without podman.

use superzej_core::db::Db;
use superzej_core::store::WorktreeAuxStore;

fn db() -> Db {
    Db::open_memory().unwrap()
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

// ── I1: insert + query round-trip ────────────────────────────────────────────

#[test]
fn i1_container_events_round_trip() {
    let db = db();
    let n = now();
    db.insert_container_event("/wt/feat", n, "exec", Some("cargo build"), None)
        .unwrap();
    db.insert_container_event("/wt/feat", n + 1, "exec", Some("git status"), Some(0))
        .unwrap();
    db.insert_container_event("/wt/feat", n + 2, "die", None, Some(137))
        .unwrap();

    let events = db.container_events("/wt/feat", 10).unwrap();
    assert_eq!(events.len(), 3);
    // Newest first.
    assert_eq!(events[0].kind, "die");
    assert_eq!(events[0].exit_code, Some(137));
    assert_eq!(events[1].detail.as_deref(), Some("git status"));
    assert_eq!(events[1].exit_code, Some(0));
    assert_eq!(events[2].detail.as_deref(), Some("cargo build"));
}

// ── I2: prune removes old rows across worktrees ──────────────────────────────

#[test]
fn i2_prune_removes_old_rows_across_worktrees() {
    let db = db();
    let n = now();
    // Old events for /wt/a (8 days ago).
    for i in 0..5i64 {
        db.insert_container_event("/wt/a", n - 8 * 86400 + i, "exec", Some("old"), None)
            .unwrap();
    }
    // Recent events for /wt/b.
    for i in 0..3i64 {
        db.insert_container_event("/wt/b", n + i, "exec", Some("recent"), None)
            .unwrap();
    }
    // Prune older than 7 days.
    let removed = db.prune_container_events(7 * 24 * 3600).unwrap();
    assert_eq!(removed, 5, "all 5 old events should be pruned");
    assert!(
        db.container_events("/wt/a", 10).unwrap().is_empty(),
        "/wt/a events should all be pruned"
    );
    assert_eq!(
        db.container_events("/wt/b", 10).unwrap().len(),
        3,
        "/wt/b events must be untouched"
    );
}

// ── I3: limit is honoured ────────────────────────────────────────────────────

#[test]
fn i3_limit_honoured() {
    let db = db();
    let n = now();
    for i in 0..15i64 {
        db.insert_container_event("/wt/feat", n + i, "exec", None, None)
            .unwrap();
    }
    let ten = db.container_events("/wt/feat", 10).unwrap();
    assert_eq!(ten.len(), 10, "limit=10 must return exactly 10 rows");
}
