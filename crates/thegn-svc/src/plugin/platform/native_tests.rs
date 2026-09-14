//! Real-kernel lifecycle fixtures, independent of a shell or external programs.
//! The ignored entry is reexecuted only by these tests, never shipped as a tool.
use crate::plugin::provider::ProviderBridge;
use crate::plugin::session::{
    CloseReason, ResidentSession, ResidentSupervisor, SessionEvent, SessionFailure, SessionOutcome,
    ShutdownReport, TreeGuarantee,
};
use std::collections::BTreeMap;
use std::io::{BufRead, Write};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const CHILD_MODE: &str = "THEGN_TEST_RESIDENT_NATIVE_CHILD";
const CHILD_ENTRY: &str = "plugin::platform::native_tests::resident_native_fixture_entry";
const READY: &str = "fixture_ready";
const TEST_BUDGET: Duration = Duration::from_secs(5);

fn child_argv() -> Vec<String> {
    vec![
        std::env::current_exe()
            .expect("test executable")
            .into_os_string()
            .into_string()
            .expect("test executable path is UTF-8"),
        "--ignored".into(),
        "--exact".into(),
        CHILD_ENTRY.into(),
        "--nocapture".into(),
        "--test-threads=1".into(),
    ]
}

// Raw closure is confined to the fixture process. It subsequently exits without
// running libtest's stdout reporter, and never reuses the closed descriptor.
#[cfg(unix)]
fn close_standard_stream(stdout: bool) {
    let fd = if stdout {
        libc::STDOUT_FILENO
    } else {
        libc::STDIN_FILENO
    };
    assert_eq!(unsafe { libc::close(fd) }, 0);
}
#[cfg(windows)]
fn close_standard_stream(stdout: bool) {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Console::{
        GetStdHandle, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle,
    };
    let slot = if stdout {
        STD_OUTPUT_HANDLE
    } else {
        STD_INPUT_HANDLE
    };
    let handle = unsafe { GetStdHandle(slot) };
    assert_ne!(unsafe { SetStdHandle(slot, std::ptr::null_mut()) }, 0);
    assert_ne!(unsafe { CloseHandle(handle) }, 0);
}

#[test]
#[ignore = "helper only: exact reexec with a private fixture mode, not a regression"]
fn resident_native_fixture_entry() {
    let Ok(mode) = std::env::var(CHILD_MODE) else {
        return;
    };
    // A finite watchdog is owned by this fixture process. No descendant or
    // detached host thread survives a failed assertion/transport deadlock.
    let _watchdog = std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(20));
        std::process::exit(90);
    });
    if mode == "closed-stdin" {
        close_standard_stream(false);
    }
    // Leading newline isolates the handshake from libtest's test-name prefix.
    println!("\n{{\"method\":\"{READY}\",\"params\":{{}}}}");
    std::io::stdout().flush().unwrap();
    match mode.as_str() {
        "blocked" | "closed-stdin" => {}
        "stdout-eof" => close_standard_stream(true),
        "final-reply" | "eof-on-request" | "invalid-output" | "callback" => {
            let mut line = String::new();
            assert!(std::io::stdin().lock().read_line(&mut line).unwrap() > 0);
            match mode.as_str() {
                "final-reply" => {
                    let request: serde_json::Value = serde_json::from_str(&line).unwrap();
                    println!(
                        "{}",
                        serde_json::json!({"id": request["id"], "result": {"final": true}})
                    );
                    std::io::stdout().flush().unwrap();
                    std::process::exit(0);
                }
                "eof-on-request" => close_standard_stream(true),
                "invalid-output" => {
                    std::io::stdout().write_all(b"\xff\n").unwrap();
                    std::io::stdout().flush().unwrap();
                }
                "callback" => {
                    println!("{{\"method\":\"fixture_callback\",\"params\":{{}}}}");
                    std::io::stdout().flush().unwrap();
                }
                _ => unreachable!(),
            }
        }
        _ => panic!("unknown private fixture mode: {mode}"),
    }
    // Keep the leader and both remaining pipes alive without reading stdin.
    std::thread::sleep(Duration::from_secs(20));
    std::process::exit(91);
}

