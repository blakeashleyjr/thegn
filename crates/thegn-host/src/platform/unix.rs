//! Unix impls of the platform seam: real fds, signals, and process groups.

use std::io;
use std::process::{Child, Command};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

// Run the vendored dependency's real PTY regressions in the normal host test
// target too: Cargo cannot select a patched non-workspace dependency's tests.
#[cfg(test)]
mod termwiz_regression {
    use std::os::fd::AsRawFd;
    use termwiz::caps::{Capabilities, ProbeHints};
    use termwiz::terminal::{Terminal, UnixTerminal};

    mod drop_tests {
        include!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../vendor/termwiz/src/terminal/unix_drop_tests.rs"
        ));
    }
}

/// Restores the original stderr fd on drop (see [`super::redirect_stderr_to_logfile`]).
///
/// Holds an `OwnedFd` rather than a `RawFd`: nix 0.31 moved the fd API to
/// `AsFd`/`OwnedFd`, and owning it means the saved descriptor is closed by its
/// own `Drop` instead of a hand-written `close` that a early return could skip.
pub struct StderrGuard {
    saved: std::os::fd::OwnedFd,
}

impl Drop for StderrGuard {
    fn drop(&mut self) {
        // Restore fd 2 from the copy; `saved` then closes itself.
        nix::unistd::dup2_stderr(&self.saved).ok(); // best-effort: stderr restore in Drop: failure cannot unwind (crash path)
    }
}

impl StderrGuard {
    /// Hand the panic hook a dup of the ORIGINAL (pre-redirect) stderr so its
    /// one-line crash notice reaches the user's terminal even though fd 2 now
    /// points at the log file. Best-effort; the write in the closure ignores its
    /// result so it can run during a panic unwind without risking a re-panic.
    pub fn register_crash_notice(&self) {
        if let Ok(fd) = nix::unistd::dup(&self.saved) {
            thegn_core::log_trace::register_crash_notice(move |s: &str| {
                let _ = nix::unistd::write(&fd, s.as_bytes()); // best-effort: crash notice: best-effort write during a panic; failure loses the notice
            });
        }
    }
}

/// A panic-safe terminal restorer: an owned fd to the controlling terminal plus
/// the saved *cooked* termios captured before raw mode. [`TerminalRestore::restore`]
/// uses only non-panicking writes / a raw `libc::tcsetattr` — never a termwiz
/// method that can `unwrap` during unwind — so the panic hook can call it while
/// the original panic is unwinding without risking a double panic.
///
/// The termios is stored as the raw `libc::termios` (a plain `Copy` C struct)
/// rather than nix's `Termios`, whose internal `RefCell` is not `Sync` and so
/// could not be shared into the `Fn() + Send + Sync` restore callback.
pub struct TerminalRestore {
    tty: std::os::fd::OwnedFd,
    cooked: libc::termios,
}

impl TerminalRestore {
    pub fn restore(&self) {
        use std::os::fd::AsRawFd;
        // Mouse reporting off (1006/1002), autowrap back on (?7h), reset
        // modifyOtherKeys (>4m), pop the kitty keyboard flags (<u), cursor
        // visible (?25h), leave the alternate screen (?1049l) — the same
        // teardown the normal path writes — then restore cooked mode from the
        // saved termios directly on the tty fd.
        //
        // `\x1b[>4m` is XTMODKEYS with the value omitted, which resets the
        // resource to the terminal's initial value. Without it a panic leaves
        // the user's shell in `modifyOtherKeys = 2`, where readline sees CSI-u
        // sequences it cannot parse — the normal path gets this for free from
        // termwiz's `set_cooked_mode()`, the panic path has no termwiz.
        //
        // `\x1b[<u` pops the kitty keyboard stack even though thegn never
        // pushes it (`run.rs`'s keyboard comment says why). That is deliberate:
        // popping an empty stack is a documented no-op in the kitty spec, and
        // it is defensive against an inner app that pushed flags and died
        // without popping them. Leave the bytes.
        const SEQ: &[u8] = b"\x1b[?1006l\x1b[?1002l\x1b[?7h\x1b[>4m\x1b[<u\x1b[?25h\x1b[?1049l";
        let _ = nix::unistd::write(&self.tty, SEQ); // best-effort: terminal teardown: leave the bytes; the tty drops anyway
        // SAFETY: `tcsetattr` on our own controlling-terminal fd with a termios
        // we captured from it. Result ignored — this runs during a panic unwind
        // and must not itself panic.
        unsafe {
            libc::tcsetattr(self.tty.as_raw_fd(), libc::TCSANOW, &self.cooked);
        }
    }
}

