//! Bounded execution of GPU helper commands (`nvidia-smi`, `ioreg`).
//!
//! A wedged helper (broken NVIDIA driver, a hostile earlier `PATH` entry) must
//! never stall the caller, grow memory, or leave a process behind. [`run_bounded`]
//! gives every run: a hard deadline, a stdout cap, its own process group that is
//! SIGKILLed as a unit on any non-clean exit, and a guaranteed `wait` on the
//! direct child before it returns (so the caller may treat "returned" as "reaped").
//!
//! Output is read on the calling thread with `poll(2)` against the deadline — no
//! reader thread, so nothing can leak if a descendant that escaped the group
//! keeps the pipe open (the pipe is simply closed when this returns).
//!
//! Process exit mid-probe: probes run on detached threads. If the whole process
//! exits while one is in flight, the helper (its own group) is not killed and
//! finishes or is orphaned to init; it holds no thegn resources.
//!
//! Windows has no process-group kill in this crate: there the direct child is
//! killed and reaped; descendant containment waits on THE-274.

use std::ffi::OsStr;
use std::process::{Child, ChildStdout, Command, Stdio};
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

/// Owns the child until it is waited: dropping an armed guard (early return,
/// panic) SIGKILLs the whole group and reaps the leader.
struct ChildGuard(Option<Child>);

impl ChildGuard {
    fn child(&mut self) -> &mut Child {
        self.0.as_mut().expect("guard holds the child until disarm")
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut c) = self.0.take() {
            kill_tree(&mut c);
            let _ = c.wait(); // best-effort: SIGKILLed, returns promptly
        }
    }
}

enum Capture {
    Eof(Vec<u8>),
    Overflow,
    Deadline,
}

/// Run `program args..` with `deadline` and a `cap`-byte stdout limit. stdin and
/// stderr are null. Returns stdout on a zero exit.
pub(crate) fn run_bounded(
    program: &OsStr,
    args: &[&OsStr],
    deadline: Duration,
    cap: usize,
) -> Result<Vec<u8>, ExecFailure> {
    let end = Instant::now() + deadline;
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
    let stdout = child.stdout.take();
    let mut guard = ChildGuard(Some(child));
    let Some(stdout) = stdout else {
        return Err(ExecFailure::Spawn { not_found: false });
    };

    let buf = match capture(stdout, end, cap) {
        Capture::Eof(buf) => buf,
        Capture::Overflow => return Err(ExecFailure::OutputTooLarge),
        Capture::Deadline => return Err(ExecFailure::Timeout),
    };
    // EOF seen: the leader normally exits at once. Observe that WITHOUT reaping
    // (the pgid stays pinned to our unreaped child), polling bounded by the
    // deadline.
    loop {
        match leader_exited(guard.child()) {
            Ok(true) => break,
            Ok(false) if Instant::now() < end => std::thread::sleep(Duration::from_millis(5)),
            Ok(false) => return Err(ExecFailure::Timeout),
            Err(_) => return Err(ExecFailure::Malformed),
        }
    }
    // Leader exited but is unreaped: sweep any descendants still in its group,
    // then reap. Never killpg after the reap — the pgid could be reused.
    kill_group_only(guard.child());
    let mut child = guard.0.take().expect("guard still armed");
    let status = child.wait().map_err(|_| ExecFailure::Malformed)?;
    if status.success() {
        Ok(buf)
    } else {
        Err(ExecFailure::Status(status.code()))
    }
}

#[cfg(unix)]
fn capture(mut stdout: ChildStdout, end: Instant, cap: usize) -> Capture {
    use std::io::Read;
    use std::os::fd::AsRawFd;
    let fd = stdout.as_raw_fd();
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let now = Instant::now();
        if now >= end {
            return Capture::Deadline;
        }
        // Round up so a sub-millisecond remainder cannot busy-spin at 0.
        let ms = (end - now)
            .as_millis()
            .saturating_add(1)
            .min(i32::MAX as u128) as i32;
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: `pfd` is a valid pollfd for the one entry we pass, and `fd`
        // stays open (owned by `stdout`) for the whole call.
        let rc = unsafe { libc::poll(&mut pfd, 1, ms) };
        if rc < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Capture::Deadline;
        }
        if rc == 0 {
            continue; // deadline re-checked at the top
        }
        // Readable (or hung up): one read cannot block.
        match stdout.read(&mut chunk) {
            Ok(0) => return Capture::Eof(buf),
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.len() > cap {
                    return Capture::Overflow;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return Capture::Eof(buf),
        }
    }
}

/// Non-unix: no `poll` on a pipe handle, so read on a helper thread and wait for
/// it against the deadline. (If a descendant holds the pipe the thread ends when
/// that process does.)
#[cfg(not(unix))]
fn capture(stdout: ChildStdout, end: Instant, cap: usize) -> Capture {
    use std::io::Read;
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let limit = cap as u64 + 1;
    let spawned = std::thread::Builder::new()
        .name("thegn-gpu-read".into())
        .spawn(move || {
            let mut buf = Vec::new();
            // best-effort: partial output is judged by its length below
            let _ = stdout.take(limit).read_to_end(&mut buf); // best-effort: a short read yields a partial or empty sample
            let _ = tx.send(buf); // best-effort: receiver may have timed out
        });
    if spawned.is_err() {
        return Capture::Deadline;
    }
    match rx.recv_timeout(end.saturating_duration_since(Instant::now())) {
        Ok(buf) if buf.len() as u64 >= limit => Capture::Overflow,
        Ok(buf) => Capture::Eof(buf),
        Err(_) => Capture::Deadline,
    }
}

/// Whether the direct child has exited, WITHOUT reaping it (`WNOWAIT`).
#[cfg(unix)]
fn leader_exited(child: &mut Child) -> std::io::Result<bool> {
    let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
    // SAFETY: waitid initializes siginfo on success; WNOWAIT leaves the child
    // waitable so its pid/pgid cannot be reused before we reap it.
    let rc = unsafe {
        libc::waitid(
            libc::P_PID,
            child.id() as libc::id_t,
            info.as_mut_ptr(),
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: successful waitid initialized the siginfo structure.
    Ok(unsafe { info.assume_init().si_pid() } != 0)
}

#[cfg(not(unix))]
fn leader_exited(child: &mut Child) -> std::io::Result<bool> {
    child.try_wait().map(|s| s.is_some())
}

fn kill_tree(child: &mut Child) {
    kill_group_only(child);
    let _ = child.kill(); // best-effort: covers non-unix and group-signal failure
}

/// SIGKILL the child's process group. Call only while the leader is unreaped.
#[cfg(unix)]
fn kill_group_only(child: &Child) {
    use nix::sys::signal::{Signal, killpg};
    use nix::unistd::Pid;
    if let Ok(pid) = i32::try_from(child.id()) {
        let _ = killpg(Pid::from_raw(pid), Signal::SIGKILL); // best-effort: group may be empty
    }
}

#[cfg(not(unix))]
fn kill_group_only(_child: &Child) {}

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
        assert_eq!(sh(&s, 1500, 1024), Err(ExecFailure::Timeout));
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