struct Fixture {
    supervisor: ResidentSupervisor,
    runtime: tokio::runtime::Runtime,
}
impl Fixture {
    fn new() -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        Self {
            supervisor: ResidentSupervisor::new(runtime.handle().clone()),
            runtime,
        }
    }

    fn spawn(&self, key: &str, mode: &str) -> ResidentSession {
        self.spawn_with_callback(key, mode, |_| {})
    }

    fn spawn_with_callback(
        &self,
        key: &str,
        mode: &str,
        callback: impl Fn(SessionEvent) + Send + Sync + 'static,
    ) -> ResidentSession {
        let (ready, handshake) = mpsc::sync_channel(1);
        let session = ResidentSession::spawn(
            &self.supervisor,
            key,
            &child_argv(),
            &BTreeMap::from([(CHILD_MODE.into(), mode.into())]),
            None,
            move |event| {
                if matches!(&event, SessionEvent::Message(message) if message.method.as_str() == READY)
                {
                    ready.try_send(()).expect("one fixture handshake");
                } else if !matches!(event, SessionEvent::Junk(_)) {
                    // libtest startup text is diagnostic junk before READY.
                    callback(event);
                }
            },
        )
        .unwrap();
        handshake
            .recv_timeout(TEST_BUDGET)
            .expect("native child READY");
        session
    }

    fn outcome(&self, session: &ResidentSession) -> SessionOutcome {
        let mut receipt = session.completion();
        self.runtime.block_on(async {
            tokio::time::timeout(TEST_BUDGET, async {
                loop {
                    if let Some(outcome) = receipt.borrow().clone() {
                        return outcome;
                    }
                    receipt.changed().await.expect("owner completion");
                }
            })
            .await
            .expect("native lifecycle deadline")
        })
    }

    fn shutdown(&self) -> ShutdownReport {
        self.runtime.block_on(
            self.supervisor
                .shutdown_until(tokio::time::Instant::now() + TEST_BUDGET),
        )
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        // Runs on assertion unwinding too. Supervisor/Process retain exact
        // owned cleanup authority; no numeric PID lookup or broad kill exists.
        let report = self.shutdown();
        if !std::thread::panicking() {
            assert!(report.is_settled(), "fixture cleanup: {report:?}");
        }
    }
}

fn assert_owned_settlement(outcome: &SessionOutcome) {
    assert!(outcome.settled(), "{outcome:?}");
    assert!(outcome.errors.is_empty(), "{outcome:?}");
    assert_eq!(outcome.tree, TreeGuarantee::Unproven);
}

#[test]
fn resident_native_stopped_reader_preserves_ui_progress_and_control() {
    let fixture = Fixture::new();
    let session = fixture.spawn("blocked-ui", "blocked");
    let writer = session.writer();
    let large = "x".repeat(crate::plugin::proc::MAX_LINE_BYTES - 1);
    let start = Instant::now();
    let mut heartbeats = 0;
    let mut full = 0;
    // The calling thread stands in for the compositor: every admitted/refused
    // callback is followed by an input/render heartbeat on THAT SAME thread.
    // READY proves a real child is alive, not merely awaiting process startup.
    for _ in 0..20 {
        match writer.send_raw(&large) {
            Ok(()) => {}
            Err(SessionFailure::Full) => full += 1,
            other => panic!("unexpected admission: {other:?}"),
        }
        heartbeats += 1;
    }
    assert_eq!(heartbeats, 20);
    assert!(full > 0);
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "UI callback stalled"
    );
    assert!(!writer.is_closed(), "write deadline rescued the UI first");
    let control = Instant::now();
    session.kill();
    assert!(control.elapsed() < Duration::from_millis(100));
    assert_owned_settlement(&fixture.outcome(&session));
    assert_eq!(writer.send_raw("late"), Err(SessionFailure::Closed));
}

#[test]
fn resident_native_stopped_reader_hits_write_deadline() {
    let fixture = Fixture::new();
    let session = fixture.spawn("blocked-deadline", "blocked");
    session
        .writer()
        .send_raw(&"x".repeat(crate::plugin::proc::MAX_LINE_BYTES - 1))
        .unwrap();
    let outcome = fixture.outcome(&session);
    assert_eq!(outcome.reason, CloseReason::WriteTimeout);
    assert_owned_settlement(&outcome);
}

#[test]
fn resident_native_stdout_eof_while_alive_is_bounded() {
    let fixture = Fixture::new();
    let session = fixture.spawn("stdout-eof", "stdout-eof");
    let outcome = fixture.outcome(&session);
    assert_eq!(outcome.reason, CloseReason::StdoutClosed);
    assert_owned_settlement(&outcome);
}

