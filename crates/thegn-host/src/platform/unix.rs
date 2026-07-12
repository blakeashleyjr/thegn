//! Unix impls of the platform seam: real fds, signals, and process groups.

use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

/// Restores the original stderr fd on drop (see [`super::redirect_stderr_to_logfile`]).
pub struct StderrGuard {
    saved: std::os::unix::io::RawFd,
}

impl Drop for StderrGuard {
    fn drop(&mut self) {
        nix::unistd::dup2(self.saved, 2).ok();
        nix::unistd::close(self.saved).ok();
    }
}

/// Point fd 2 at `file` (dup2), saving the original for the guard's `Drop`.
pub(super) fn redirect_stderr_to(file: std::fs::File) -> Option<StderrGuard> {
    use std::os::unix::io::AsRawFd;
    let saved = nix::unistd::dup(2).ok()?;
    if nix::unistd::dup2(file.as_raw_fd(), 2).is_err() {
        nix::unistd::close(saved).ok();
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

/// Put the child in its own process group so [`kill_tree`] can reap the whole
/// tree (e.g. a `cargo test` and its spawned test binaries) in one call.
pub fn set_process_group(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    cmd.process_group(0);
}

/// Best-effort termination of the process tree rooted at a child spawned with
/// [`set_process_group`] (`pid` == the child's pid == its pgid).
pub fn kill_tree(pid: i32) {
    nix::sys::signal::killpg(
        nix::unistd::Pid::from_raw(pid),
        nix::sys::signal::Signal::SIGTERM,
    )
    .ok();
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
