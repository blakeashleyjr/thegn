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
            runner: unexpected_runner,
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
            runner: unexpected_runner,
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

fn unexpected_runner(
    _: &str,
    _: &str,
    _: &str,
    _: &str,
    _: &Failure,
    _: Option<thegn_core::sandbox::SandboxSpec>,
) -> bool {
    panic!("fixture must not dispatch an agent")
}

const READY_OID: &str = "1234567890123456789012345678901234567890";
const HOLD_REASON: &str = "private guest-kernel floor unavailable";
const INERT_COMMAND: &str = "private-in-process-agent-boundary";

/// File-backed SQL is observed through a second connection. The trigger records
/// every status UPDATE, including repeated identical values (the original bug).
/// No worker or subprocess is needed: the root runs exact tests under a watchdog.
struct AuditFixture {
    db: Db,
    audit: rusqlite::Connection,
    root: tempfile::TempDir,
}

impl AuditFixture {
    fn new(branches: &[(&str, u32)]) -> Self {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("queue.sqlite3");
        let db = Db::open_at(&path).unwrap();
        for (branch, attempts) in branches {
            let worktree = root.path().join(branch);
            std::fs::create_dir(&worktree).unwrap();
            let worktree = worktree.to_str().unwrap();
            db.enqueue_merge(worktree, branch, "main").unwrap();
            db.set_merge_agent_attempts(worktree, *attempts).unwrap();
            db.update_merge_status(
                worktree,
                "deferred",
                Some("stale-oid"),
                Some("stale-path"),
                Some("stale-diagnostic"),
            )
            .unwrap();
        }
        let audit = rusqlite::Connection::open(&path).unwrap();
        audit.execute_batch(
            "CREATE TABLE status_audit (
                seq INTEGER PRIMARY KEY, worktree TEXT,
                old_status TEXT, old_oid TEXT, old_paths TEXT, old_error TEXT,
                new_status TEXT, new_oid TEXT, new_paths TEXT, new_error TEXT,
                old_attempts INTEGER, new_attempts INTEGER);
             CREATE TRIGGER audit_every_status_update AFTER UPDATE OF status ON merge_queue
             BEGIN
                INSERT INTO status_audit VALUES (NULL, NEW.worktree,
                  OLD.status, OLD.result_oid, OLD.conflict_paths, OLD.error_detail,
                  NEW.status, NEW.result_oid, NEW.conflict_paths, NEW.error_detail,
                  OLD.agent_attempts, NEW.agent_attempts);
             END;
             CREATE TABLE budget_audit (seq INTEGER PRIMARY KEY, worktree TEXT, old_attempts INTEGER, new_attempts INTEGER);
             CREATE TRIGGER audit_every_budget_update AFTER UPDATE OF agent_attempts ON merge_queue
             BEGIN
                INSERT INTO budget_audit VALUES (NULL, NEW.worktree, OLD.agent_attempts, NEW.agent_attempts);
             END;",
        ).unwrap();
        Self { db, audit, root }
    }

    fn row(&self, branch: &str) -> thegn_core::db::MergeQueueRow {
        self.db
            .list_merge_queue()
            .unwrap()
            .into_iter()
            .find(|row| row.branch == branch)
            .unwrap()
    }

    fn item(&self, branch: &str) -> QueueItem {
        let row = self.row(branch);
        QueueItem {
            worktree: row.worktree,
            branch: row.branch,
            location: row.location,
            agent_attempts: row.agent_attempts,
        }
    }

    fn clear_audit(&self) {
        self.audit
            .execute_batch("DELETE FROM status_audit; DELETE FROM budget_audit;")
            .unwrap();
    }

    fn progress(
        &self,
        step: &DriveStep,
        panel: &mut crate::panel::PanelData,
        observed: &mut Vec<thegn_core::db::MergeQueueRow>,
    ) {
        let saved = self.row(step.branch);
        assert_eq!(saved.worktree, step.worktree);
        assert_eq!(saved.status, step.status);
        assert_fields(&saved, step.fields);
        if step.status == "folding" {
            assert_eq!(step.fields, &MergeStatusFields::default());
        } else if step.status == "agent_running" {
            assert_eq!(
                step.fields,
                &MergeStatusFields {
                    result_oid: None,
                    conflict_paths: None,
                    error_detail: Some(format!("agent fixing ({}/3)", saved.agent_attempts)),
                }
            );
        }
        crate::handlers::merge_queue::apply_step(
            panel,
            step.worktree,
            step.branch,
            step.status,
            step.fields,
        );
        let projected = panel
            .merge_queue
            .iter()
            .find(|row| row.worktree == step.worktree)
            .unwrap();
        assert_eq!(projected.status, saved.status);
        assert_fields(projected, step.fields);
        observed.push(saved);
    }

    fn assert_audit(
        &self,
        before: &[thegn_core::db::MergeQueueRow],
        observed: &[thegn_core::db::MergeQueueRow],
        expected: &[(&str, &str)],
        budget: &[(&str, u32, u32)],
    ) {
        assert_eq!(
            observed
                .iter()
                .map(|r| (r.branch.as_str(), r.status.as_str()))
                .collect::<Vec<_>>(),
            expected
        );
        let mut previous = before
            .iter()
            .map(|row| (row.worktree.clone(), row.clone()))
            .collect::<std::collections::HashMap<_, _>>();
        let mut stmt = self.audit.prepare("SELECT seq,worktree,old_status,old_oid,old_paths,old_error,new_status,new_oid,new_paths,new_error,old_attempts,new_attempts FROM status_audit ORDER BY seq").unwrap();
        let mut sql = stmt.query([]).unwrap();
        for (index, saved) in observed.iter().enumerate() {
            let row = sql
                .next()
                .unwrap()
                .expect("each progress follows a real SQL update");
            assert_eq!(
                row.get::<_, i64>(0).unwrap(),
                i64::try_from(index + 1).unwrap()
            );
            assert_eq!(row.get::<_, String>(1).unwrap(), saved.worktree);
            let old = &previous[&saved.worktree];
            assert_eq!(row.get::<_, String>(2).unwrap(), old.status);
            assert_eq!(row.get::<_, Option<String>>(3).unwrap(), old.result_oid);
            assert_eq!(row.get::<_, Option<String>>(4).unwrap(), old.conflict_paths);
            assert_eq!(row.get::<_, Option<String>>(5).unwrap(), old.error_detail);
            assert_eq!(row.get::<_, String>(6).unwrap(), saved.status);
            assert_eq!(row.get::<_, Option<String>>(7).unwrap(), saved.result_oid);
            assert_eq!(
                row.get::<_, Option<String>>(8).unwrap(),
                saved.conflict_paths
            );
            assert_eq!(row.get::<_, Option<String>>(9).unwrap(), saved.error_detail);
            // Budget writes precede agent_running; the status update itself
            // must not consume a second attempt or mutate budget again.
            assert_eq!(row.get::<_, u32>(10).unwrap(), saved.agent_attempts);
            assert_eq!(row.get::<_, u32>(11).unwrap(), saved.agent_attempts);
            let mut metadata = saved.clone();
            metadata.status.clone_from(&old.status);
            metadata.result_oid.clone_from(&old.result_oid);
            metadata.conflict_paths.clone_from(&old.conflict_paths);
            metadata.error_detail.clone_from(&old.error_detail);
            metadata.updated_at = old.updated_at;
            metadata.agent_attempts = old.agent_attempts;
            assert_eq!(metadata, *old, "unrelated queue metadata must survive");
            previous.insert(saved.worktree.clone(), saved.clone());
        }
        assert!(
            sql.next().unwrap().is_none(),
            "extra status UPDATE, even if no progress was emitted"
        );
        let mut stmt = self
            .audit
            .prepare("SELECT worktree,old_attempts,new_attempts FROM budget_audit ORDER BY seq")
            .unwrap();
        let actual = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, u32>(1)?,
                    row.get::<_, u32>(2)?,
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        let expected = budget
            .iter()
            .map(|(branch, old, new)| (self.row(branch).worktree, *old, *new))
            .collect::<Vec<_>>();
        assert_eq!(actual, expected);
    }

    fn close(self) {
        let Self { db, audit, root } = self;
        drop(db);
        audit.close().unwrap();
        root.close().expect("owned SQLite fixture cleanup");
    }
}

