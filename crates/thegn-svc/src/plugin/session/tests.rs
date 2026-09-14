pub(crate) struct FixtureSupervisor {
    pub(super) supervisor: super::ResidentSupervisor,
    runtime: tokio::runtime::Runtime,
}
impl FixtureSupervisor {
    pub(crate) fn new() -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let supervisor = super::ResidentSupervisor::new(runtime.handle().clone());
        Self {
            supervisor,
            runtime,
        }
    }
    pub(crate) fn spawn(
        &self,
        argv: &[String],
        env: &std::collections::BTreeMap<String, String>,
        cwd: Option<&std::path::Path>,
        callback: impl Fn(super::SessionEvent) + Send + Sync + 'static,
    ) -> Result<super::ResidentSession, crate::plugin::PluginError> {
        super::ResidentSession::spawn(&self.supervisor, "fixture", argv, env, cwd, callback)
    }
    pub(crate) fn shutdown(&self) -> super::ShutdownReport {
        self.runtime.block_on(
            self.supervisor
                .shutdown_until(tokio::time::Instant::now() + std::time::Duration::from_secs(3)),
        )
    }
}
impl Drop for FixtureSupervisor {
    fn drop(&mut self) {
        assert!(self.shutdown().is_settled(), "fixture left owned resources");
    }
}
use super::*;
use std::sync::mpsc;
use std::time::Duration;
use thegn_core::plugin_api::PluginCallback;

fn sh(script: &str) -> Vec<String> {
    vec!["sh".into(), "-c".into(), script.into()]
}

fn collect(rx: &mpsc::Receiver<SessionEvent>) -> Vec<SessionEvent> {
    let mut out = Vec::new();
    while let Ok(ev) = rx.recv_timeout(Duration::from_secs(10)) {
        let done = matches!(ev, SessionEvent::Exit { .. });
        out.push(ev);
        if done {
            break;
        }
    }
    out
}

#[test]
fn round_trips_a_callback_and_classifies_lines() {
    let (tx, rx) = mpsc::channel();
    // The child echoes an update verb for every line it reads, plus one
    // junk line, then exits when stdin closes.
    let fixture = FixtureSupervisor::new();
    let session = fixture.spawn(
            &sh(r#"echo not-json; while read -r _; do echo '{"method":"update","params":{"surface":"s"}}'; break; done"#),
            &BTreeMap::new(),
            None,
            move |ev| {
                drop(tx.send(ev)); // best-effort: test receiver may be gone
            },
        )
        .unwrap();
    session
        .writer()
        .notify(PluginCallback::Render, serde_json::json!({}))
        .unwrap();
    session.writer().close();
    let events = collect(&rx);
    assert!(
        matches!(events.first(), Some(SessionEvent::Junk(j)) if j == "not-json"),
        "{events:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, SessionEvent::Message(m) if m.method.as_str() == "update")),
        "{events:?}"
    );
    assert!(
        matches!(events.last(), Some(SessionEvent::Exit { code: Some(0) })),
        "{events:?}"
    );
}

#[test]
fn responses_are_classified_and_kill_delivers_exit() {
    let (tx, rx) = mpsc::channel();
    let fixture = FixtureSupervisor::new();
    let session = fixture
        .spawn(
            &sh(r#"echo '{"id":7,"result":{"ok":true}}'; sleep 30"#),
            &BTreeMap::new(),
            None,
            move |ev| {
                drop(tx.send(ev)); // best-effort: test receiver may be gone
            },
        )
        .unwrap();
    let first = rx.recv_timeout(Duration::from_secs(10)).unwrap();
    assert!(
        matches!(&first, SessionEvent::Response(r) if r.id == 7),
        "{first:?}"
    );
    session.kill();
    let events = collect(&rx);
    assert!(
        matches!(events.last(), Some(SessionEvent::Exit { .. })),
        "{events:?}"
    );
}

#[test]
fn writes_to_a_dead_session_error() {
    let (tx, rx) = mpsc::channel();
    let fixture = FixtureSupervisor::new();
    let session = fixture
        .spawn(&sh("exit 3"), &BTreeMap::new(), None, move |ev| {
            drop(tx.send(ev)); // best-effort: test receiver may be gone (dead-session case)
        })
        .unwrap();
    let events = collect(&rx);
    assert!(
        matches!(events.last(), Some(SessionEvent::Exit { code: Some(3) })),
        "{events:?}"
    );
    // The reader closed the writer on EOF; a late notify errors cleanly.
    assert!(
        session
            .writer()
            .notify(PluginCallback::Render, serde_json::json!({}))
            .is_err()
    );
}

#[test]
fn resident_nonreading_stdin_never_blocks_admission_or_shutdown() {
    let fixture = FixtureSupervisor::new();
    let session = fixture
        .spawn(&sh("sleep 30"), &BTreeMap::new(), None, |_| {})
        .unwrap();
    let writer = session.writer();
    let large = "x".repeat(crate::plugin::proc::MAX_LINE_BYTES - 1);
    let start = std::time::Instant::now();
    let mut full = 0;
    for _ in 0..20 {
        if writer.send_raw(&large) == Err(SessionFailure::Full) {
            full += 1;
        }
    }
    assert!(full > 0);
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "admission performed pipe I/O"
    );
    session.kill();
    let report = fixture.shutdown();
    assert!(report.is_settled(), "{report:?}");
    assert_eq!(writer.send_raw("late"), Err(SessionFailure::Closed));
}

#[test]
fn resident_stdout_eof_while_alive_closes_and_reaps() {
    let fixture = FixtureSupervisor::new();
    let session = fixture
        .spawn(&sh("exec 1>&-; sleep 30"), &BTreeMap::new(), None, |_| {})
        .unwrap();
    let mut completion = session.completion();
    let result = fixture.runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(2), async {
            while completion.borrow().is_none() {
                completion.changed().await.unwrap();
            }
            completion.borrow().clone().unwrap()
        })
        .await
        .unwrap()
    });
    assert!(result.settled(), "{result:?}");
    assert_eq!(result.reason, CloseReason::StdoutClosed);
    assert_eq!(result.tree, TreeGuarantee::Unproven);
}

