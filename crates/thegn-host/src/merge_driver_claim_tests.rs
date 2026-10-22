use super::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use thegn_core::store::{MergeClaimRequest, MergeClaimView};

fn registered_row(db: &Db, root: &Path, worktree: &str, branch: &str) -> QueueItem {
    let root = root.to_string_lossy().into_owned();
    db.put_worktree(branch, &root, worktree, branch, None, None)
        .unwrap();
    db.enqueue_merge(worktree, branch, "main").unwrap();
    QueueItem {
        worktree: worktree.to_owned(),
        branch: branch.to_owned(),
        location: String::new(),
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

fn registered_config() -> MergeQueueConfig {
    MergeQueueConfig {
        target_branch: "main".into(),
        organize_folders: false,
        conflict_handoff: ConflictHandoff::Notify,
        ..Default::default()
    }
}

#[test]
fn racing_drains_on_two_connections_have_one_owner() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("queue.sqlite");
    let root = dir.path().join("repo");
    std::fs::create_dir(&root).unwrap();
    let db_a = Db::open_at(&db_path).unwrap();
    let item = registered_row(&db_a, &root, "/worktrees/feature", "feature");
    db_a.set_merge_agent_attempts(&item.worktree, 1).unwrap();
    let worktree = item.worktree.clone();
    let db_b = Db::open_at(&db_path).unwrap();
    let root_a = root.clone();
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let winner = std::thread::spawn(move || {
        drive_queue_with(
            &registered_config(),
            &Config::default(),
            &root_a,
            &db_a,
            vec![item],
            |_| {},
            ready_actions(
                move |_, _, _| {
                    started_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    Ok(AttemptOutcome::Ready { tip: "tip".into() })
                },
                unexpected_agent_handoff,
                unused_agent_runner,
            ),
        )
    });
    started_rx.recv().unwrap();
    let loser_attempts = Arc::new(AtomicUsize::new(0));
    let loser_count = Arc::clone(&loser_attempts);
    let loser = drive_queue_with(
        &registered_config(),
        &Config::default(),
        &root,
        &db_b,
        vec![QueueItem {
            worktree: "/worktrees/feature".into(),
            branch: "feature".into(),
            location: String::new(),
        }],
        |_| {},
        ready_actions(
            move |_, _, _| {
                loser_count.fetch_add(1, Ordering::Relaxed);
                Ok(AttemptOutcome::Ready {
                    tip: "wrong".into(),
                })
            },
            unexpected_agent_handoff,
            unused_agent_runner,
        ),
    );
    let attempts_after_loser = loser_attempts.load(Ordering::Relaxed);
    let persisted_attempts = db_b
        .list_merge_queue()
        .unwrap()
        .into_iter()
        .find(|row| row.worktree == worktree)
        .unwrap()
        .agent_attempts;
    release_tx.send(()).unwrap();
    let winner = winner.join().unwrap();
    assert_eq!(attempts_after_loser, 0);
    assert_eq!(persisted_attempts, 1);
    assert_eq!(loser.warnings.len(), 1);
    assert!(loser.warnings[0].contains("claimed by another drain"));
    assert_eq!(winner.ready, ["feature"]);
}

#[test]
fn row_held_by_another_connection_is_skipped_and_the_next_row_lands() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("queue.sqlite");
    let root = dir.path().join("repo");
    std::fs::create_dir(&root).unwrap();
    let db = Db::open_at(&db_path).unwrap();
    let first = registered_row(&db, &root, "/worktrees/first", "first");
    let second = registered_row(&db, &root, "/worktrees/second", "second");
    let holder = Db::open_at(&db_path).unwrap();
    let root_s = root.to_string_lossy().into_owned();
    holder
        .admit_merge_claim(&MergeClaimRequest {
            worktree: first.worktree.clone(),
            branch: first.branch.clone(),
            repo_root: root_s,
            target_branch: "main".into(),
            location: String::new(),
            expected_status: "queued".into(),
            lease_until_ms: util::now_ms() + 60_000,
            owner_hint: Some("holder".into()),
        })
        .unwrap()
        .unwrap();
    let attempted = Arc::new(AtomicUsize::new(0));
    let called = Arc::clone(&attempted);
    let mut progressed = Vec::new();
    let outcome = drive_queue_with(
        &registered_config(),
        &Config::default(),
        &root,
        &db,
        vec![first.clone(), second],
        |step| progressed.push(step.worktree.to_owned()),
        ready_actions(
            move |_, _, _| {
                called.fetch_add(1, Ordering::Relaxed);
                Ok(AttemptOutcome::Ready { tip: "tip".into() })
            },
            unexpected_agent_handoff,
            unused_agent_runner,
        ),
    );
    assert_eq!(attempted.load(Ordering::Relaxed), 1);
    assert!(
        !progressed
            .iter()
            .any(|worktree| worktree == &first.worktree)
    );
    assert!(
        progressed
            .iter()
            .any(|worktree| worktree == "/worktrees/second")
    );
    assert_eq!(
        db.list_merge_queue()
            .unwrap()
            .into_iter()
            .find(|row| row.worktree == first.worktree)
            .unwrap()
            .status,
        "queued"
    );
    assert_eq!(outcome.ready, ["second"]);
    assert!(outcome.warnings[0].contains("claimed by another drain"));
}

