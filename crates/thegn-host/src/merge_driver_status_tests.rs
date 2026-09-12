use super::*;

const WORKTREE: &str = "/private/driver-status";

fn config() -> MergeQueueConfig {
    MergeQueueConfig {
        target_branch: "main".into(),
        organize_folders: false,
        conflict_handoff: ConflictHandoff::Notify,
        ..Default::default()
    }
}

fn queued(db: &Db, attempts: u32) -> QueueItem {
    db.enqueue_merge(WORKTREE, "feature", "main").unwrap();
    db.set_merge_agent_attempts(WORKTREE, attempts).unwrap();
    db.update_merge_status(
        WORKTREE,
        "deferred",
        Some("old-result"),
        Some("old-conflict"),
        Some("old-error"),
    )
    .unwrap();
    QueueItem {
        worktree: WORKTREE.into(),
        branch: "feature".into(),
        location: String::new(),
        agent_attempts: attempts,
    }
}

fn assert_fields(row: &thegn_core::db::MergeQueueRow, fields: &MergeStatusFields) {
    assert_eq!(row.result_oid, fields.result_oid);
    assert_eq!(row.conflict_paths, fields.conflict_paths);
    assert_eq!(row.error_detail, fields.error_detail);
}

/// Exercises the real driver mapping, SQL write and UI projection. The only
/// injected operation is the fold result; no repository/provider is contacted.
fn one_outcome(
    attempt: anyhow::Result<AttemptOutcome>,
    attempts: u32,
) -> (thegn_core::db::MergeQueueRow, DriveOutcome) {
    let cfg = config();
    let db = Db::open_memory().unwrap();
    let item = queued(&db, attempts);
    let before = db.list_merge_queue().unwrap().remove(0);
    let mut result = Some(attempt);
    let mut panel = crate::panel::PanelData::default();
    panel.merge_queue.push(before.clone());
    let mut steps = Vec::new();
    let out = drive_queue_with(
        &cfg,
        &Config::default(),
        Path::new(WORKTREE),
        &db,
        vec![item],
        |step| {
            let saved = db.list_merge_queue().unwrap().remove(0);
            assert_eq!(saved.status, step.status);
            assert_fields(&saved, step.fields);
            crate::handlers::merge_queue::apply_step(
                &mut panel,
                step.worktree,
                step.branch,
                step.status,
                step.fields,
            );
            assert_eq!(panel.merge_queue[0].status, saved.status);
            assert_fields(&panel.merge_queue[0], step.fields);
            if step.status == "folding" {
                assert_eq!(step.fields, &MergeStatusFields::default());
            }
            steps.push(step.status.to_owned());
        },
        DriveActions {
            attempt: |_branch: &str, _location: &thegn_core::remote::GitLoc| {
                result.take().expect("one attempt only")
            },
            floor: |_worktree: &str| panic!("notify-only never attempts agent admission"),
        },
    );
    let saved = db.list_merge_queue().unwrap().remove(0);
    assert_eq!(steps.len(), 2);
    assert_eq!(steps[0], "folding");
    assert_eq!(saved.queued_at, before.queued_at);
    assert_eq!(saved.agent_attempts, before.agent_attempts);
    assert_eq!(saved.branch, before.branch);
    assert_eq!(saved.location, before.location);
    assert_eq!(saved.target_branch, before.target_branch);
    (saved, out)
}

#[test]
fn actual_conflict_paths_are_separate_from_submodule_context() {
    for attempts in [0, 2] {
        let (saved, out) = one_outcome(
            Ok(AttemptOutcome::Conflict {
                paths: vec!["src/a.rs".into(), "vendor/lib".into()],
                submodule_conflicts: vec![thegn_core::submodule::SubmoduleConflict {
                    path: "vendor/lib".into(),
                    ours_sha: "abc123".into(),
                    theirs_sha: "def456".into(),
                }],
            }),
            attempts,
        );
        assert_eq!(
            saved.status,
            if attempts == 0 {
                "deferred"
            } else {
                "needs_human"
            }
        );
        assert_eq!(
            saved.conflict_paths.as_deref(),
            Some("src/a.rs\nvendor/lib")
        );
        assert_eq!(
            saved.error_detail.as_deref(),
            Some("submodule pointer conflict: vendor/lib (abc123 vs def456)")
        );
        assert!(saved.result_oid.is_none());
        assert_eq!(out.deferred.len(), usize::from(attempts == 0));
        assert_eq!(out.needs_human.len(), usize::from(attempts != 0));
    }
}