#[test]
fn resident_callback_panic_keeps_custody_and_closes_transport() {
    let fixture = FixtureSupervisor::new();
    let session = fixture
        .spawn(&sh("echo '{}'; sleep 30"), &BTreeMap::new(), None, |_| {
            panic!("fixture callback panic")
        })
        .unwrap();
    let mut completion = session.completion();
    let result = fixture.runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(2), async {
            while completion.borrow().is_none() {
                completion.changed().await.unwrap();
            }
            completion.borrow().clone().unwrap()
        })
        .await
        .unwrap()
    });
    assert_eq!(result.reason, CloseReason::CallbackPanic);
    assert!(result.settled(), "{result:?}");
    assert!(session.writer().is_closed());
}

#[test]
fn resident_shutdown_is_concurrent_idempotent_and_cancellation_safe() {
    let fixture = FixtureSupervisor::new();
    let mut sessions = Vec::new();
    for n in 0..4 {
        sessions.push(
            ResidentSession::spawn(
                &fixture.supervisor,
                &format!("fixture-{n}"),
                &sh("sleep 30"),
                &BTreeMap::new(),
                None,
                |_| {},
            )
            .unwrap(),
        );
    }
    let start = std::time::Instant::now();
    let (one, two) = fixture.runtime.block_on(async {
        let deadline = Instant::now() + Duration::from_secs(2);
        // Cancellation of a shutdown waiter must leave the owner JoinHandles in
        // registry-owned mutexes, available to these subsequent waiters.
        drop(
            tokio::time::timeout(Duration::ZERO, fixture.supervisor.shutdown_until(deadline)).await,
        );
        tokio::join!(
            fixture.supervisor.shutdown_until(deadline),
            fixture.supervisor.shutdown_until(deadline)
        )
    });
    assert!(one.is_settled(), "{one:?}");
    assert!(two.is_settled(), "{two:?}");
    assert_eq!(one.outcomes.len(), 4);
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "serial per-plugin timeout"
    );
}

#[test]
fn resident_failed_spawn_releases_admission_and_settled_identity_restarts() {
    let fixture = FixtureSupervisor::new();
    assert!(
        fixture
            .spawn(
                &["/fixture/does-not-exist".into()],
                &BTreeMap::new(),
                None,
                |_| {}
            )
            .is_err()
    );
    assert!(
        fixture
            .supervisor
            .0
            .registry
            .lock()
            .unwrap()
            .entries
            .is_empty()
    );
    let session = fixture
        .spawn(&sh("exit 0"), &BTreeMap::new(), None, |_| {})
        .unwrap();
    let mut receipt = session.completion();
    fixture.runtime.block_on(async {
        while receipt.borrow().is_none() {
            receipt.changed().await.unwrap();
        }
    });
    fixture.supervisor.wait_for_release("fixture").unwrap();
    let replacement = fixture
        .spawn(&sh("sleep 30"), &BTreeMap::new(), None, |_| {})
        .unwrap();
    assert!(!replacement.writer().is_closed());
    replacement.kill();
}