#[test]
fn claimed_row_is_released_after_the_drain() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("queue.sqlite");
    let root = dir.path().join("repo");
    std::fs::create_dir(&root).unwrap();
    let db = Db::open_at(&db_path).unwrap();
    let worktree = root.join("worktree").to_string_lossy().into_owned();
    let item = registered_row(&db, &root, &worktree, "feature");
    let before = db.observe_merge_claim(&item.worktree).unwrap().unwrap();
    drive_queue_with(
        &registered_config(),
        &Config::default(),
        &root,
        &db,
        vec![item.clone()],
        |_| {},
        ready_actions(
            |_, _, _| Ok(AttemptOutcome::Ready { tip: "tip".into() }),
            unexpected_agent_handoff,
            unused_agent_runner,
        ),
    );
    let after = db.observe_merge_claim(&item.worktree).unwrap().unwrap();
    assert_eq!(after.phase, "unclaimed");
    assert_eq!(after.generation, before.generation + 1);
    assert!(claim::heartbeat_joined(&item.worktree));
}

#[test]
fn budget_is_consumed_through_the_claim() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("queue.sqlite");
    let root = dir.path().join("repo");
    std::fs::create_dir(&root).unwrap();
    let db = Db::open_at(&db_path).unwrap();
    let item = registered_row(&db, &root, "/worktrees/feature", "feature");
    let cfg = MergeQueueConfig {
        conflict_handoff: ConflictHandoff::Agent,
        agent_command: "agent".into(),
        agent_max_attempts: 2,
        ..registered_config()
    };
    let runs = Arc::new(AtomicUsize::new(0));
    let runner_runs = Arc::clone(&runs);
    let attempt_db_path = db_path.clone();
    let attempt_worktree = item.worktree.clone();
    let outcome = drive_queue_with(
        &cfg,
        &Config::default(),
        &root,
        &db,
        vec![item],
        |_| {},
        DriveActions {
            attempt: move |_: &str, _: &thegn_core::remote::GitLoc, _: bool| {
                let connection = rusqlite::Connection::open(&attempt_db_path)?;
                connection.execute(
                    "UPDATE merge_queue SET agent_attempts = 2 WHERE worktree = ?1",
                    [&attempt_worktree],
                )?;
                Ok(AttemptOutcome::GateFailed { log: "red".into() })
            },
            floor: |_: &str| crate::agent_run::AgentDispatch::Run(None),
            runner: move |_: &str,
                          _: &str,
                          _: &str,
                          _: &str,
                          _: &Failure,
                          _: Option<thegn_core::sandbox::SandboxSpec>| {
                runner_runs.fetch_add(1, Ordering::Relaxed);
                crate::agent_run::AgentRunVerdict::Exited(Some(0))
            },
        },
    );
    assert_eq!(runs.load(Ordering::Relaxed), 0);
    assert_eq!(outcome.needs_human, ["feature"]);
    let row = db.list_merge_queue().unwrap().remove(0);
    assert_eq!(row.agent_attempts, 2);
    assert_eq!(row.status, "needs_human");
}