/// Capture the controlling terminal's current (cooked) state so the panic hook
/// can restore it. Call BEFORE entering raw mode + the alternate screen. `None`
/// if `/dev/tty` cannot be opened or queried (the caller then relies on the
/// normal teardown path).
pub fn capture_terminal_restore() -> Option<TerminalRestore> {
    use std::os::fd::AsRawFd;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .ok()?;
    let tty: std::os::fd::OwnedFd = file.into();
    // SAFETY: `tcgetattr` into a zeroed termios on a valid open fd.
    let mut cooked: libc::termios = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::tcgetattr(tty.as_raw_fd(), &mut cooked) };
    if rc != 0 {
        return None;
    }
    Some(TerminalRestore { tty, cooked })
}

/// Point fd 2 at `file`, saving the original for the guard's `Drop`.
pub(super) fn redirect_stderr_to(file: std::fs::File) -> Option<StderrGuard> {
    let saved = nix::unistd::dup(std::io::stderr()).ok()?;
    if nix::unistd::dup2_stderr(&file).is_err() {
        // `saved` drops here, closing the copy we no longer need.
        return None;
    }
    Some(StderrGuard { saved })
}

/// Is a process with this pid alive (signal-0 probe)?
pub fn pid_alive(pid: i64) -> bool {
    pid > 0
        && pid <= i64::from(i32::MAX)
        && nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None).is_ok()
}

/// Best-effort graceful termination of a single process (`SIGTERM`).
pub fn terminate_pid(pid: u32) {
    let Some(pid) = checked_positive_pid(pid) else {
        return;
    };
    // best-effort: signal: the process may already be gone
    nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGTERM).ok();
}