#[test]
fn resident_owner_abort_before_first_poll_retains_custody_for_cleanup() {
    // A current-thread runtime is deliberately not driven until after abort.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let supervisor = ResidentSupervisor::new(runtime.handle().clone());
    let session = ResidentSession::spawn(
        &supervisor,
        "unpolled",
        &sh("sleep 30"),
        &BTreeMap::new(),
        None,
        |_| {},
    )
    .unwrap();
    {
        let registry = supervisor.0.registry.lock().unwrap();
        registry.entries["unpolled"]
            .task
            .try_lock()
            .unwrap()
            .handle
            .as_ref()
            .unwrap()
            .abort();
    }
    let report =
        runtime.block_on(supervisor.shutdown_until(Instant::now() + Duration::from_secs(2)));
    assert!(
        report.outcomes.iter().all(|(_, result)| result.settled()),
        "{report:?}"
    );
    // The retry reaps the fixture, but a cancelled owner is still a reported
    // failure; joining a later cleanup task must not erase that fact.
    assert!(!report.is_settled());
    assert!(
        report
            .unresolved
            .iter()
            .any(|entry| entry.contains("cancelled"))
    );
    assert!(
        supervisor.0.registry.lock().unwrap().entries["unpolled"]
            .held
            .lock()
            .unwrap()
            .is_none()
    );
    assert!(session.writer().is_closed());
}

#[test]
fn resident_cancelled_join_waiter_retains_handle_and_global_deadline() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let supervisor = ResidentSupervisor::new(runtime.handle().clone());
    let mut releases = Vec::new();
    let mut tasks = Vec::new();
    for n in 0..4 {
        let shared = Shared::new();
        shared.outcome.send_replace(Some(SessionOutcome {
            reason: CloseReason::Shutdown,
            leader_reaped: true,
            code: Some(0),
            pipes_settled: true,
            tree: TreeGuarantee::Unproven,
            termination_requested: true,
            errors: Vec::new(),
        }));
        let (release, ready) = tokio::sync::oneshot::channel();
        releases.push(release);
        let task = Arc::new(tokio::sync::Mutex::new(TaskState {
            handle: Some(runtime.spawn(async move {
                ready.await.expect("fixture release retained");
            })),
            joined: None,
            failures: Vec::new(),
        }));
        supervisor.0.registry.lock().unwrap().entries.insert(
            format!("barrier-{n}"),
            Entry {
                shared,
                task: task.clone(),
                held: Arc::new(Mutex::new(None)),
            },
        );
        tasks.push(task);
    }
    runtime.block_on(async {
        let closing = supervisor.clone();
        let waiter = tokio::spawn(async move {
            closing
                .shutdown_until(Instant::now() + Duration::from_secs(2))
                .await
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            // Positive proof that the shutdown waiter owns the join-state lock
            // while the lifecycle task is held behind the fixture barrier.
            while tasks[0].try_lock().is_ok() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        waiter.abort();
        assert!(waiter.await.unwrap_err().is_cancelled());
        assert!(tasks[0].lock().await.handle.is_some());
        let start = std::time::Instant::now();
        let report = supervisor
            .shutdown_until(Instant::now() + Duration::from_millis(60))
            .await;
        assert!(!report.is_settled());
        assert_eq!(report.unresolved.len(), 4);
        assert!(
            start.elapsed() < Duration::from_millis(180),
            "four held joins multiplied the deadline"
        );
        for release in releases {
            release.send(()).expect("fixture owner waiting");
        }
        let deadline = Instant::now() + Duration::from_secs(1);
        let (one, two) = tokio::join!(
            supervisor.shutdown_until(deadline),
            supervisor.shutdown_until(deadline)
        );
        assert!(one.is_settled(), "{one:?}");
        assert!(two.is_settled(), "{two:?}");
    });
}

#[test]
fn resident_shutdown_during_spawn_registration_does_not_panic_or_lose_child() {
    let fixture = FixtureSupervisor::new();
    let (entered, reached) = mpsc::channel();
    let (resume, resumed) = mpsc::channel();
    *fixture.supervisor.0.after_spawn.lock().unwrap() = Some(Box::new(move || {
        entered.send(()).unwrap();
        resumed.recv().unwrap();
    }));
    let spawning = fixture.supervisor.clone();
    let thread = std::thread::spawn(move || {
        ResidentSession::spawn(
            &spawning,
            "racing-spawn",
            &sh("sleep 30"),
            &BTreeMap::new(),
            None,
            |_| {},
        )
        .unwrap()
    });
    reached.recv_timeout(Duration::from_secs(2)).unwrap();
    let first = fixture.runtime.block_on(
        fixture
            .supervisor
            .shutdown_until(Instant::now() + Duration::from_millis(30)),
    );
    assert!(!first.is_settled());
    resume.send(()).unwrap();
    let session = thread.join().expect("spawn registration raced shutdown");
    let result = fixture.shutdown();
    assert!(result.is_settled(), "{result:?}");
    assert!(session.writer().is_closed());
}
