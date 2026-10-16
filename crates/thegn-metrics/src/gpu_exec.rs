//! Bounded execution of GPU helper commands (`nvidia-smi`, `ioreg`).
//!
//! A wedged helper (broken NVIDIA driver, a hostile earlier `PATH` entry) must
//! never stall the caller, grow memory, or leave a process behind. [`run_bounded`]
//! uses `thegn_proc::run` for an owned process group, deadline, stdout cap, group
//! kill, and reap. On unix it reads pipes on the calling thread without a reader
//! thread; elsewhere containment is direct-child-only until THE-274.
//!
//! Process exit mid-probe: probes run on detached threads. If the whole process
//! exits while one is in flight, the helper (its own group) is not killed and
//! finishes or is orphaned to init; it holds no thegn resources.
//!
//! Windows has no process-group kill in this crate: there the direct child is
//! killed and reaped; descendant containment waits on THE-274.

use std::{ffi::OsStr, io, process::Command, sync::atomic::AtomicBool, time::Duration};
use thegn_proc::{End, Leader, Limits, Run, Stream};

/// Why a bounded run produced no output. Typed so health can say what happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExecFailure {
    /// The program could not be started (most commonly: not installed).
    Spawn { not_found: bool },
    /// Still running (or its pipe still open) at the deadline; killed.
    Timeout,
    /// Wrote more than the stdout cap; killed.
    OutputTooLarge,
    /// Exited unsuccessfully (`None` = terminated by signal).
    Status(Option<i32>),
    /// Output arrived but did not have the expected shape.
    Malformed,
    /// The probe thread panicked.
    Panicked,
}

impl std::fmt::Display for ExecFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawn { not_found: true } => write!(f, "not found"),
            Self::Spawn { not_found: false } => write!(f, "spawn failed"),
            Self::Timeout => write!(f, "timed out"),
            Self::OutputTooLarge => write!(f, "output too large"),
            Self::Status(Some(c)) => write!(f, "exit status {c}"),
            Self::Status(None) => write!(f, "exit by signal"),
            Self::Malformed => write!(f, "reap failed"),
            Self::Panicked => write!(f, "panicked"),
        }
    }
}

/// Run `program args..` with `deadline` and a `cap`-byte stdout limit. stdin and
/// stderr are null. Returns stdout on a zero exit.
pub(crate) fn run_bounded(
    program: &OsStr,
    args: &[&OsStr],
    deadline: Duration,
    cap: usize,
) -> Result<Vec<u8>, ExecFailure> {
    let mut cmd = Command::new(program);
    cmd.args(args);
    let cancel = AtomicBool::new(false);
    let limits = Limits {
        quiesce_after_leader: Some(Duration::ZERO),
        stdout: Stream::HeadOrFail { cap },
        reader_grace: Duration::from_millis(50),
        ..Limits::silent(deadline)
    };
    classify(thegn_proc::run(&mut cmd, &limits, &cancel))
}

/// Convert the shared runner's outcomes into the GPU probe's stable failure API.
fn classify(run: Run) -> Result<Vec<u8>, ExecFailure> {
    let stdout_eof = run.stdout.eof;
    match run.end {
        End::Spawn(error) => Err(ExecFailure::Spawn {
            not_found: error.kind() == io::ErrorKind::NotFound,
        }),
        End::OutputLimit(_) => Err(ExecFailure::OutputTooLarge),
        End::TimedOut => Err(ExecFailure::Timeout),
        // The GPU runner never sets its cancellation flag.
        End::Cancelled => Err(ExecFailure::Timeout),
        End::Supervise(_) => Err(ExecFailure::Malformed),
        End::LeaderExited if !stdout_eof => Err(ExecFailure::Timeout),
        End::LeaderExited => match run.leader {
            Leader::Reaped(status) if status.success() => Ok(run.stdout.bytes),
            Leader::Reaped(status) => Err(ExecFailure::Status(status.code())),
            Leader::ReapFailed(_) | Leader::Detached | Leader::NotSpawned => {
                Err(ExecFailure::Malformed)
            }
        },
    }
}

