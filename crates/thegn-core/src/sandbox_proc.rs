//! Bounded subprocess capture adapter for sandbox control probes.
//!
//! The shared `thegn_proc` runner owns the process group and deadline; the
//! deadline also closes inherited pipes held by descendants.

use std::process::{Child, Command, ExitStatus};
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{self, Sender};
use std::time::Duration;

const KILL_REAP_BUDGET: Duration = Duration::from_millis(250);
const READER_REAP_BUDGET: Duration = Duration::from_millis(250);

/// Which streams to capture and their respective byte ceilings.
#[derive(Clone, Copy)]
pub(crate) struct Capture {
    pub stdout: Option<usize>,
    pub stderr: Option<usize>,
}

#[derive(Debug, Default)]
pub(crate) struct Captured {
    bytes: Vec<u8>,
    truncated: bool,
}

/// The complete set of outcomes from a bounded command.
#[derive(Debug)]
pub(crate) enum ProcOutcome {
    Spawn,
    Timeout,
    Wait,
    Read,
    Exited {
        status: ExitStatus,
        stdout: Captured,
        stderr: Captured,
    },
}

impl ProcOutcome {
    pub(crate) fn into_stdout_and_stderr(self) -> Option<(bool, String, String)> {
        match self {
            Self::Exited {
                status,
                stdout,
                stderr,
            } if !stdout.truncated && !stderr.truncated => Some((
                status.success(),
                String::from_utf8_lossy(&stdout.bytes).into_owned(),
                String::from_utf8_lossy(&stderr.bytes).into_owned(),
            )),
            Self::Exited { .. } | Self::Spawn | Self::Wait | Self::Read | Self::Timeout => None,
        }
    }

    pub(crate) fn into_stdout(self) -> Option<(bool, String)> {
        match self {
            Self::Exited { status, stdout, .. } if !stdout.truncated => Some((
                status.success(),
                String::from_utf8_lossy(&stdout.bytes).into_owned(),
            )),
            Self::Exited { .. } | Self::Spawn | Self::Wait | Self::Read | Self::Timeout => None,
        }
    }