#[derive(Clone, Copy, Debug)]
enum RetryFailure {
    Conflict,
    RedGate,
}

impl RetryFailure {
    fn attempt(self) -> AttemptOutcome {
        match self {
            Self::Conflict => AttemptOutcome::Conflict {
                paths: vec!["src/conflict.rs".into(), "second.txt".into()],
                submodule_conflicts: vec![],
            },
            Self::RedGate => AttemptOutcome::GateFailed {
                log: "private-red-gate: compilation failed".into(),
            },
        }
    }

    fn assert_failure(self, failure: &Failure) {
        match (self, failure) {
            (
                Self::Conflict,
                Failure::Conflict {
                    paths,
                    submodule_conflicts,
                },
            ) => {
                assert_eq!(paths, &["src/conflict.rs", "second.txt"]);
                assert!(submodule_conflicts.is_empty());
            }
            (Self::RedGate, Failure::Gate(log)) => {
                assert_eq!(log, "private-red-gate: compilation failed")
            }
            _ => panic!("runner must receive original borrowed failure"),
        }
    }
}

fn agent_config() -> MergeQueueConfig {
    MergeQueueConfig {
        conflict_handoff: ConflictHandoff::Agent,
        agent_max_attempts: 3,
        agent_command: INERT_COMMAND.into(),
        ..config()
    }
}