/// Read Linux's process-start identity (`/proc/<pid>/stat`, field 22).
///
/// The `comm` field is parenthesized and may contain spaces or `)`, so parse
/// the numeric fields only after the final closing parenthesis. A missing or
/// malformed identity is an intentional fail-closed result.
pub fn proxy_process_start_time(pid: u32) -> Option<u64> {
    checked_positive_pid(pid)?;
    #[cfg(target_os = "linux")]
    {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        parse_proc_start_time(&stat)
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

#[cfg(target_os = "linux")]
fn parse_proc_start_time(stat: &str) -> Option<u64> {
    // Field 3 (state) is the first token after field 2 (comm); field 22 is
    // therefore token 19 in this suffix.
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(19)?
        .parse()
        .ok()
}

// The fixture reaps (`Child::wait`) are a blocking child wait, banned in this
// crate so the event loop can never stall on a subprocess. A unit test provably
// never runs on the loop, and leaving a killed `sleep` unreaped would leak a
// zombie for the rest of the test binary.
#[expect(clippy::disallowed_methods)]
#[cfg(all(test, target_os = "linux"))]
mod proxy_pid_tests {
    use super::*;

    #[test]
    fn parses_start_time_after_a_parenthesized_comm_with_spaces_and_parens() {
        let suffix = std::iter::once("S")
            .chain(std::iter::repeat_n("0", 18))
            .chain(std::iter::once("987654"))
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(
            parse_proc_start_time(&format!("7 (proxy ) with spaces) {suffix}")),
            Some(987654)
        );
    }

    #[test]
    fn invalid_proc_stat_and_missing_identity_fail_closed() {
        assert_eq!(parse_proc_start_time("7 (proxy) S too-short"), None);
        assert_eq!(proxy_process_start_time(0), None);
        assert!(!pid_alive(i64::from(u32::MAX)));
        assert!(!terminate_proxy_pid(u32::MAX, Some(1)));
        assert!(!terminate_proxy_pid(0, Some(1)));
    }

    #[test]
    fn mismatched_start_time_leaves_live_fixture_untouched() {
        let mut fixture = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn harmless fixture");
        let pid = fixture.id();
        let actual = proxy_process_start_time(pid).expect("fixture identity");
        assert!(!terminate_proxy_pid(pid, Some(actual.saturating_add(1))));
        assert!(pid_alive(i64::from(pid)));
        let _ = fixture.kill();
        let _ = fixture.wait();
    }

    #[test]
    fn matching_start_time_allows_term_for_the_expected_fixture() {
        let mut fixture = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn harmless fixture");
        let pid = fixture.id();
        let actual = proxy_process_start_time(pid).expect("fixture identity");
        assert!(terminate_proxy_pid(pid, Some(actual)));
        let _ = fixture.wait();
    }

    #[test]
    fn vanished_fixture_is_not_signalled() {
        let mut fixture = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn harmless fixture");
        let pid = fixture.id();
        let expected = proxy_process_start_time(pid).expect("fixture identity");
        let _ = fixture.kill();
        let _ = fixture.wait();
        assert!(!terminate_proxy_pid(pid, Some(expected)));
    }
}

fn checked_positive_pid(pid: u32) -> Option<nix::unistd::Pid> {
    (pid != 0 && pid <= i32::MAX as u32).then(|| nix::unistd::Pid::from_raw(pid as i32))
}

/// Send SIGTERM only when the current process has the recorded start identity.
/// The range check happens before the `u32 -> i32` conversion, and identity is
/// re-read in this seam immediately before the syscall. No process-group
/// semantics are reachable from stored state.
pub fn terminate_proxy_pid(pid: u32, expected_start_time: Option<u64>) -> bool {
    let raw_pid = pid;
    let Some(pid) = checked_positive_pid(raw_pid) else {
        return false;
    };
    let Some(expected) = expected_start_time else {
        return false;
    };
    if proxy_process_start_time(raw_pid) != Some(expected) {
        return false;
    }
    nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGTERM).is_ok()
}

/// Deliver `sig` to `pid`, surfacing the outcome. Unlike [`terminate_pid`] the
/// result is returned rather than swallowed, so the monitor's Processes tab can
/// show a `no such process` / `permission denied` failure instead of pretending
/// the signal landed. Refuses pid 0 (`kill(0, …)` would hit the whole process
/// group — never the intent of a single-row action).
pub fn signal_pid(pid: u32, sig: super::ProcSignal) -> Result<(), String> {
    use nix::errno::Errno;
    // Guard the `as i32` below: pid 0 is the caller's process group, and any pid
    // past `i32::MAX` casts to a NEGATIVE i32 — `kill(-N, …)` signals a whole
    // process group. Neither is ever a single-process target, and a real Linux
    // pid never exceeds `i32::MAX`, so both are refused outright.
    let Some(pid) = checked_positive_pid(pid) else {
        return Err("invalid pid".into());
    };
    let signal = match sig {
        super::ProcSignal::Terminate => nix::sys::signal::Signal::SIGTERM,
        super::ProcSignal::Kill => nix::sys::signal::Signal::SIGKILL,
    };
    nix::sys::signal::kill(pid, signal).map_err(|e| match e {
        Errno::ESRCH => "no such process".to_string(),
        Errno::EPERM => "permission denied".to_string(),
        other => other.to_string(),
    })
}

/// Create a fresh file readable/writable only by the owner (mode `0600`),
/// truncating any prior contents. Session recordings are terminal output and
/// can contain secrets echoed by tools, so their `.cast` files must never be
/// group/world readable.
pub fn create_private_file(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
}

/// Open a log for owner-only append. Hook output can contain credentials even
/// when the hook itself was configured by a trusted user, so do not rely on
/// the process umask for this file.
pub fn append_private_file(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::fs::PermissionsExt;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)?;
    // Tighten logs created by older versions too.
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

/// Restrict a directory to owner-only access and preserve any chmod failure for
/// callers that need to report an incomplete security boundary.
pub fn restrict_dir_owner_only_checked(path: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}

/// Publish a shell wrapper atomically and make both the staged and published
/// file owner-executable. The portable caller owns the script contents; this
/// seam owns Unix mode bits.
pub(crate) fn publish_private_executable(
    temporary: &std::path::Path,
    path: &std::path::Path,
    contents: &[u8],
) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o700)
            .open(temporary)?;
        file.write_all(contents)?;
        file.set_permissions(std::fs::Permissions::from_mode(0o700))?;
        drop(file);
        std::fs::rename(temporary, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    result
}