    pub(crate) fn into_stderr(self) -> Option<(bool, String)> {
        match self {
            Self::Exited { status, stderr, .. } if !stderr.truncated => Some((
                status.success(),
                String::from_utf8_lossy(&stderr.bytes).into_owned(),
            )),
            Self::Exited { .. } | Self::Spawn | Self::Wait | Self::Read | Self::Timeout => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InputDelivery {
    Complete,
    Failed,
}

pub(crate) fn run_bounded(argv: &[String], timeout: Duration, capture: Capture) -> ProcOutcome {
    run_inner(argv, timeout, capture, None).0
}

pub(crate) fn run_bounded_with_input(
    argv: &[String],
    timeout: Duration,
    capture: Capture,
    input: Vec<u8>,
) -> (ProcOutcome, InputDelivery) {
    run_inner(argv, timeout, capture, Some(input))
}

fn stream_for(cap: Option<usize>) -> thegn_proc::Stream {
    match cap {
        Some(cap) => thegn_proc::Stream::Head { cap },
        None => thegn_proc::Stream::Null,
    }
}

fn captured(capture: thegn_proc::Capture) -> Captured {
    Captured {
        bytes: capture.bytes,
        truncated: capture.truncated,
    }
}

/// The complete mapping from a shared-runner `Run` to sandbox probe outcomes.
fn map_run(run: thegn_proc::Run) -> ProcOutcome {
    let thegn_proc::Run {
        end,
        leader,
        stdout,
        stderr,
        ..
    } = run;
    match (end, leader) {
        (thegn_proc::End::Spawn(_), _) => ProcOutcome::Spawn,
        (thegn_proc::End::TimedOut, _) => ProcOutcome::Timeout,
        (thegn_proc::End::LeaderExited, thegn_proc::Leader::Reaped(status)) => {
            ProcOutcome::Exited {
                status,
                stdout: captured(stdout),
                stderr: captured(stderr),
            }
        }
        (thegn_proc::End::LeaderExited, _)
        | (thegn_proc::End::Supervise(_), _)
        | (thegn_proc::End::Cancelled, _)
        | (thegn_proc::End::OutputLimit(_), _) => ProcOutcome::Wait,
    }
}

fn run_inner(
    argv: &[String],
    timeout: Duration,
    capture: Capture,
    input: Option<Vec<u8>>,
) -> (ProcOutcome, InputDelivery) {
    let Some(program) = argv.first() else {
        return (ProcOutcome::Spawn, InputDelivery::Failed);
    };
    let mut command = Command::new(program);
    command.args(&argv[1..]);
    let limits = thegn_proc::Limits {
        kill_settle: KILL_REAP_BUDGET,
        reader_grace: READER_REAP_BUDGET,
        stdout: stream_for(capture.stdout),
        stderr: stream_for(capture.stderr),
        ..thegn_proc::Limits::silent(timeout)
    };
    let cancel = AtomicBool::new(false);
    let (run, feed) = match input.as_deref() {
        Some(bytes) => {
            let (run, feed) = thegn_proc::run_with_input(&mut command, &limits, bytes, &cancel);
            (run, Some(feed))
        }
        None => (thegn_proc::run(&mut command, &limits, &cancel), None),
    };
    let outcome = map_run(run);
    let delivery = match (&outcome, feed) {
        (ProcOutcome::Exited { .. }, Some(feed)) if !feed.complete => InputDelivery::Failed,
        (ProcOutcome::Exited { .. }, _) => InputDelivery::Complete,
        _ => InputDelivery::Failed,
    };
    (outcome, delivery)
}

/// Put a long-lived sandbox helper in its own process group where the
/// platform supports group termination.
pub(crate) fn configure_event_process_group(_command: &mut Command) {
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(_command, 0);
}

/// Kill the process group for an events child (or the child itself on
/// platforms without process groups).
pub(crate) fn terminate_event_group(child: &mut Child) {
    #[cfg(unix)]
    {
        use nix::sys::signal::{Signal, killpg};
        use nix::unistd::Pid;
        if let Err(error) = killpg(Pid::from_raw(child.id() as i32), Signal::SIGKILL) {
            tracing::debug!(%error, "sandbox events: process-group kill failed");
        }
    }
    #[cfg(windows)]
    if let Err(error) = child.kill() {
        tracing::debug!(%error, "sandbox events: child kill failed");
    }
}

/// Hand a child that outlived its shutdown budget to the detached reaper.
/// If the reaper is unavailable, a dedicated waiter owns the child; if that
/// thread cannot start, this call waits inline.
pub(crate) fn reap_event_child(child: Child) {
    reap_event_child_with(child, reaper_sender());
}

fn reap_event_child_with(child: Child, reaper: Option<&Sender<Child>>) {
    let child = match reaper {
        Some(sender) => match sender.send(child) {
            Ok(()) => return,
            Err(mpsc::SendError(child)) => child,
        },
        None => child,
    };

    let child = std::sync::Arc::new(std::sync::Mutex::new(Some(child)));
    let waiter_child = std::sync::Arc::clone(&child);
    match crate::thread_name::try_spawn_named("thegn-sbx-wait", move || {
        let child = waiter_child
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(mut child) = child
            && let Err(error) = child.wait()
        {
            tracing::debug!(target: "thegn::sandbox", %error, "sandbox events: waiter failed to reap child");
        }
    }) {
        Ok(_waiter) => {}
        Err(error) => {
            tracing::debug!(target: "thegn::sandbox", %error, "sandbox events: waiter could not start; waiting inline");
            let child = child
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            if let Some(mut child) = child
                && let Err(error) = child.wait()
            {
                tracing::debug!(target: "thegn::sandbox", %error, "sandbox events: inline wait failed to reap child");
            }
        }
    }
}

fn reaper_sender() -> Option<&'static Sender<Child>> {
    static REAPER: OnceLock<Option<Sender<Child>>> = OnceLock::new();
    REAPER
        .get_or_init(|| {
            let (sender, receiver) = mpsc::channel::<Child>();
            crate::thread_name::try_spawn_named("thegn-sbx-proc", move || {
                while let Ok(mut child) = receiver.recv() {
                    Result::ok(child.wait());
                }
            })
            .ok()?;
            Some(sender)
        })
        .as_ref()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;
    use std::thread;
    use std::time::Instant;

    fn sh(script: &str) -> Vec<String> {
        vec!["sh".into(), "-c".into(), script.into()]
    }

    #[cfg(unix)]
    fn killed_event_child() -> (Child, u32) {
        let mut command = std::process::Command::new("sh");
        command.args(["-c", "sleep 30"]);
        configure_event_process_group(&mut command);
        let mut child = command.spawn().unwrap();
        let pid = child.id();
        terminate_event_group(&mut child);
        (child, pid)
    }

    #[cfg(unix)]
    fn assert_pid_is_reaped(pid: u32) {
        let pid = nix::unistd::Pid::from_raw(pid as i32);
        let deadline = Instant::now() + Duration::from_secs(1);
        while nix::sys::signal::kill(pid, None).is_ok() {
            assert!(
                Instant::now() < deadline,
                "killed child {pid} was not reaped"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[cfg(unix)]
    #[test]
    fn event_child_is_reaped_without_a_reaper() {
        let (child, pid) = killed_event_child();
        reap_event_child_with(child, None);
        assert_pid_is_reaped(pid);
    }

    #[cfg(unix)]
    #[test]
    fn event_child_is_reaped_when_the_reaper_is_disconnected() {
        let (sender, receiver) = std::sync::mpsc::channel::<Child>();
        drop(receiver);
        let (child, pid) = killed_event_child();
        reap_event_child_with(child, Some(&sender));
        assert_pid_is_reaped(pid);
    }

    #[cfg(unix)]
    #[test]
    fn map_run_maps_every_end_and_a_missing_leader() {
        let run = |end, leader| thegn_proc::Run {
            end,
            group: thegn_proc::GroupState::Unknown,
            leader,
            leader_exited_first: false,
            stdout: thegn_proc::Capture::default(),
            stderr: thegn_proc::Capture::default(),
            containment: thegn_proc::Containment::ProcessGroup,
        };

        assert!(matches!(
            map_run(run(thegn_proc::End::TimedOut, thegn_proc::Leader::Detached)),
            ProcOutcome::Timeout
        ));
        assert!(matches!(
            map_run(run(
                thegn_proc::End::Cancelled,
                thegn_proc::Leader::Detached
            )),
            ProcOutcome::Wait
        ));
        assert!(matches!(
            map_run(run(
                thegn_proc::End::Supervise(io::Error::other("wait failed")),
                thegn_proc::Leader::ReapFailed(io::Error::other("wait failed")),
            )),
            ProcOutcome::Wait
        ));
        assert!(matches!(
            map_run(run(
                thegn_proc::End::OutputLimit(thegn_proc::Which::Stdout),
                thegn_proc::Leader::Detached,
            )),
            ProcOutcome::Wait
        ));
        assert!(matches!(
            map_run(run(
                thegn_proc::End::LeaderExited,
                thegn_proc::Leader::Detached
            )),
            ProcOutcome::Wait
        ));
        assert!(matches!(
            map_run(run(
                thegn_proc::End::Spawn(io::Error::from(io::ErrorKind::NotFound)),
                thegn_proc::Leader::NotSpawned
            )),
            ProcOutcome::Spawn
        ));

        let status = std::process::Command::new("true").status().unwrap();
        match map_run(run(
            thegn_proc::End::LeaderExited,
            thegn_proc::Leader::Reaped(status),
        )) {
            ProcOutcome::Exited { stdout, stderr, .. } => {
                assert!(!stdout.truncated);
                assert!(!stderr.truncated);
            }
            other => panic!("reaped leader should map to exit, got {other:?}"),
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn bounded_runs_start_no_helper_threads() {
        let runner = thread::spawn(|| {
            run_bounded_with_input(
                &sh("sleep 1"),
                Duration::from_secs(5),
                Capture {
                    stdout: Some(64),
                    stderr: Some(64),
                },
                b"x".to_vec(),
            )
        });
        thread::sleep(Duration::from_millis(300));
        let names: Vec<String> = std::fs::read_dir("/proc/self/task")
            .unwrap()
            .filter_map(|entry| std::fs::read_to_string(entry.unwrap().path().join("comm")).ok())
            .map(|name| name.trim().to_string())
            .collect();
        let (outcome, delivery) = runner.join().unwrap();
        assert!(
            !names
                .iter()
                .any(|name| name == "thegn-sbx-read" || name == "thegn-sbx-feed"),
            "a bounded run must not spawn reader/feeder threads: {names:?}"
        );
        assert!(matches!(outcome, ProcOutcome::Exited { .. }));
        assert_eq!(delivery, InputDelivery::Complete);
    }

    #[test]
    fn reports_spawn_and_exit_outcomes_through_adapters() {
        assert!(matches!(
            run_bounded(
                &[],
                Duration::from_millis(10),
                Capture {
                    stdout: None,
                    stderr: None
                }
            ),
            ProcOutcome::Spawn
        ));
        assert!(matches!(
            run_bounded(
                &["/no/such/sandbox-command".into()],
                Duration::from_millis(10),
                Capture {
                    stdout: None,
                    stderr: None
                }
            ),
            ProcOutcome::Spawn
        ));
        let outcome = run_bounded(
            &sh("printf out; printf err >&2; exit 7"),
            Duration::from_secs(1),
            Capture {
                stdout: Some(16),
                stderr: Some(16),
            },
        );
        match outcome {
            ProcOutcome::Exited {
                status,
                stdout,
                stderr,
            } => {
                assert!(!status.success());
                assert_eq!(String::from_utf8_lossy(&stdout.bytes), "out");
                assert_eq!(String::from_utf8_lossy(&stderr.bytes), "err");
            }
            other => panic!("expected exit outcome, got {other:?}"),
        }
        let failed = run_bounded(
            &sh("printf out; printf err >&2; exit 7"),
            Duration::from_secs(1),
            Capture {
                stdout: Some(16),
                stderr: Some(16),
            },
        );
        assert_eq!(
            failed.into_stdout_and_stderr().unwrap(),
            (false, "out".into(), "err".into())
        );
        let success = run_bounded(
            &sh("printf captured; printf diagnostic >&2"),
            Duration::from_secs(1),
            Capture {
                stdout: Some(16),
                stderr: Some(16),
            },
        );
        assert_eq!(success.into_stdout().unwrap(), (true, "captured".into()));
        let success = run_bounded(
            &sh("printf captured; printf diagnostic >&2"),
            Duration::from_secs(1),
            Capture {
                stdout: Some(16),
                stderr: Some(16),
            },
        );
        assert_eq!(success.into_stderr().unwrap(), (true, "diagnostic".into()));
        assert!(ProcOutcome::Timeout.into_stdout().is_none());
        assert!(ProcOutcome::Spawn.into_stderr().is_none());
        assert!(ProcOutcome::Wait.into_stdout().is_none());
        assert!(ProcOutcome::Read.into_stderr().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn bounded_input_is_delivered_and_epipe_keeps_child_diagnostics() {
        let (outcome, delivery) = run_bounded_with_input(
            &sh("cat >&2"),
            Duration::from_secs(1),
            Capture {
                stdout: None,
                stderr: Some(64),
            },
            b"K=v\n".to_vec(),
        );
        assert_eq!(delivery, InputDelivery::Complete);
        match outcome {
            ProcOutcome::Exited { stderr, .. } => assert_eq!(stderr.bytes, b"K=v\n"),
            other => panic!("expected captured input, got {other:?}"),
        }

        let input = vec![b'x'; 1024 * 1024];
        let (outcome, delivery) = run_bounded_with_input(
            &sh("printf diag >&2; exit 3"),
            Duration::from_secs(1),
            Capture {
                stdout: None,
                stderr: Some(64),
            },
            input,
        );
        assert_eq!(delivery, InputDelivery::Failed);
        match outcome {
            ProcOutcome::Exited { status, stderr, .. } => {
                assert_eq!(status.code(), Some(3));
                assert_eq!(stderr.bytes, b"diag");
            }
            other => panic!("child diagnostics must survive stdin EPIPE: {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn bounded_input_deadline_kills_descendant_holding_stdin() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let pid_file = dir.path().join("stdin-descendant-pid");
        let script = format!(
            "exec 3<&0; sleep 30 & echo $! > '{}'; exit 0",
            pid_file.display()
        );
        let started = Instant::now();
        let (outcome, delivery) = run_bounded_with_input(
            &sh(&script),
            Duration::from_millis(300),
            Capture {
                stdout: None,
                stderr: None,
            },
            vec![b'x'; 1024 * 1024],
        );
        assert!(matches!(outcome, ProcOutcome::Exited { .. }));
        assert_eq!(delivery, InputDelivery::Failed);
        assert!(started.elapsed() < Duration::from_secs(2));
        let pid: i32 = std::fs::read_to_string(pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let deadline = Instant::now() + Duration::from_millis(500);
        loop {
            if nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_err() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "stdin-holding descendant {pid} survived"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[cfg(unix)]
    #[test]
    fn bounded_input_times_out_and_repeated_timeouts_reap_leaders() {
        use std::os::unix::fs::PermissionsExt;
        let (outcome, delivery) = run_bounded_with_input(
            &sh("sleep 5"),
            Duration::from_millis(100),
            Capture {
                stdout: None,
                stderr: None,
            },
            b"secret".to_vec(),
        );
        assert!(matches!(outcome, ProcOutcome::Timeout));
        assert_eq!(delivery, InputDelivery::Failed);

        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        for index in 0..5 {
            let pid_file = dir.path().join(format!("leader-{index}"));
            let script = format!("echo $$ > '{}'; sleep 30", pid_file.display());
            assert!(matches!(
                run_bounded(
                    &sh(&script),
                    Duration::from_millis(100),
                    Capture {
                        stdout: None,
                        stderr: None
                    }
                ),
                ProcOutcome::Timeout
            ));
            let pid: i32 = std::fs::read_to_string(pid_file)
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(1);
            while nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok() {
                assert!(
                    Instant::now() < deadline,
                    "timed-out leader {pid} was not reaped"
                );
                thread::sleep(Duration::from_millis(10));
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn descendant_holding_pipe_does_not_outlive_deadline() {
        let started = Instant::now();
        let outcome = run_bounded(
            &sh("sleep 30 & echo hi"),
            Duration::from_millis(300),
            Capture {
                stdout: Some(64),
                stderr: None,
            },
        );
        match outcome {
            ProcOutcome::Exited { stdout, .. } => assert_eq!(stdout.bytes, b"hi\n"),
            other => panic!("leader should finish despite inherited pipe: {other:?}"),
        }
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[cfg(unix)]
    #[test]
    fn flood_is_drained_and_capture_is_capped() {
        let outcome = run_bounded(
            &sh("head -c 4000000 /dev/zero | tee /dev/stderr"),
            Duration::from_secs(2),
            Capture {
                stdout: Some(128),
                stderr: Some(96),
            },
        );
        match outcome {
            ProcOutcome::Exited { stdout, stderr, .. } => {
                assert_eq!(stdout.bytes.len(), 128);
                assert!(stdout.truncated);
                assert_eq!(stderr.bytes.len(), 96);
                assert!(stderr.truncated);
            }
            other => panic!("expected completed bounded capture, got {other:?}"),
        }
    }

    #[test]
    fn truncated_capture_is_not_reported_as_complete_output() {
        let stdout = run_bounded(
            &sh("printf 12345"),
            Duration::from_secs(1),
            Capture {
                stdout: Some(4),
                stderr: None,
            },
        );
        assert!(stdout.into_stdout().is_none());

        let stderr = run_bounded(
            &sh("printf 12345 >&2"),
            Duration::from_secs(1),
            Capture {
                stdout: None,
                stderr: Some(4),
            },
        );
        assert!(stderr.into_stderr().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn deadline_kills_descendant_process_group() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let pid_file = dir.path().join("descendant-pid");
        let script = format!("sleep 30 & echo $! > '{}'; sleep 30", pid_file.display());
        let outcome = run_bounded(
            &sh(&script),
            Duration::from_millis(300),
            Capture {
                stdout: None,
                stderr: None,
            },
        );
        assert!(matches!(outcome, ProcOutcome::Timeout));
        let pid: i32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let deadline = Instant::now() + Duration::from_millis(500);
        loop {
            let alive = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok();
            if !alive {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "descendant process {pid} survived group termination"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
}