fn assert_only(
    out: &DriveOutcome,
    ready: &[&str],
    deferred: &[&str],
    human: &[&str],
    gate_error: &[&str],
) {
    assert_eq!(out.ready, ready);
    assert_eq!(out.deferred, deferred);
    assert_eq!(out.needs_human, human);
    assert_eq!(out.gate_error, gate_error);
    assert!(out.landed.is_empty() && out.resyncs.is_empty() && out.warnings.is_empty());
}

fn hold_then_recover(failure: RetryFailure) {
    let started = std::time::Instant::now();
    let fixture = AuditFixture::new(&[("held", 1), ("next", 0)]);
    let cfg = agent_config();
    let full = Config::default();
    let before = fixture.db.list_merge_queue().unwrap();
    let held = fixture.item("held");
    let mut panel = crate::panel::PanelData {
        merge_queue: before.clone(),
        ..Default::default()
    };
    let mut observed = Vec::new();
    let (mut held_attempts, mut next_attempts, mut floors) = (0, 0, 0);
    let out = drive_queue_with(
        &cfg,
        &full,
        fixture.root.path(),
        &fixture.db,
        vec![held.clone(), fixture.item("next")],
        |step| fixture.progress(step, &mut panel, &mut observed),
        DriveActions {
            attempt: |branch: &str, _location: &thegn_core::remote::GitLoc| {
                if branch == "held" {
                    held_attempts += 1;
                    assert_eq!(held_attempts, 1, "hold must not retry the fold");
                    Ok(failure.attempt())
                } else {
                    assert_eq!(branch, "next");
                    next_attempts += 1;
                    assert_eq!(next_attempts, 1);
                    Ok(AttemptOutcome::Ready {
                        tip: READY_OID.into(),
                    })
                }
            },
            floor: |worktree: &str| {
                assert_eq!(worktree, held.worktree);
                floors += 1;
                assert_eq!(floors, 1);
                crate::agent_run::AgentDispatch::InfraHold(HOLD_REASON.into())
            },
            runner: unexpected_runner,
        },
    );
    assert_eq!((held_attempts, next_attempts, floors), (1, 1, 1));
    assert_only(&out, &["next"], &["held"], &[], &[]);
    fixture.assert_audit(
        &before,
        &observed,
        &[
            ("held", "folding"),
            ("held", "agent_blocked"),
            ("next", "folding"),
            ("next", "ready"),
        ],
        &[],
    );
    let saved = fixture.row("held");
    assert_eq!(saved.agent_attempts, 1);
    assert_eq!(saved.error_detail.as_deref(), Some(HOLD_REASON));
    assert!(saved.result_oid.is_none() && saved.conflict_paths.is_none());
    assert_eq!(fixture.row("next").result_oid.as_deref(), Some(READY_OID));

    // New independent drive uses the persisted held identity/budget. Floor
    // recovery must actually enter the inert runner before a subsequent Ready.
    fixture.clear_audit();
    observed.clear();
    let before = fixture.db.list_merge_queue().unwrap();
    let item = fixture.item("held");
    assert_eq!(item.agent_attempts, 1);
    let repaired = std::cell::Cell::new(false);
    let (mut attempts, mut floors, mut runs) = (0, 0, 0);
    let out = drive_queue_with(
        &cfg,
        &full,
        fixture.root.path(),
        &fixture.db,
        vec![item.clone()],
        |step| fixture.progress(step, &mut panel, &mut observed),
        DriveActions {
            attempt: |branch: &str, _location: &thegn_core::remote::GitLoc| {
                assert_eq!(branch, "held");
                attempts += 1;
                assert!(attempts <= 2, "recovery fold bound");
                assert_eq!(repaired.get(), attempts == 2);
                Ok(if repaired.get() {
                    AttemptOutcome::Ready {
                        tip: READY_OID.into(),
                    }
                } else {
                    failure.attempt()
                })
            },
            floor: |worktree: &str| {
                assert_eq!(worktree, item.worktree);
                floors += 1;
                assert_eq!(floors, 1);
                crate::agent_run::AgentDispatch::Run(None)
            },
            runner: |template: &str,
                     worktree: &str,
                     branch: &str,
                     target: &str,
                     cause: &Failure,
                     sandbox: Option<thegn_core::sandbox::SandboxSpec>| {
                assert_eq!(
                    (template, worktree, branch, target),
                    (INERT_COMMAND, item.worktree.as_str(), "held", "main")
                );
                assert!(sandbox.is_none());
                failure.assert_failure(cause);
                runs += 1;
                assert_eq!(runs, 1);
                assert_eq!(fixture.row("held").agent_attempts, 2);
                assert_eq!(fixture.row("held").status, "agent_running");
                assert!(!repaired.replace(true));
                true
            },
        },
    );
    assert_eq!((attempts, floors, runs), (2, 1, 1));
    assert_only(&out, &["held"], &[], &[], &[]);
    fixture.assert_audit(
        &before,
        &observed,
        &[
            ("held", "folding"),
            ("held", "agent_running"),
            ("held", "ready"),
        ],
        &[("held", 1, 2)],
    );
    let saved = fixture.row("held");
    assert_eq!(saved.agent_attempts, 2);
    assert_eq!(saved.result_oid.as_deref(), Some(READY_OID));
    assert!(saved.conflict_paths.is_none());
    assert_eq!(
        saved.error_detail.as_deref(),
        Some("gated green — awaiting land")
    );
    assert_eq!(
        fixture.row("next"),
        before.into_iter().find(|row| row.branch == "next").unwrap()
    );
    fixture.close();
    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "finite in-process drive fixture deadline"
    );
}