#[test]
fn unregistered_row_without_git_attribution_is_skipped() {
    let db = Db::open_memory().unwrap();
    db.enqueue_merge("/ad-hoc", "feature", "main").unwrap();
    let outcome = drive_queue_with(
        &registered_config(),
        &Config::default(),
        Path::new("/repo"),
        &db,
        vec![QueueItem {
            worktree: "/ad-hoc".into(),
            branch: "feature".into(),
            location: String::new(),
        }],
        |_| {},
        ready_actions(
            |_, _, _| Ok(AttemptOutcome::Ready { tip: "tip".into() }),
            unexpected_agent_handoff,
            unused_agent_runner,
        ),
    );
    assert!(outcome.ready.is_empty());
    assert_eq!(outcome.warnings.len(), 1);
    assert!(outcome.warnings[0].contains("not attributable"));
    assert_eq!(db.list_merge_queue().unwrap()[0].status, "queued");
    assert_eq!(
        db.observe_merge_claim("/ad-hoc").unwrap().unwrap().phase,
        "unclaimed"
    );
}

#[test]
fn stale_folding_row_is_skipped_with_a_warning() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("queue.sqlite");
    let root = dir.path().join("repo");
    std::fs::create_dir(&root).unwrap();
    let db = Db::open_at(&db_path).unwrap();
    let item = registered_row(&db, &root, "/worktrees/feature", "feature");
    db.replace_merge_status(&item.worktree, "folding", &MergeStatusFields::default())
        .unwrap();
    let outcome = drive_queue_with(
        &registered_config(),
        &Config::default(),
        &root,
        &db,
        vec![item],
        |_| {},
        ready_actions(
            |_, _, _| panic!("stale row must not be attempted"),
            unexpected_agent_handoff,
            unused_agent_runner,
        ),
    );
    assert_eq!(db.list_merge_queue().unwrap()[0].status, "folding");
    assert!(outcome.warnings[0].contains("not claimable"));
}

#[test]
fn expired_claim_target_filter_keeps_item_for_claim_recovery() {
    let expired = MergeClaimView {
        worktree: "/worktrees/feature".into(),
        generation: 2,
        phase: "claimed_no_child".into(),
        lease_until_ms: Some(99),
        owner_hint: Some("stale drain".into()),
    };
    assert_eq!(
        target_stamp_action(true, Some(&expired), 100),
        TargetStampAction::RetryWithoutStamp
    );
}

#[test]
fn live_claim_target_filter_skips_item() {
    let live = MergeClaimView {
        worktree: "/worktrees/feature".into(),
        generation: 2,
        phase: "claimed_no_child".into(),
        lease_until_ms: Some(101),
        owner_hint: Some("active drain".into()),
    };
    assert_eq!(
        target_stamp_action(true, Some(&live), 100),
        TargetStampAction::SkipHeld
    );
}

#[test]
fn expired_claim_on_in_progress_row_is_recovered_by_the_next_drain() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("queue.sqlite");
    let root = dir.path().join("repo");
    std::fs::create_dir(&root).unwrap();
    let db = Db::open_at(&db_path).unwrap();
    let worktree = root.join("feature").to_string_lossy().into_owned();
    let item = registered_row(&db, &root, &worktree, "feature");
    let holder = Db::open_at(&db_path).unwrap();
    let claim = holder
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
    holder
        .replace_merge_status_claimed(&claim, "folding", &MergeStatusFields::default())
        .unwrap();
    let before = db.observe_merge_claim(&item.worktree).unwrap().unwrap();
    let sql = rusqlite::Connection::open(&db_path).unwrap();
    sql.execute(
        "UPDATE merge_queue SET claim_lease_until_ms=?2 WHERE worktree=?1",
        rusqlite::params![item.worktree, util::now_ms() - 1],
    )
    .unwrap();
    drop(sql);
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
            move |_, _, _| {
                called.fetch_add(1, Ordering::Relaxed);
                Ok(AttemptOutcome::Ready { tip: "tip".into() })
            },
            unexpected_agent_handoff,
            unused_agent_runner,
        ),
    );
    let after = db.observe_merge_claim(&item.worktree).unwrap().unwrap();
    assert_eq!(attempts.load(Ordering::Relaxed), 1);
    assert_eq!(outcome.ready, ["feature"]);
    assert!(
        outcome
            .warnings
            .iter()
            .all(|warning| !warning.contains("not claimable"))
    );
    assert_eq!(db.list_merge_queue().unwrap()[0].status, "ready");
    assert_eq!(after.phase, "unclaimed");
    // Recovery admission advances the generation; release preserves it.
    assert_eq!(after.generation, before.generation + 1);
}