/// Report the native identity and filesystem hardening behind a Unix-domain
/// local control endpoint.
pub(crate) fn local_control_security(path: &std::path::Path) -> super::LocalControlSecurity {
    let (hardening, error) = if path.exists() {
        match thegn_svc::ipc::control_endpoint_hardening(path) {
            Ok(()) => ("ok", None),
            Err(error) => ("failed", Some(error.to_string())),
        }
    } else {
        ("endpoint-not-running", None)
    };
    super::LocalControlSecurity {
        auth: "same-euid-or-token",
        peer_identity: "native-effective-uid",
        hardening,
        error,
    }
}

/// Open an existing path without following a symlink in its final component.
/// Callers must validate the returned handle's metadata before consuming it.
pub fn open_nofollow(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        // A metadata check after `open` is too late for FIFOs: opening one for
        // reading can block forever before the caller can reject it as a
        // non-regular file. `O_NONBLOCK` is inert for regular files and keeps
        // hostile/racy special files on the normal error path.
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)
}

/// Pin a CLI file identity while following ordinary executable symlinks.
/// Reject replacement with a special file without blocking on FIFO input.
pub(crate) fn open_capability_identity(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::other(
            "capability identity is not a regular file",
        ));
    }
    Ok(file)
}

/// Directory-identity opening for automatic cleanup; callers verify type.
pub fn open_directory_nofollow(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    open_nofollow(path)
}

#[cfg(test)]
pub fn symlink_file_for_test(
    original: &std::path::Path,
    link: &std::path::Path,
) -> std::io::Result<()> {
    std::os::unix::fs::symlink(original, link)
}

/// Restrict a directory to owner-only access (mode `0700`). Best-effort — a
/// failure hardens less but must not stop recording.
pub fn restrict_dir_owner_only(path: &std::path::Path) {
    // best-effort: 0700 is defence-in-depth; the dir is already under the
    // per-profile state root.
    let _ = restrict_dir_owner_only_checked(path); // best-effort: hardening: a failed chmod must never block the caller
}

/// A spawned child's process group — what [`GroupHandle::terminate`] reaps
/// (e.g. a `cargo test` and every test binary it spawned) in one call.
#[derive(Clone)]
pub struct GroupHandle {
    pgid: i32,
}

impl GroupHandle {
    /// A handle over an already-known pid/pgid — for tests and callers that
    /// track pids themselves (the PTY pane's `Drop` reap, which only ever has
    /// the pid). (On Windows this is also the degraded no-job path, so it's
    /// part of the seam's shared API.)
    pub fn from_pid(pid: i32) -> Self {
        Self { pgid: pid }
    }

    /// Best-effort `SIGTERM` to the whole group.
    pub fn terminate(&self) {
        // best-effort: signal: the process may already be gone
        nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(self.pgid),
            nix::sys::signal::Signal::SIGTERM,
        )
        .ok();
    }
    /// Forcefully terminate the whole process group.
    pub fn kill(&self) {
        // best-effort: signal: the process may already be gone
        nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(self.pgid),
            nix::sys::signal::Signal::SIGKILL,
        )
        .ok();
    }
}

/// Spawn `cmd` in its own process group (Job Object on Windows) and return the
/// child plus the group handle that reaps the whole tree.
pub fn spawn_grouped(cmd: &mut Command) -> std::io::Result<(std::process::Child, GroupHandle)> {
    use std::os::unix::process::CommandExt;
    cmd.process_group(0);
    let child = cmd.spawn()?;
    let pgid = child.id() as i32;
    Ok((child, GroupHandle { pgid }))
}

