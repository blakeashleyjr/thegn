//! Periodic roster self-heal — the daemon half of `dispatch reap`.
//!
//! The failure this exists to end: a stage worker does its work, commits its
//! artifact, and dies before calling `dispatch report`. `pipeline_reap`
//! classifies that correctly, but until now its only caller was the CLI verb
//! (`cmd::dispatch::reap`), so a row stayed `running` — indistinguishable from
//! a live worker — until a human happened to type the command. Across the
//! 469-row pipeline run that produced 40 rows recording no reason at all, and
//! it is the mechanism behind the 121-unclosable-row incident.
//!
//! # What this task may and may not write
//!
//! `pipeline_retry`'s header states the rule the daemon has always followed:
//! *the daemon can park a row but never finish one*. This task **extends that
//! rule in exactly one direction**, and it is worth being precise about why.
//!
//! - **`CloseDone` → `done`.** Allowed. This verdict fires only when the
//!   artifact exists, is committed and unchanged in `HEAD`, and a report with
//!   valid PASS gate evidence is filed — bit for bit
//!   the gate `dispatch set-status done` already enforces on the supervisor's
//!   behalf. Applying it is arithmetic on recorded facts, not a judgement about
//!   whether the work was any good, so there is nothing here for a human to
//!   decide differently. Refusing to apply it would not protect anything; it
//!   would only mean the row keeps lying about being live.
//! - **`MarkFailed` → `waiting_human` + a note.** Parked, never failed. The
//!   CLI writes `failed` here because a supervisor is present and owns the
//!   verdict; the daemon has no such standing, and "the worker vanished leaving
//!   nothing" is exactly the case a human should look at.
//! - **`NeedsDecision` → untouched.** Explicitly a human's call
//!   (`thegn_core::pipeline_reap`'s module doc).
//! - **`Live` / `Closed` → untouched.**
//!
//! # Cadence
//!
//! A slow timer, deliberately unlike [`super::HEARTBEAT_SECS`]: each pass reads
//! every active row's worktree and shells out to `git` twice per row, so it
//! runs on its own long interval and does that work inside `spawn_blocking` —
//! never on the runtime, and never on the compositor's loop (this is the
//! daemon process; the 0%-idle contract still applies to the host).

use std::sync::Arc;
use std::time::Duration;

use thegn_core::issue::AgentDispatchStatus;
use thegn_core::pipeline_reap::ReapVerdict;
use thegn_core::store::NotificationStore as _;

use super::service::DaemonService;

/// How often to reconcile. Long on purpose: the work is per-row git I/O, and
/// nothing here is latency-sensitive — a stale row costs a supervisor's
/// attention, not a user's frame.
const REAP_INTERVAL_SECS: u64 = 300;

/// Wait this long after daemon start before the first pass, so a restart that
/// is about to re-adopt live sessions does not read them as absent and park
/// rows that are seconds away from checking back in.
const REAP_FIRST_DELAY_SECS: u64 = 60;

pub(crate) fn spawn(svc: Arc<DaemonService>) {
    tokio::spawn(reap_loop(svc));
}

async fn reap_loop(svc: Arc<DaemonService>) {
    tokio::time::sleep(Duration::from_secs(REAP_FIRST_DELAY_SECS)).await;
    let mut tick = tokio::time::interval(Duration::from_secs(REAP_INTERVAL_SECS));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        // The daemon's own session map is the liveness source: it holds live
        // entries only (an exited session becomes a `Lookup::Dead` tombstone),
        // so this needs no control round-trip to ourselves.
        let live_ids: Vec<String> = { svc.sessions.lock().await.keys().cloned().collect() };
        let db = svc.db.clone();
        let fresh = svc.clone();
        // best-effort: a failed pass is retried on the next tick; a reap that
        // cannot read git must never take the daemon down.
        let _ = tokio::task::spawn_blocking(move || {
            // THE-267: re-read liveness immediately before each mutation. The
            // plan's slow git/fs probes widen the window in which a worker can
            // open; `blocking_lock` is legal here (blocking pool, not async).
            let fresh_live = move || -> Option<Vec<String>> {
                Some(fresh.sessions.blocking_lock().keys().cloned().collect())
            };
            reap_pass(&db, &live_ids, &fresh_live)
        })
        .await;
    }
}