#[test]
fn resident_native_pipe_and_protocol_failures_close_owned_session() {
    for (mode, reason) in [
        ("closed-stdin", CloseReason::WriteFailed),
        ("invalid-output", CloseReason::Protocol),
    ] {
        let fixture = Fixture::new();
        let session = fixture.spawn(mode, mode);
        session.writer().send_raw("trigger").unwrap();
        let outcome = fixture.outcome(&session);
        assert_eq!(outcome.reason, reason);
        // WriteFailed records the actual native pipe error as a diagnostic.
        assert!(outcome.settled(), "{outcome:?}");
        assert_eq!(outcome.tree, TreeGuarantee::Unproven);
        assert_eq!(
            session.writer().send_raw("late"),
            Err(SessionFailure::Closed)
        );
    }
}

#[test]
fn resident_native_callback_panic_retains_cleanup() {
    let fixture = Fixture::new();
    let session = fixture.spawn_with_callback("callback", "callback", |event| {
        if matches!(event, SessionEvent::Message(_)) {
            panic!("native fixture callback panic");
        }
    });
    session.writer().send_raw("trigger").unwrap();
    let outcome = fixture.outcome(&session);
    assert_eq!(outcome.reason, CloseReason::CallbackPanic);
    assert_owned_settlement(&outcome);
}

#[test]
fn resident_native_final_reply_and_eof_pending_rpc() {
    for mode in ["final-reply", "eof-on-request"] {
        let fixture = Fixture::new();
        let session = fixture.spawn(mode, mode);
        let bridge = ProviderBridge::new(session.writer(), Duration::from_secs(15));
        let start = Instant::now();
        let result = bridge.call("fixture", "last", serde_json::Value::Null);
        if mode == "final-reply" {
            assert_eq!(result.unwrap(), serde_json::json!({"final": true}));
        } else {
            assert!(result.is_err());
        }
        assert!(
            start.elapsed() < TEST_BUDGET,
            "RPC waited for its 15s deadline"
        );
        assert_owned_settlement(&fixture.outcome(&session));
        let start = Instant::now();
        assert!(
            bridge
                .call("fixture", "late", serde_json::Value::Null)
                .is_err()
        );
        assert!(start.elapsed() < Duration::from_millis(100));
    }
}

#[test]
fn resident_native_concurrent_shutdown_and_kill_share_deadline() {
    let fixture = Fixture::new();
    let sessions: Vec<_> = (0..4)
        .map(|n| fixture.spawn(&format!("concurrent-{n}"), "blocked"))
        .collect();
    for session in &sessions {
        session
            .writer()
            .send_raw(&"x".repeat(crate::plugin::proc::MAX_LINE_BYTES - 1))
            .unwrap();
    }
    let start = Instant::now();
    sessions[0].kill();
    let (one, two) = fixture.runtime.block_on(async {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        tokio::join!(
            fixture.supervisor.shutdown_until(deadline),
            fixture.supervisor.shutdown_until(deadline)
        )
    });
    assert!(one.is_settled(), "{one:?}");
    assert!(two.is_settled(), "{two:?}");
    assert_eq!(one.outcomes.len(), 4);
    assert!(start.elapsed() < TEST_BUDGET);
    for (_, outcome) in &one.outcomes {
        assert_owned_settlement(outcome);
    }
}

#[test]
fn resident_native_spawn_failure_releases_workers_and_allows_restart() {
    let fixture = Fixture::new();
    let missing = tempfile::tempdir().unwrap();
    assert!(
        ResidentSession::spawn(
            &fixture.supervisor,
            "replacement",
            &[missing
                .path()
                .join("absent-executable")
                .to_str()
                .unwrap()
                .into()],
            &BTreeMap::new(),
            None,
            |_| {},
        )
        .is_err()
    );
    let old = fixture.spawn("replacement", "blocked");
    old.kill();
    assert_owned_settlement(&fixture.outcome(&old));
    fixture.supervisor.wait_for_release("replacement").unwrap();
    let replacement = fixture.spawn("replacement", "blocked");
    assert!(!replacement.writer().is_closed());
    replacement.kill();
    assert_owned_settlement(&fixture.outcome(&replacement));
}