#[test]
fn expired_recovery_fences_the_former_owner() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("queue.sqlite");
    let root = dir.path().join("repo");
    std::fs::create_dir(&root).unwrap();
    let db = Db::open_at(&db_path).unwrap();
    let worktree = root.join("feature").to_string_lossy().into_owned();
    let item = registered_row(&db, &root, &worktree, "feature");
    let holder = Db::open_at(&db_path).unwrap();
    let claim = holder
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
    holder
        .replace_merge_status_claimed(&claim, "folding", &MergeStatusFields::default())
        .unwrap();
    let sql = rusqlite::Connection::open(&db_path).unwrap();
    sql.execute(
        "UPDATE merge_queue SET claim_lease_until_ms=?2 WHERE worktree=?1",
        rusqlite::params![item.worktree, util::now_ms() - 1],
    )
    .unwrap();
    drop(sql);
    let outcome = drive_queue_with(
        &registered_config(),
        &Config::default(),
        &root,
        &db,
        vec![item.clone()],
        |_| {},
        ready_actions(
            |_, _, _| Ok(AttemptOutcome::Ready { tip: "tip".into() }),
            unexpected_agent_handoff,
            unused_agent_runner,
        ),
    );
    let stale_write =
        holder.replace_merge_status_claimed(&claim, "gate_failed", &MergeStatusFields::default());
    assert!(
        matches!(stale_write, Err(error) if error.to_string().contains(thegn_core::store::MERGE_CLAIM_LOST))
    );
    assert_eq!(db.list_merge_queue().unwrap()[0].status, "ready");
    assert_eq!(outcome.ready, ["feature"]);
}

#[test]
fn live_claim_on_in_progress_row_is_still_skipped() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("queue.sqlite");
    let root = dir.path().join("repo");
    std::fs::create_dir(&root).unwrap();
    let db = Db::open_at(&db_path).unwrap();
    let worktree = root.join("feature").to_string_lossy().into_owned();
    let item = registered_row(&db, &root, &worktree, "feature");
    let holder = Db::open_at(&db_path).unwrap();
    let claim = holder
        .admit_merge_claim(&MergeClaimRequest {
            worktree: item.worktree.clone(),
            branch: item.branch.clone(),
            repo_root: root.to_string_lossy().into_owned(),
            target_branch: "main".into(),
            location: String::new(),
            expected_status: "queued".into(),
            lease_until_ms: util::now_ms() + 60_000,
            owner_hint: Some("live owner".into()),
        })
        .unwrap()
        .unwrap();
    holder
        .replace_merge_status_claimed(&claim, "folding", &MergeStatusFields::default())
        .unwrap();
    let outcome = drive_queue_with(
        &registered_config(),
        &Config::default(),
        &root,
        &db,
        vec![item],
        |_| {},
        ready_actions(
            |_, _, _| panic!("live in-progress claim must not be attempted"),
            unexpected_agent_handoff,
            unused_agent_runner,
        ),
    );
    assert!(
        outcome
            .warnings
            .iter()
            .any(|warning| warning.contains("claimed by another drain"))
    );
    assert_eq!(db.list_merge_queue().unwrap()[0].status, "folding");
}

#[test]
fn expired_recoverable_decision_table() {
    let now_ms = 100;
    let cases = [
        ("unclaimed", None, false),
        ("unclaimed", Some(now_ms), false),
        ("unclaimed", Some(now_ms - 1), false),
        ("unclaimed", Some(now_ms + 1), false),
        ("claimed_no_child", None, false),
        ("claimed_no_child", Some(now_ms), false),
        ("claimed_no_child", Some(now_ms - 1), true),
        ("claimed_no_child", Some(now_ms + 1), false),
        ("settled", None, false),
        ("settled", Some(now_ms), false),
        ("settled", Some(now_ms - 1), true),
        ("settled", Some(now_ms + 1), false),
        ("future-phase", None, false),
        ("future-phase", Some(now_ms), false),
        ("future-phase", Some(now_ms - 1), false),
        ("future-phase", Some(now_ms + 1), false),
    ];
    for (phase, lease_until_ms, expected) in cases {
        let view = MergeClaimView {
            worktree: "/worktrees/feature".into(),
            generation: 2,
            phase: phase.into(),
            lease_until_ms,
            owner_hint: None,
        };
        assert_eq!(
            expired_recoverable(&view, now_ms),
            expected,
            "{phase} {lease_until_ms:?}"
        );
    }
}