#[test]
fn conflict_hold_continues_queue_and_independent_floor_recovery_spends_one_attempt() {
    hold_then_recover(RetryFailure::Conflict);
}

#[test]
fn red_gate_hold_continues_queue_and_independent_floor_recovery_spends_one_attempt() {
    hold_then_recover(RetryFailure::RedGate);
}

#[test]
fn unchanged_agent_result_exhausts_only_remaining_budget_for_conflict_and_red_gate() {
    for failure in [RetryFailure::Conflict, RetryFailure::RedGate] {
        let started = std::time::Instant::now();
        let fixture = AuditFixture::new(&[("budget", 1)]);
        let cfg = agent_config();
        let before = fixture.db.list_merge_queue().unwrap();
        let item = fixture.item("budget");
        let mut panel = crate::panel::PanelData {
            merge_queue: before.clone(),
            ..Default::default()
        };
        let mut observed = Vec::new();
        let (mut attempts, mut floors, mut runs) = (0, 0, 0);
        let out = drive_queue_with(
            &cfg,
            &Config::default(),
            fixture.root.path(),
            &fixture.db,
            vec![item.clone()],
            |step| fixture.progress(step, &mut panel, &mut observed),
            DriveActions {
                attempt: |branch: &str, _location: &thegn_core::remote::GitLoc| {
                    assert_eq!(branch, "budget");
                    attempts += 1;
                    assert!(attempts <= 3, "finite remaining-budget fold bound");
                    Ok(failure.attempt())
                },
                floor: |worktree: &str| {
                    assert_eq!(worktree, item.worktree);
                    floors += 1;
                    assert!(floors <= 2, "no floor admission after budget exhausted");
                    crate::agent_run::AgentDispatch::Run(None)
                },
                runner:
                    |template: &str,
                     worktree: &str,
                     branch: &str,
                     target: &str,
                     cause: &Failure,
                     sandbox: Option<thegn_core::sandbox::SandboxSpec>| {
                        assert_eq!(
                            (template, worktree, branch, target),
                            (INERT_COMMAND, item.worktree.as_str(), "budget", "main")
                        );
                        assert!(sandbox.is_none());
                        failure.assert_failure(cause);
                        runs += 1;
                        assert!(runs <= 2, "no agent after remaining budget");
                        assert_eq!(fixture.row("budget").agent_attempts, 1 + runs);
                        false // advisory failure must still consume the admitted attempt
                    },
            },
        );
        assert_eq!((attempts, floors, runs), (3, 2, 2));
        assert_only(&out, &[], &[], &["budget"], &[]);
        fixture.assert_audit(
            &before,
            &observed,
            &[
                ("budget", "folding"),
                ("budget", "agent_running"),
                ("budget", "agent_running"),
                ("budget", "needs_human"),
            ],
            &[("budget", 1, 2), ("budget", 2, 3)],
        );
        let saved = fixture.row("budget");
        assert_eq!(saved.agent_attempts, 3);
        assert!(saved.result_oid.is_none());
        match failure {
            RetryFailure::Conflict => {
                assert_eq!(
                    saved.conflict_paths.as_deref(),
                    Some("src/conflict.rs\nsecond.txt")
                );
                assert!(saved.error_detail.is_none());
            }
            RetryFailure::RedGate => {
                assert!(saved.conflict_paths.is_none());
                assert_eq!(
                    saved.error_detail.as_deref(),
                    Some("breaks build\nprivate-red-gate: compilation failed")
                );
            }
        }
        fixture.close();
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
    }
}