/// One reconciliation pass. Separated from the timer so the policy is readable
/// (and so a future caller — `thegn doctor`, say — can run it directly).
///
/// `fresh_live` re-reads the daemon's live session ids right before a mutation
/// (`None` = liveness unavailable). Every transition is also fenced on the
/// (session, `run_gen`) captured with the row snapshot, so a plan built from a
/// stale observation is a no-op, never a worker-gone verdict (THE-267).
fn reap_pass(
    db: &super::service::SharedDb,
    live_ids: &[String],
    fresh_live: &dyn Fn() -> Option<Vec<String>>,
) {
    // Snapshot under the shared mutex, then release it before any filesystem
    // or git work. A large stale roster must not stop unrelated daemon DB
    // operations for the duration of hundreds of subprocesses.
    let (rows, runs) = match db.lock() {
        Ok(db) => match db.list_dispatches() {
            Ok(rows) => {
                let rows = rows
                    .into_iter()
                    .filter(|r| {
                        matches!(
                            r.status,
                            AgentDispatchStatus::Spawning | AgentDispatchStatus::Running
                        )
                    })
                    .collect::<Vec<_>>();
                // Same lock as the roster snapshot: the generation each plan
                // entry is fenced on. A row we cannot fence is not planned.
                let runs = rows
                    .iter()
                    .filter_map(|r| db.dispatch_run_ref(r.id).ok().flatten())
                    .collect::<Vec<_>>();
                (rows, runs)
            }
            Err(e) => {
                tracing::debug!(target: "thegn::pipeline", error = %e, "reap pass could not snapshot");
                return;
            }
        },
        Err(_) => return,
    };
    let plan = crate::cmd::dispatch::reap_plan_rows(&rows, Some(live_ids));
    for r in &plan {
        if !matches!(
            r.verdict,
            ReapVerdict::CloseDone | ReapVerdict::MarkFailed { .. }
        ) {
            continue;
        }
        let Some(run) = runs.iter().find(|x| x.id == r.id) else {
            continue;
        };
        // Exact-session liveness, fresh: a session that is live now (opened
        // after the snapshot) or an unreadable daemon is a no-op.
        match fresh_live() {
            Some(now) if !now.iter().any(|s| s == &run.session_id) => {}
            _ => {
                tracing::debug!(
                    target: "thegn::pipeline", row = r.id,
                    "reap skipped: session live or liveness unavailable at apply"
                );
                continue;
            }
        }
        let db = match db.lock() {
            Ok(db) => db,
            Err(_) => return,
        };
        match &r.verdict {
            ReapVerdict::CloseDone => {
                if matches!(
                    db.compare_and_set_dispatch_status_run(
                        run,
                        r.observed_status,
                        AgentDispatchStatus::Done,
                        None,
                    ),
                    Ok(true)
                ) {
                    tracing::info!(
                        target: "thegn::pipeline",
                        row = r.id,
                        "reaped: artifact committed and report filed — closing done"
                    );
                }
            }
            ReapVerdict::MarkFailed { why } => {
                // Park, do not fail. Status and reason are one atomic,
                // compare-and-set transition: a concurrent human decision wins.
                if matches!(
                    db.compare_and_set_dispatch_status_run(
                        run,
                        r.observed_status,
                        AgentDispatchStatus::WaitingHuman,
                        Some(&format!("reaped (daemon): {why}")),
                    ),
                    Ok(true)
                ) {
                    tracing::info!(
                        target: "thegn::pipeline",
                        row = r.id, why = %why,
                        "reaped: parked for a supervisor"
                    );
                }
            }
            // A human's call, or nothing to do.
            // `Unknown` belongs with the do-nothing arms, not the reaping
            // ones: it means liveness could not be established, and this
            // reaper's doctrine is that it may park a row but never finish one.
            // Acting on an unknown row is the guess the fail-closed change
            // exists to prevent.
            ReapVerdict::NeedsDecision { .. }
            | ReapVerdict::Unknown { .. }
            | ReapVerdict::Live
            | ReapVerdict::Closed => {}
        }
    }
}