#[test]
fn gate_and_unreachable_errors_never_become_conflict_paths() {
    let cases = [
        (
            Ok(AttemptOutcome::GateError {
                reason: "tool unavailable".into(),
                log: "gate log".into(),
            }),
            "gate_error",
            "tool unavailable",
        ),
        (
            Ok(AttemptOutcome::Unreachable {
                detail: "source unavailable".into(),
            }),
            "deferred",
            "source unavailable",
        ),
        (
            Ok(AttemptOutcome::GateFailed {
                log: "compile error".into(),
            }),
            "gate_failed",
            "compile error",
        ),
        (
            Err(anyhow::anyhow!("attempt failed")),
            "needs_human",
            "attempt failed",
        ),
    ];
    for (attempt, status, expected) in cases {
        let (saved, out) = one_outcome(attempt, 0);
        assert_eq!(saved.status, status);
        assert!(saved.result_oid.is_none() && saved.conflict_paths.is_none());
        assert!(saved.error_detail.as_deref().unwrap().contains(expected));
        if status == "gate_error" {
            assert_eq!(out.gate_error, ["feature"]);
            assert!(out.deferred.is_empty() && out.needs_human.is_empty());
        }
    }
}

#[test]
fn success_projects_full_oid_and_does_not_fabricate_oid_from_progress_text() {
    let oid = "1234567890123456789012345678901234567890";
    let (landed, _) = one_outcome(
        Ok(AttemptOutcome::Landed {
            commit: oid.into(),
            resyncs: Vec::new(),
        }),
        0,
    );
    assert_eq!(landed.result_oid.as_deref(), Some(oid));
    assert!(landed.conflict_paths.is_none() && landed.error_detail.is_none());
    let (ready, _) = one_outcome(Ok(AttemptOutcome::Ready { tip: oid.into() }), 0);
    assert_eq!(ready.result_oid.as_deref(), Some(oid));
    assert_eq!(
        ready.error_detail.as_deref(),
        Some("gated green — awaiting land")
    );
    assert!(ready.conflict_paths.is_none());
    let (already, _) = one_outcome(Ok(AttemptOutcome::UpToDate), 0);
    assert!(already.result_oid.is_none() && already.conflict_paths.is_none());
    assert_eq!(already.error_detail.as_deref(), Some("already merged"));
}

#[test]
fn infrastructure_hold_stops_after_one_attempt_and_admission_without_spending_budget() {
    let mut cfg = config();
    cfg.conflict_handoff = ConflictHandoff::Agent;
    cfg.agent_max_attempts = 3;
    // Never executable: a regression reaching agent_running panics before run_agent.
    cfg.agent_command = "private-test-agent-must-never-execute".into();
    let db = Db::open_memory().unwrap();
    let item = queued(&db, 1);
    let mut attempts = 0;
    let mut admissions = 0;
    let mut steps = Vec::new();
    let out = drive_queue_with(
        &cfg,
        &Config::default(),
        Path::new(WORKTREE),
        &db,
        vec![item],
        |step| {
            assert_ne!(step.status, "agent_running", "no agent dispatch after hold");
            steps.push(step.status.to_owned());
            let saved = db.list_merge_queue().unwrap().remove(0);
            assert_fields(&saved, step.fields);
        },
        DriveActions {
            attempt: |_branch: &str, _location: &thegn_core::remote::GitLoc| {
                attempts += 1;
                assert_eq!(attempts, 1, "hold must not retry the fold");
                Ok(AttemptOutcome::Conflict {
                    paths: vec!["a.rs".into()],
                    submodule_conflicts: vec![],
                })
            },
            floor: |_worktree: &str| {
                admissions += 1;
                assert_eq!(admissions, 1);
                crate::agent_run::AgentDispatch::InfraHold("private floor unavailable".into())
            },
        },
    );
    assert_eq!((attempts, admissions), (1, 1));
    assert_eq!(steps, ["folding", "agent_blocked"]);
    assert_eq!(out.deferred, ["feature"]);
    assert!(
        out.landed.is_empty()
            && out.ready.is_empty()
            && out.needs_human.is_empty()
            && out.gate_error.is_empty()
    );
    let saved = db.list_merge_queue().unwrap().remove(0);
    assert_eq!(saved.status, "agent_blocked");
    assert_eq!(saved.agent_attempts, 1);
    assert!(saved.result_oid.is_none() && saved.conflict_paths.is_none());
    assert_eq!(
        saved.error_detail.as_deref(),
        Some("private floor unavailable")
    );
}