#[test]
fn gate_error_never_admits_agent_and_next_item_still_reaches_ready() {
    let started = std::time::Instant::now();
    let fixture = AuditFixture::new(&[("error", 1), ("next", 0)]);
    let cfg = agent_config();
    let before = fixture.db.list_merge_queue().unwrap();
    let mut panel = crate::panel::PanelData {
        merge_queue: before.clone(),
        ..Default::default()
    };
    let mut observed = Vec::new();
    let mut attempts = 0;
    let out = drive_queue_with(
        &cfg,
        &Config::default(),
        fixture.root.path(),
        &fixture.db,
        vec![fixture.item("error"), fixture.item("next")],
        |step| fixture.progress(step, &mut panel, &mut observed),
        DriveActions {
            attempt: |branch: &str, _location: &thegn_core::remote::GitLoc| {
                attempts += 1;
                assert!(attempts <= 2);
                assert_eq!(branch, if attempts == 1 { "error" } else { "next" });
                Ok(if attempts == 1 {
                    AttemptOutcome::GateError {
                        reason: "private setup exit 23".into(),
                        log: "private setup diagnostic".into(),
                    }
                } else {
                    AttemptOutcome::Ready {
                        tip: READY_OID.into(),
                    }
                })
            },
            floor: |_worktree: &str| panic!("GateError must not attempt floor admission"),
            runner: unexpected_runner,
        },
    );
    assert_eq!(attempts, 2);
    assert_only(&out, &["next"], &[], &[], &["error"]);
    fixture.assert_audit(
        &before,
        &observed,
        &[
            ("error", "folding"),
            ("error", "gate_error"),
            ("next", "folding"),
            ("next", "ready"),
        ],
        &[],
    );
    let saved = fixture.row("error");
    assert_eq!(saved.agent_attempts, 1);
    assert!(saved.result_oid.is_none() && saved.conflict_paths.is_none());
    assert_eq!(
        saved.error_detail.as_deref(),
        Some("private setup exit 23\nprivate setup diagnostic")
    );
    assert_eq!(fixture.row("next").result_oid.as_deref(), Some(READY_OID));
    fixture.close();
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
}
