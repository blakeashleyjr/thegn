//! Bounded subprocess capture for sandbox control probes.
//!
//! Captured pipes are drained while the process runs. On Unix each command has
//! its own process group, so a deadline also closes inherited pipes held by
//! descendants.

use std::io::{self, Read, Write};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::OnceLock;
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::thread;
use std::time::{Duration, Instant};

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

#[derive(Debug)]
enum ReadOutcome {
    Captured(Captured),
    Failed,
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
    command
        .args(&argv[1..])
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(if capture.stdout.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stderr(if capture.stderr.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(_) => return (ProcOutcome::Spawn, InputDelivery::Failed),
    };

    let feeder = input.and_then(|input| child.stdin.take().map(|stdin| spawn_feeder(stdin, input)));

    let stdout = match child.stdout.take() {
        Some(pipe) if capture.stdout.is_some() => {
            Some(spawn_reader(pipe, capture.stdout.unwrap_or(0)))
        }
        _ => None,
    };
    let stderr = match child.stderr.take() {
        Some(pipe) if capture.stderr.is_some() => {
            Some(spawn_reader(pipe, capture.stderr.unwrap_or(0)))
        }
        _ => None,
    };
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                terminate_tree(&mut child);
                reap_bounded(child);
                return (ProcOutcome::Timeout, InputDelivery::Failed);
            }
            Ok(None) => thread::sleep(
                Duration::from_millis(10).min(deadline.saturating_duration_since(Instant::now())),
            ),
            Err(_) => {
                terminate_tree(&mut child);
                reap_bounded(child);
                return (ProcOutcome::Wait, InputDelivery::Failed);
            }
        }
    };

    // Do not join the feeder: a descendant can retain stdin forever. Its result
    // is observed only until this invocation's original deadline.
    let input_delivery = match &feeder {
        Some(receiver) => {
            match receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(Ok(())) => InputDelivery::Complete,
                Ok(Err(_)) | Err(mpsc::RecvTimeoutError::Disconnected) => InputDelivery::Failed,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    terminate_group_after_exit(&child);
                    InputDelivery::Failed
                }
            }
        }
        None => InputDelivery::Complete,
    };

    // Keep the deadline active after the leader exits: an inherited pipe may
    // still be open in a descendant, so EOF is not evidence of leader exit.
    let stdout_result = receive_reader(&stdout, deadline);
    if stdout_result.is_err() {
        terminate_group_after_exit(&child);
        let stdout = receive_reader(&stdout, Instant::now() + READER_REAP_BUDGET);
        let stderr = receive_reader(&stderr, Instant::now() + READER_REAP_BUDGET);
        return (
            match (stdout, stderr) {
                (Ok(stdout), Ok(stderr)) => ProcOutcome::Exited {
                    status,
                    stdout,
                    stderr,
                },
                (Err(error), _) | (_, Err(error)) => error,
            },
            input_delivery,
        );
    }
    let stdout = match stdout_result {
        Ok(result) => result,
        Err(error) => return (error, input_delivery),
    };
    let stderr_result = receive_reader(&stderr, deadline);
    if stderr_result.is_err() {
        terminate_group_after_exit(&child);
        let stderr = receive_reader(&stderr, Instant::now() + READER_REAP_BUDGET);
        return (
            match stderr {
                Ok(stderr) => ProcOutcome::Exited {
                    status,
                    stdout,
                    stderr,
                },
                Err(error) => error,
            },
            input_delivery,
        );
    }
    let stderr = match stderr_result {
        Ok(result) => result,
        Err(error) => return (error, input_delivery),
    };
    (
        ProcOutcome::Exited {
            status,
            stdout,
            stderr,
        },
        input_delivery,
    )
}

fn spawn_feeder(mut stdin: ChildStdin, input: Vec<u8>) -> Receiver<io::Result<()>> {
    let (sender, receiver) = mpsc::sync_channel(1);
    crate::thread_name::spawn_named("thegn-sbx-feed", move || {
        let result = stdin.write_all(&input);
        if let Err(error) = sender.send(result) {
            tracing::debug!(target: "thegn::sandbox", %error, "stdin feeder result receiver closed");
        }
    });
    receiver
}