// Cadence invariants, pinned at COMPILE time rather than in a `#[test]`: both
// operands are constants, so a runtime `assert!` on them is tautological — it
// can never observe a violation a compile could not (clippy's
// `assertions_on_constants` says exactly this). As `const` assertions they
// genuinely cannot be violated: lowering either value stops the build.
//
// A restart re-adopts its sessions asynchronously, so an immediate first pass
// would read live workers as absent and park rows that are seconds from
// checking back in. And each pass walks every active row's worktree and shells
// out to `git`, which is not something to do on a short timer.
const _: () = assert!(
    REAP_FIRST_DELAY_SECS >= 30,
    "first pass must not race a restart's session re-adoption"
);
const _: () = assert!(
    REAP_INTERVAL_SECS >= 60,
    "this pass does per-row git I/O; it must stay a slow timer"
);

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use thegn_core::db::Db;
    use thegn_core::issue::NewDispatch;
    use thegn_core::store::NotificationStore;

    #[expect(
        clippy::disallowed_methods,
        reason = "test fixture setup is synchronous and runs no production or event-loop code"
    )]
    fn git(dir: &std::path::Path, args: &[&str]) {
        let status = thegn_core::util::git_cmd(dir)
            // Test repositories must not inherit interactive signing from the
            // developer's global config. A signed commit would launch pinentry
            // in this headless test and make the suite depend on workstation
            // credentials rather than the fixture itself.
            .args(["-c", "commit.gpgsign=false", "-c", "tag.gpgsign=false"])
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    fn row(db: &Db, wt: &std::path::Path, artifact: &str) -> i64 {
        let id = db
            .put_agent_dispatch(NewDispatch {
                issue_id: "linear:THE-1",
                worktree_path: wt.to_str().unwrap(),
                agent_name: "worker",
                stage: Some("code"),
                parent_id: None,
                session_id: Some("dead-session"),
                artifact_path: Some(artifact),
                chunk_path: None,
            })
            .unwrap();
        db.update_dispatch_status(id, AgentDispatchStatus::Running)
            .unwrap();
        let run = db.dispatch_run_ref(id).unwrap().unwrap();
        db.stamp_dispatch_exit(&run, Some(0)).unwrap();
        id
    }

    #[test]
    fn production_pass_parks_once_and_does_not_grow_notes() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = Db::open_at(&dir.path().join("thegn.db")).unwrap();
        let wt = dir.path().join("wt");
        std::fs::create_dir(&wt).unwrap();
        let id = row(&db, &wt, ".thegn/pipeline/THE-1/code/1.md");
        let shared = Arc::new(Mutex::new(db));

        reap_pass(&shared, &[], &|| Some(Vec::new()));
        {
            let db = shared.lock().unwrap();
            assert_eq!(
                db.get_dispatch(id).unwrap().unwrap().status,
                AgentDispatchStatus::WaitingHuman
            );
            assert_eq!(db.dispatch_notes(id, None, 0).unwrap().len(), 1);
        }
        reap_pass(&shared, &[], &|| Some(Vec::new()));
        assert_eq!(
            shared
                .lock()
                .unwrap()
                .dispatch_notes(id, None, 0)
                .unwrap()
                .len(),
            1,
            "a parked row is not reaped again"
        );
    }

    #[test]
    fn production_pass_closes_only_a_clean_head_artifact_with_gated_pass() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = Db::open_at(&dir.path().join("thegn.db")).unwrap();
        let wt = dir.path().join("wt");
        std::fs::create_dir(&wt).unwrap();
        git(&wt, &["init", "-q"]);
        git(&wt, &["config", "user.email", "test@example.com"]);
        git(&wt, &["config", "user.name", "Test"]);
        let artifact = ".thegn/pipeline/THE-1/code/1.md";
        std::fs::create_dir_all(wt.join(".thegn/pipeline/THE-1/code")).unwrap();
        std::fs::write(wt.join(artifact), "handoff").unwrap();
        git(&wt, &["add", artifact]);
        git(&wt, &["commit", "-qm", "handoff"]);
        let id = row(&db, &wt, artifact);
        db.set_dispatch_report(id, "PASS\ngate: just test — exit 0")
            .unwrap();
        let shared = Arc::new(Mutex::new(db));

        reap_pass(&shared, &[], &|| Some(Vec::new()));
        assert_eq!(
            shared
                .lock()
                .unwrap()
                .get_dispatch(id)
                .unwrap()
                .unwrap()
                .status,
            AgentDispatchStatus::Done
        );
    }

    #[test]
    fn daemon_restart_without_exit_stamp_leaves_row_active_and_slot_occupying() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = Db::open_at(&dir.path().join("thegn.db")).unwrap();
        let wt = dir.path().join("wt");
        std::fs::create_dir(&wt).unwrap();
        let id = db
            .put_agent_dispatch(NewDispatch {
                issue_id: "linear:THE-209",
                worktree_path: wt.to_str().unwrap(),
                agent_name: "worker",
                stage: Some("code"),
                parent_id: None,
                session_id: Some("unobserved-session"),
                artifact_path: Some("handoff.md"),
                chunk_path: None,
            })
            .unwrap();
        db.update_dispatch_status(id, AgentDispatchStatus::Running)
            .unwrap();
        let shared = Arc::new(Mutex::new(db));

        // After restart, the session map can be empty while this row has no
        // durable exit stamp. The daemon and CLI both leave that ambiguous row.
        reap_pass(&shared, &[], &|| Some(Vec::new()));
        let db = shared.lock().unwrap();
        let row = db.get_dispatch(id).unwrap().unwrap();
        assert_eq!(row.status, AgentDispatchStatus::Running);
        assert!(row.exit_code.is_none() && row.exited_at_ms.is_none());
    }

    #[test]
    fn production_pass_still_recognizes_a_live_session_as_live() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = Db::open_at(&dir.path().join("thegn.db")).unwrap();
        let wt = dir.path().join("wt");
        std::fs::create_dir(&wt).unwrap();
        let id = db
            .put_agent_dispatch(NewDispatch {
                issue_id: "linear:THE-209",
                worktree_path: wt.to_str().unwrap(),
                agent_name: "worker",
                stage: Some("code"),
                parent_id: None,
                session_id: Some("live-session"),
                artifact_path: Some("handoff.md"),
                chunk_path: None,
            })
            .unwrap();
        db.update_dispatch_status(id, AgentDispatchStatus::Running)
            .unwrap();
        let shared = Arc::new(Mutex::new(db));

        reap_pass(&shared, &["live-session".to_string()], &|| {
            Some(vec!["live-session".to_string()])
        });
        assert_eq!(
            shared
                .lock()
                .unwrap()
                .get_dispatch(id)
                .unwrap()
                .unwrap()
                .status,
            AgentDispatchStatus::Running
        );
    }

    #[test]
    fn session_opened_after_the_live_snapshot_is_not_parked() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = Db::open_at(&dir.path().join("thegn.db")).unwrap();
        let wt = dir.path().join("wt");
        std::fs::create_dir(&wt).unwrap();
        let id = row(&db, &wt, ".thegn/pipeline/THE-1/code/1.md");
        let shared = Arc::new(Mutex::new(db));
        // Stale snapshot lacks the session; the apply-time re-read has it.
        reap_pass(&shared, &[], &|| Some(vec!["dead-session".to_string()]));
        assert_eq!(status_of(&shared, id), AgentDispatchStatus::Running);
    }

    #[test]
    fn unavailable_liveness_at_apply_is_a_noop() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = Db::open_at(&dir.path().join("thegn.db")).unwrap();
        let wt = dir.path().join("wt");
        std::fs::create_dir(&wt).unwrap();
        let id = row(&db, &wt, ".thegn/pipeline/THE-1/code/1.md");
        let shared = Arc::new(Mutex::new(db));
        reap_pass(&shared, &[], &|| None);
        assert_eq!(status_of(&shared, id), AgentDispatchStatus::Running);
    }

    #[test]
    fn relaunch_between_plan_and_apply_is_not_parked() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = Db::open_at(&dir.path().join("thegn.db")).unwrap();
        let wt = dir.path().join("wt");
        std::fs::create_dir(&wt).unwrap();
        let id = row(&db, &wt, ".thegn/pipeline/THE-1/code/1.md");
        let shared = Arc::new(Mutex::new(db));
        // The retry publishes the SAME session id (bumping run_gen) after the
        // plan was built and after the fresh liveness read said "absent".
        let relaunch = || {
            shared
                .lock()
                .unwrap()
                .stamp_dispatch_run(id, "dead-session", ".thegn/pipeline/THE-1/code/1.md")
                .unwrap();
            Some(Vec::new())
        };
        reap_pass(&shared, &[], &relaunch);
        assert_eq!(status_of(&shared, id), AgentDispatchStatus::Running);
    }

    fn status_of(shared: &Arc<Mutex<Db>>, id: i64) -> AgentDispatchStatus {
        shared
            .lock()
            .unwrap()
            .get_dispatch(id)
            .unwrap()
            .unwrap()
            .status
    }
}