#[cfg(all(test, unix))]
pub(crate) mod test_support {
    use std::path::{Path, PathBuf};

    /// Write an `sh` script; returns its path. Run it as `sh <path>` (never
    /// exec the file directly — avoids ETXTBSY races between parallel tests).
    pub(crate) fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, body).unwrap();
        p
    }

    /// Whether `pid` is gone (or only a zombie awaiting an init that reaps late).
    #[cfg(target_os = "linux")]
    pub(crate) fn gone(pid: i32) -> bool {
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Err(_) => true,
            Ok(stat) => stat_state_is_gone(&stat),
        }
    }

    #[cfg(target_os = "linux")]
    pub(super) fn stat_state_is_gone(stat: &str) -> bool {
        stat.rsplit(')')
            .next()
            .and_then(|tail| tail.trim_start().chars().next())
            .map(|state| matches!(state, 'Z' | 'X' | 'x'))
            .unwrap_or(true)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::test_support::*;
    use super::*;
    use std::os::unix::process::ExitStatusExt;
    use std::time::Instant;
    use thegn_proc::{Capture, Containment, GroupState};

    fn synthetic_run(end: End, leader: Leader, stdout: Capture) -> Run {
        Run {
            end,
            group: GroupState::Drained,
            leader,
            leader_exited_first: true,
            stdout,
            stderr: Capture::default(),
            containment: Containment::ProcessGroup,
        }
    }

    fn stdout(bytes: &[u8], eof: bool) -> Capture {
        Capture {
            bytes: bytes.to_vec(),
            eof,
            ..Capture::default()
        }
    }

    #[test]
    fn classify_open_pipe_after_leader_exit_is_timeout() {
        let run = synthetic_run(
            End::LeaderExited,
            Leader::Reaped(std::process::ExitStatus::from_raw(0)),
            stdout(b"partial", false),
        );
        assert_eq!(classify(run), Err(ExecFailure::Timeout));
    }

    #[test]
    fn classify_unreapable_leader_is_malformed() {
        for leader in [
            Leader::Detached,
            Leader::ReapFailed(io::Error::other("wait failed")),
        ] {
            let run = synthetic_run(End::LeaderExited, leader, stdout(b"", true));
            assert_eq!(classify(run), Err(ExecFailure::Malformed));
        }
    }

    #[test]
    fn classify_supervise_error_is_malformed() {
        let run = synthetic_run(
            End::Supervise(io::Error::other("waitid failed")),
            Leader::Detached,
            stdout(b"", false),
        );
        assert_eq!(classify(run), Err(ExecFailure::Malformed));
    }

    #[test]
    fn classify_nonzero_and_signal_status() {
        let exited = synthetic_run(
            End::LeaderExited,
            Leader::Reaped(std::process::ExitStatus::from_raw(3 << 8)),
            stdout(b"", true),
        );
        assert_eq!(classify(exited), Err(ExecFailure::Status(Some(3))));

        let signaled = synthetic_run(
            End::LeaderExited,
            Leader::Reaped(std::process::ExitStatus::from_raw(9)),
            stdout(b"", true),
        );
        assert_eq!(classify(signaled), Err(ExecFailure::Status(None)));
    }

    #[test]
    fn classify_success_returns_stdout_bytes() {
        let run = synthetic_run(
            End::LeaderExited,
            Leader::Reaped(std::process::ExitStatus::from_raw(0)),
            stdout(b"gpu data", true),
        );
        assert_eq!(classify(run), Ok(b"gpu data".to_vec()));
    }

    fn sh(script: &std::path::Path, deadline_ms: u64, cap: usize) -> Result<Vec<u8>, ExecFailure> {
        run_bounded(
            OsStr::new("sh"),
            &[script.as_os_str()],
            Duration::from_millis(deadline_ms),
            cap,
        )
    }

    #[test]
    fn ok_returns_stdout() {
        let d = tempfile::tempdir().unwrap();
        let s = script(d.path(), "ok.sh", "echo hi\n");
        assert_eq!(sh(&s, 5000, 1024).unwrap(), b"hi\n");
    }

    #[test]
    fn missing_program_is_typed_not_found() {
        let r = run_bounded(
            OsStr::new("/nonexistent/thegn-gpu-helper"),
            &[],
            Duration::from_secs(1),
            16,
        );
        assert_eq!(r, Err(ExecFailure::Spawn { not_found: true }));
    }

    #[test]
    fn hang_times_out_and_is_killed_promptly() {
        let d = tempfile::tempdir().unwrap();
        let s = script(d.path(), "hang.sh", "sleep 300\n");
        let t = Instant::now();
        assert_eq!(sh(&s, 200, 1024), Err(ExecFailure::Timeout));
        assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
    }

    #[test]
    fn hang_returns_within_deadline_plus_settle() {
        let d = tempfile::tempdir().unwrap();
        let s = script(d.path(), "bounded-hang.sh", "sleep 300\n");
        let deadline = Duration::from_millis(300);
        let started = Instant::now();
        assert_eq!(sh(&s, 300, 1024), Err(ExecFailure::Timeout));
        assert!(
            started.elapsed() < deadline + Duration::from_secs(2),
            "elapsed {:?} exceeded deadline plus settle slack",
            started.elapsed()
        );
    }

    #[test]
    fn infinite_output_is_capped_and_killed() {
        let d = tempfile::tempdir().unwrap();
        let s = script(d.path(), "yes.sh", "yes\n");
        let t = Instant::now();
        assert_eq!(sh(&s, 10_000, 4096), Err(ExecFailure::OutputTooLarge));
        assert!(t.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn nonzero_status_is_typed() {
        let d = tempfile::tempdir().unwrap();
        let s = script(d.path(), "bad.sh", "echo partial; exit 3\n");
        assert_eq!(sh(&s, 5000, 1024), Err(ExecFailure::Status(Some(3))));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn proc_stat_parser_recognizes_zombie_and_dead_states() {
        assert!(test_support::stat_state_is_gone("123 (a) b) Z 1 ..."));
        assert!(test_support::stat_state_is_gone("123 (a) b) X 1 ..."));
        assert!(test_support::stat_state_is_gone("123 (a) b) x 1 ..."));
        assert!(!test_support::stat_state_is_gone("123 (a) b) S 1 ..."));
        assert!(test_support::stat_state_is_gone(""));
    }

    // SIGKILL-to-exit can exceed a few seconds on a CPU-capped host under swap.
    #[cfg(target_os = "linux")]
    const REAP_GRACE: Duration = Duration::from_secs(15);

    #[cfg(target_os = "linux")]
    fn assert_pid_gone(pid: i32, context: &str) {
        if !gone(pid) {
            let last_stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .unwrap_or_else(|error| format!("<unavailable: {error}>"));
            panic!("{context}: process {pid} remained; last /proc/{pid}/stat: {last_stat}");
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn timeout_kills_grandchildren() {
        let d = tempfile::tempdir().unwrap();
        let pidfile = d.path().join("gc.pid");
        let s = script(
            d.path(),
            "gc.sh",
            &format!("sleep 300 &\necho $! > {}\nsleep 300\n", pidfile.display()),
        );
        assert_eq!(sh(&s, 1500, 1024), Err(ExecFailure::Timeout));
        let pid: i32 = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let end = Instant::now() + REAP_GRACE;
        while !gone(pid) && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_pid_gone(pid, "grandchild survived the timeout");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn clean_exit_sweeps_backgrounded_descendant() {
        let d = tempfile::tempdir().unwrap();
        let pidfile = d.path().join("bg.pid");
        // Descendant detaches stdout so EOF arrives; helper itself exits 0.
        let s = script(
            d.path(),
            "bg.sh",
            &format!(
                "sleep 300 >/dev/null 2>&1 &\necho $! > {}\necho ok\n",
                pidfile.display()
            ),
        );
        assert_eq!(sh(&s, 5000, 1024).unwrap(), b"ok\n");
        let pid: i32 = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let end = Instant::now() + REAP_GRACE;
        while !gone(pid) && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_pid_gone(pid, "descendant survived a clean exit");
    }
}