fn spawn_reader<R: Read + Send + 'static>(mut reader: R, cap: usize) -> Receiver<ReadOutcome> {
    let (sender, receiver) = mpsc::sync_channel(1);
    crate::thread_name::spawn_named("thegn-sbx-read", move || {
        let mut captured = Captured {
            bytes: Vec::with_capacity(cap.min(4096)),
            truncated: false,
        };
        let mut buffer = [0u8; 8192];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => {
                    let room = cap.saturating_sub(captured.bytes.len());
                    let keep = room.min(count);
                    captured.bytes.extend_from_slice(&buffer[..keep]);
                    captured.truncated |= keep < count;
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => {
                    Result::ok(sender.send(ReadOutcome::Failed));
                    return;
                }
            }
        }
        Result::ok(sender.send(ReadOutcome::Captured(captured)));
    });
    receiver
}

fn receive_reader(
    receiver: &Option<Receiver<ReadOutcome>>,
    deadline: Instant,
) -> Result<Captured, ProcOutcome> {
    let Some(receiver) = receiver else {
        return Ok(Captured::default());
    };
    match receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(ReadOutcome::Captured(captured)) => Ok(captured),
        Ok(ReadOutcome::Failed) => Err(ProcOutcome::Read),
        Err(mpsc::RecvTimeoutError::Timeout) => Err(ProcOutcome::Timeout),
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(ProcOutcome::Read),
    }
}

fn terminate_tree(child: &mut Child) {
    #[cfg(unix)]
    {
        use nix::sys::signal::{Signal, killpg};
        use nix::unistd::Pid;
        Result::ok(killpg(Pid::from_raw(child.id() as i32), Signal::SIGKILL));
    }
    #[cfg(windows)]
    Result::ok(child.kill());
}

pub(crate) fn terminate_group_after_exit(_child: &Child) {
    #[cfg(unix)]
    {
        use nix::sys::signal::{Signal, killpg};
        use nix::unistd::Pid;
        Result::ok(killpg(Pid::from_raw(_child.id() as i32), Signal::SIGKILL));
    }
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
/// If the reaper is unavailable or full, dropping the handle leaves the OS to
/// reap the process when this process exits; shutdown itself remains bounded.
pub(crate) fn reap_event_child(child: Child) {
    match reaper_sender() {
        Some(sender) => match sender.try_send(child) {
            Ok(()) => {}
            Err(TrySendError::Full(child)) | Err(TrySendError::Disconnected(child)) => {
                tracing::debug!(
                    "sandbox events: detached reaper unavailable; leaving child to init"
                );
                drop(child);
            }
        },
        None => {
            tracing::debug!("sandbox events: detached reaper unavailable; leaving child to init");
            drop(child);
        }
    }
}

fn reap_bounded(mut child: Child) {
    let deadline = Instant::now() + KILL_REAP_BUDGET;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Err(_) => break,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            Ok(None) => break,
        }
    }
    match reaper_sender() {
        Some(sender) => match sender.try_send(child) {
            Ok(()) => {}
            Err(TrySendError::Full(mut child)) | Err(TrySendError::Disconnected(mut child)) => {
                // A saturated reaper is exceptional. Preserve ownership and
                // wait here rather than dropping a live Child handle.
                Result::ok(child.wait());
            }
        },
        None => {
            Result::ok(child.wait());
        }
    }
}

fn reaper_sender() -> Option<&'static SyncSender<Child>> {
    static REAPER: OnceLock<Option<SyncSender<Child>>> = OnceLock::new();
    REAPER
        .get_or_init(|| {
            let (sender, receiver) = mpsc::sync_channel::<Child>(64);
            thread::Builder::new()
                .spawn(move || {
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

    fn sh(script: &str) -> Vec<String> {
        vec!["sh".into(), "-c".into(), script.into()]
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
