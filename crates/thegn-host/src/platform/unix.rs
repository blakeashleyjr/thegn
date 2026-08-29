//! Unix impls of the platform seam: real fds, signals, and process groups.

use std::process::{Child, Command};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

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
    pid > 0 && nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None).is_ok()
}

/// Best-effort graceful termination of a single process (`SIGTERM`).
pub fn terminate_pid(pid: u32) {
    // best-effort: signal: the process may already be gone
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid as i32),
        nix::sys::signal::Signal::SIGTERM,
    )
    .ok();
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
    if pid == 0 || pid > i32::MAX as u32 {
        return Err("invalid pid".into());
    }
    let signal = match sig {
        super::ProcSignal::Terminate => nix::sys::signal::Signal::SIGTERM,
        super::ProcSignal::Kill => nix::sys::signal::Signal::SIGKILL,
    };
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), signal).map_err(|e| match e {
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

/// Restrict a directory to owner-only access (mode `0700`). Best-effort — a
/// failure hardens less but must not stop recording.
pub fn restrict_dir_owner_only(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    // best-effort: 0700 is defence-in-depth; the dir is already under the
    // per-profile state root.
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)); // best-effort: hardening: a failed chmod must never block the caller
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