/// Spawn a native clipboard helper in its own process group.
pub fn spawn_clipboard_helper(
    cmd: &mut Command,
) -> std::io::Result<(std::process::Child, GroupHandle)> {
    spawn_grouped(cmd)
}

/// A desktop helper together with the process-group identity created for it.
/// The direct child stays owned here until group cleanup has completed and the
/// child has been waited, so callers cannot accidentally signal a reused PGID.
pub struct DesktopChild {
    child: Child,
    pgid: i32,
    state: DesktopChildState,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DesktopChildState {
    Live,
    ObservedExited,
    Reaped,
    Uncertain,
}

impl DesktopChild {
    /// Spawn a notifier in a fresh process group.
    pub fn spawn(cmd: &mut Command) -> io::Result<Self> {
        use std::os::unix::process::CommandExt;

        cmd.process_group(0);
        let child = cmd.spawn()?;
        let pgid = child.id() as i32;
        Ok(Self {
            child,
            pgid,
            state: DesktopChildState::Live,
        })
    }

    /// Observe exit without consuming the wait status. A `true` result keeps
    /// the leader unreaped so its process group remains owned for cleanup.
    pub fn poll_exit(&mut self) -> io::Result<bool> {
        match self.state {
            DesktopChildState::ObservedExited => return Ok(true),
            DesktopChildState::Reaped => {
                return Err(io::Error::other("desktop child was already reaped"));
            }
            DesktopChildState::Uncertain => {
                return Err(io::Error::other(
                    "desktop child wait ownership is uncertain",
                ));
            }
            DesktopChildState::Live => {}
        }
        // SAFETY: zeroed siginfo is valid output storage; WNOHANG avoids a
        // blocking wait and WNOWAIT retains the direct-child identity.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                self.child.id() as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if result < 0 {
            self.state = DesktopChildState::Uncertain;
            return Err(io::Error::last_os_error());
        }
        let exited = unsafe { info.si_pid() } != 0;
        if exited {
            self.state = DesktopChildState::ObservedExited;
        }
        Ok(exited)
    }

    /// Terminate the owned group before reaping the direct child. This is also
    /// used after a normal exit: the unreaped leader keeps the original group
    /// identity available long enough to stop surviving descendants.
    #[expect(
        clippy::disallowed_methods,
        reason = "owned child wait runs only on the dedicated desktop dispatcher thread"
    )]
    pub fn terminate_and_wait(&mut self) -> io::Result<()> {
        let exited = match self.state {
            DesktopChildState::Live => self.poll_exit()?,
            DesktopChildState::ObservedExited => true,
            DesktopChildState::Reaped => {
                return Err(io::Error::other("desktop child was already reaped"));
            }
            DesktopChildState::Uncertain => {
                return Err(io::Error::other(
                    "desktop child wait ownership is uncertain",
                ));
            }
        };
        let group_error = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(self.pgid),
            nix::sys::signal::Signal::SIGKILL,
        )
        .err()
        .map(io::Error::from);
        let child_error = if exited {
            None
        } else {
            self.child.kill().err()
        };
        let wait_result = self.child.wait();
        match wait_result {
            Ok(_) => self.state = DesktopChildState::Reaped,
            Err(error) => {
                self.state = DesktopChildState::Uncertain;
                return Err(error);
            }
        }
        if let Some(error) = group_error
            && error.raw_os_error() != Some(libc::ESRCH)
        {
            return Err(error);
        }
        if let Some(error) = child_error
            && error.raw_os_error() != Some(libc::ESRCH)
        {
            return Err(error);
        }
        Ok(())
    }

    /// Reap only through the still-owned direct child. Used by quarantine;
    /// this method never signals a process group after ownership is uncertain.
    #[expect(
        clippy::disallowed_methods,
        reason = "owned child wait runs only on the dedicated desktop dispatcher thread"
    )]
    pub fn reap_owned(&mut self) -> io::Result<()> {
        if self.state == DesktopChildState::Reaped {
            return Err(io::Error::other("desktop child was already reaped"));
        }
        match self.child.wait() {
            Ok(_) => {
                self.state = DesktopChildState::Reaped;
                Ok(())
            }
            Err(error) => {
                self.state = DesktopChildState::Uncertain;
                Err(error)
            }
        }
    }

    #[cfg(test)]
    fn mark_wait_uncertain_for_test(&mut self) {
        self.state = DesktopChildState::Uncertain;
    }
}

