//! Bounded execution of GPU helper commands (`nvidia-smi`, `ioreg`).
//!
//! A wedged helper (broken NVIDIA driver, a hostile earlier `PATH` entry) must
//! never stall the caller, grow memory, or leave a process behind. [`run_bounded`]
//! gives every run: a hard deadline, a stdout cap, its own process group that is
//! SIGKILLed as a unit on any non-clean exit, and a guaranteed `wait` on the
//! direct child before it returns (so the caller may treat "returned" as "reaped").
//!
//! Windows has no process-group kill in this crate: there the direct child is
//! killed and reaped; descendant containment waits on THE-274.

use std::ffi::OsStr;
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

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
}

/// How long to wait for the reader after the group has been killed before
/// abandoning it (only reachable if a descendant escaped the group and holds
/// the pipe; the reader thread is detached and ends when that process does).
const READER_GRACE: Duration = Duration::from_millis(500);

/// Run `program prefix_args.. args..` with `deadline` and a `cap`-byte stdout
/// limit. stdin and stderr are null. Returns stdout on a zero exit.
pub(crate) fn run_bounded(
    program: &OsStr,
    args: &[&OsStr],
    deadline: Duration,
    cap: usize,
) -> Result<Vec<u8>, ExecFailure> {
    let start = Instant::now();
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = cmd.spawn().map_err(|e| ExecFailure::Spawn {
        not_found: e.kind() == std::io::ErrorKind::NotFound,
    })?;
    let Some(stdout) = child.stdout.take() else {
        kill_tree(&mut child);
        let _ = child.wait(); // best-effort: already killing; reap only
        return Err(ExecFailure::Spawn { not_found: false });
    };

    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    let limit = cap as u64 + 1;
    let reader = std::thread::Builder::new()
        .name("thegn-gpu-read".into())
        .spawn(move || {
            let mut buf = Vec::new();
            // best-effort: a read error just ends capture with what we have
            let _ = stdout.take(limit).read_to_end(&mut buf);
            let _ = tx.send(buf); // best-effort: receiver may have timed out
        });
    if reader.is_err() {
        kill_tree(&mut child);
        let _ = child.wait(); // best-effort: reap after kill
        return Err(ExecFailure::Spawn { not_found: false });
    }

    let remaining = deadline.saturating_sub(start.elapsed());
    let outcome = match rx.recv_timeout(remaining) {
        Ok(buf) if buf.len() as u64 >= limit => Err(ExecFailure::OutputTooLarge),
        Ok(buf) => {
            // EOF seen: the child normally exits at once. Poll (bounded by the
            // deadline) so a closed-stdout-but-still-running helper is caught.
            wait_exit(&mut child, deadline.saturating_sub(start.elapsed())).map(|st| (st, buf))
        }
        Err(_) => Err(ExecFailure::Timeout),
    };
    match outcome {
        Ok((status, buf)) => {
            // Sweep any descendants the helper left in its group.
            kill_group_only(&child);
            if status.success() {
                Ok(buf)
            } else {
                Err(ExecFailure::Status(status.code()))
            }
        }
        Err(e) => {
            kill_tree(&mut child);
            let _ = child.wait(); // best-effort: SIGKILLed, returns promptly
            // Let the reader drain to EOF now the group is dead; never block on it.
            let _ = rx.recv_timeout(READER_GRACE);
            Err(e)
        }
    }
}

fn wait_exit(
    child: &mut std::process::Child,
    budget: Duration,
) -> Result<std::process::ExitStatus, ExecFailure> {
    let end = Instant::now() + budget;
    loop {
        match child.try_wait() {
            Ok(Some(st)) => return Ok(st),
            Ok(None) if Instant::now() < end => std::thread::sleep(Duration::from_millis(5)),
            Ok(None) | Err(_) => return Err(ExecFailure::Timeout),
        }
    }
}

fn kill_tree(child: &mut std::process::Child) {
    kill_group_only(child);
    let _ = child.kill(); // best-effort: covers non-unix and group-signal failure
}

#[cfg(unix)]
fn kill_group_only(child: &std::process::Child) {
    use nix::sys::signal::{Signal, killpg};
    use nix::unistd::Pid;
    if let Ok(pid) = i32::try_from(child.id()) {
        // The direct child is still unreaped here, so its pgid cannot be reused.
        let _ = killpg(Pid::from_raw(pid), Signal::SIGKILL); // best-effort: group may be empty
    }
}

#[cfg(not(unix))]
fn kill_group_only(_child: &std::process::Child) {}

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
    pub(crate) fn gone(pid: i32) -> bool {
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Err(_) => true,
            Ok(s) => s
                .rsplit(')')
                .next()
                .map(|t| t.trim_start().starts_with('Z'))
                .unwrap_or(true),
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::test_support::*;
    use super::*;

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
    fn timeout_kills_grandchildren() {
        let d = tempfile::tempdir().unwrap();
        let pidfile = d.path().join("gc.pid");
        let s = script(
            d.path(),
            "gc.sh",
            &format!("sleep 300 &\necho $! > {}\nsleep 300\n", pidfile.display()),
        );
        assert_eq!(sh(&s, 400, 1024), Err(ExecFailure::Timeout));
        let pid: i32 = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let end = Instant::now() + Duration::from_secs(3);
        while !gone(pid) && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(gone(pid), "grandchild {pid} survived the timeout");
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
        let end = Instant::now() + Duration::from_secs(3);
        while !gone(pid) && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(gone(pid), "descendant {pid} survived a clean exit");
    }
}
