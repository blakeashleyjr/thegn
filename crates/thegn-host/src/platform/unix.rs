//! Unix impls of the platform seam: real fds, signals, and process groups.

use std::process::Command;
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
        nix::unistd::dup2_stderr(&self.saved).ok();
    }
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
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid as i32),
        nix::sys::signal::Signal::SIGTERM,
    )
    .ok();
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
        nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(self.pgid),
            nix::sys::signal::Signal::SIGTERM,
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
        let _ = waker.wake();
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