#[cfg(test)]
mod desktop_child_tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn observed_exit_keeps_group_owned_until_cleanup_and_reap() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "exit 0"]);
        let mut child = DesktopChild::spawn(&mut command).unwrap();
        for _ in 0..80 {
            if child.poll_exit().unwrap() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(child.poll_exit().unwrap());
        child.terminate_and_wait().unwrap();
        assert!(child.poll_exit().is_err());
        assert!(child.terminate_and_wait().is_err());
    }

    #[test]
    fn natural_leader_exit_cleans_held_descendant_before_reap() {
        let fixture = tempfile::tempdir().unwrap();
        let started_file = fixture.path().join("descendant.started");
        let survivor_file = fixture.path().join("descendant.survived");
        let script = format!(
            "(echo started > {}; sleep 2; echo survivor > {}) & exit 0",
            started_file.display(),
            survivor_file.display()
        );
        let mut command = Command::new("/bin/sh");
        command.args(["-c", &script]);
        let mut child = DesktopChild::spawn(&mut command).unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        while !started_file.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(started_file.exists());
        while !child.poll_exit().unwrap() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(child.poll_exit().unwrap());
        child.terminate_and_wait().unwrap();
        std::thread::sleep(Duration::from_millis(2_200));
        assert!(
            !survivor_file.exists(),
            "held descendant survived process-group cleanup"
        );
    }

    #[test]
    #[expect(
        clippy::disallowed_methods,
        reason = "reap the private fixture child after testing uncertain ownership"
    )]
    fn uncertain_wait_rejects_cleanup_without_signaling_cached_group() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "exec sleep 30"]);
        let mut child = DesktopChild::spawn(&mut command).unwrap();
        child.mark_wait_uncertain_for_test();
        assert!(child.terminate_and_wait().is_err());
        child.child.kill().unwrap();
        child.child.wait().unwrap();
    }
}

/// Compositor shutdown: on SIGTERM/SIGHUP set `flag` and pulse `waker` so the
/// blocking `poll_input` returns and the loop exits gracefully at the top of
/// its next iteration. Must be called inside a tokio runtime.
pub fn install_shutdown_signal(flag: Arc<AtomicBool>, waker: termwiz::terminal::TerminalWaker) {
    tokio::spawn(async move {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(_) => return,
        };
        let mut hup = match signal(SignalKind::hangup()) {
            Ok(s) => s,
            Err(_) => return,
        };
        tokio::select! {
            _ = term.recv() => {}
            _ = hup.recv() => {}
        }
        flag.store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = waker.wake(); // best-effort: waker pulse: an input nudge must never fail the calling path
    });
}

/// Daemon shutdown: notify `shutdown` on SIGTERM/SIGINT so `kill <daemon>`
/// takes the same graceful path as the shutdown RPC. Must be called inside a
/// tokio runtime.
pub fn spawn_shutdown_notifier(shutdown: Arc<tokio::sync::Notify>) {
    tokio::spawn(async move {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler");
        tokio::select! {
            _ = term.recv() => {}
            _ = tokio::signal::ctrl_c() => {}
        }
        shutdown.notify_waiters();
    });
}

/// `RLIM_INFINITY` as a `u64`. Darwin's value is `i64::MAX`, Linux's is
/// `u64::MAX`, so a hardcoded sentinel would print a 19-digit number as a real
/// limit on one of the two. `rlim_t` is `u64` on both, hence no cast.
pub fn rlim_infinity() -> u64 {
    libc::RLIM_INFINITY
}

/// `kern.maxfilesperproc` — the per-process fd ceiling the kernel enforces
/// regardless of an "unlimited" `RLIMIT_NOFILE`. `None` off macOS, or if the
/// sysctl is unavailable.
pub fn max_files_per_proc() -> Option<u64> {
    #[cfg(target_os = "macos")]
    {
        let mut out: libc::c_int = 0;
        let mut len = std::mem::size_of::<libc::c_int>();
        let name = c"kern.maxfilesperproc";
        // SAFETY: `sysctlbyname` with a NUL-terminated name, a correctly sized
        // out-param and its matching length; no input buffer.
        let rc = unsafe {
            libc::sysctlbyname(
                name.as_ptr(),
                (&raw mut out).cast(),
                &raw mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        (rc == 0 && out > 0).then_some(out as u64)
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

/// Create a symbolic link `link` → `target` (a file link; POSIX has one kind).
#[allow(dead_code)] // test support: the dispatch done-gate tests build a symlinked artifact
pub fn symlink_file(target: &std::path::Path, link: &std::path::Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

/// Open a regular-file candidate without following a final symlink. Callers
/// still validate the opened descriptor's metadata before consuming it.
pub fn open_read_nofollow(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

pub(crate) fn os_path_from_git_bytes(bytes: &[u8]) -> anyhow::Result<std::path::PathBuf> {
    use std::os::unix::ffi::OsStringExt as _;
    Ok(std::path::PathBuf::from(std::ffi::OsString::from_vec(
        bytes.to_vec(),
    )))
}

pub(crate) fn display_git_path(path: &std::path::Path) -> String {
    use std::os::unix::ffi::OsStrExt as _;

    match path.to_str() {
        Some(text) => text.to_owned(),
        None => path
            .as_os_str()
            .as_bytes()
            .iter()
            .flat_map(|byte| std::ascii::escape_default(*byte))
            .map(char::from)
            .collect(),
    }
}

#[cfg(test)]
mod git_path_tests {
    use super::*;
    use std::os::unix::ffi::OsStrExt as _;

    #[test]
    fn non_utf8_git_paths_preserve_bytes_and_escape_for_display() {
        let path = os_path_from_git_bytes(b"bad-\xff-name").expect("Unix paths are bytes");
        assert_eq!(path.as_os_str().as_bytes(), b"bad-\xff-name");
        assert_eq!(display_git_path(&path), "bad-\\xff-name");
    }
}

#[cfg(test)]
mod remote_credential_tests {
    #[test]
    #[expect(clippy::disallowed_methods)] // isolated /bin/sh data-safety fixture, test only
    fn credential_metacharacters_are_read_as_data_not_executed() {
        let temp = tempfile::tempdir().unwrap();
        let worktree = "/host/worktree-a";
        let credential_path = temp
            .path()
            .join(crate::remote_enqueue_auth::credential_path(worktree));
        std::fs::create_dir_all(credential_path.parent().unwrap()).unwrap();
        let sentinel = temp.path().join("must-not-exist");
        let origin = format!("https://host.invalid/$(touch {})", sentinel.display());
        let token = format!("token; touch {}", sentinel.display());
        std::fs::write(&credential_path, format!("{origin}\n{token}\n")).unwrap();
        let script = format!(
            "{}printf '%s\\n%s\\n' \"$THEGN_CONTROL_URL\" \"$THEGN_CONTROL_TOKEN\"",
            crate::remote_enqueue_auth::source_prefix(worktree)
        );
        let output = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(script)
            .env("HOME", temp.path())
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            format!("{origin}\n{token}\n")
        );
        assert!(!sentinel.exists());
    }
}
#[cfg(test)]
#[path = "perf_workloads_hydration.rs"]
mod perf_workloads_hydration;

#[cfg(all(test, target_os = "linux"))]
#[path = "sandbox_floor_preflight_tests.rs"]
mod sandbox_floor_preflight_tests;

#[cfg(test)]
#[path = "capability_identity_tests.rs"]
mod capability_identity_tests;
