//! Windows impls of the platform seam.
//!
//! No signals here: "terminate" is `TerminateProcess` (hard kill, no graceful
//! window — the unix side's SIGTERM handlers never run on Windows), and
//! shutdown notification listens to console control events (Ctrl+C / window
//! close / system shutdown) instead of SIGTERM/SIGHUP.
//!
//! Phase 1 scopes process-tree kills to the direct child; Job Objects (whole-
//! tree kill-on-close) land in Phase 3.

use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, STILL_ACTIVE};
use windows_sys::Win32::System::Console::{GetStdHandle, STD_ERROR_HANDLE, SetStdHandle};
use windows_sys::Win32::System::Threading::{
    GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
    TerminateProcess,
};

/// Restores the original stderr handle on drop
/// (see [`super::redirect_stderr_to_logfile`]).
pub struct StderrGuard {
    saved: HANDLE,
    /// Owns the log-file handle `STD_ERROR_HANDLE` now points at; dropped (and
    /// the handle closed) only after the std handle is restored.
    _file: std::fs::File,
}

// SAFETY: the raw HANDLE is only touched in `Drop`; the guard lives on the
// compositor's main thread but the types it's embedded in may assert Send.
unsafe impl Send for StderrGuard {}

impl Drop for StderrGuard {
    fn drop(&mut self) {
        // SAFETY: restoring a std handle we saved earlier; best-effort.
        unsafe {
            SetStdHandle(STD_ERROR_HANDLE, self.saved);
        }
    }
}

/// Point the process's `STD_ERROR_HANDLE` at `file`, saving the original for
/// the guard's `Drop`. Rust's `std::io::stderr` resolves the std handle per
/// write, so panics/`eprintln!` from any thread land in the log. (C-runtime
/// fd-2 writers are not rebound — thegn has no C code that writes stderr.)
pub(super) fn redirect_stderr_to(file: std::fs::File) -> Option<StderrGuard> {
    use std::os::windows::io::AsRawHandle;
    // SAFETY: querying/replacing our own process's std handle slot.
    unsafe {
        let saved = GetStdHandle(STD_ERROR_HANDLE);
        if SetStdHandle(STD_ERROR_HANDLE, file.as_raw_handle() as HANDLE) == 0 {
            return None;
        }
        Some(StderrGuard { saved, _file: file })
    }
}

/// Is a process with this pid alive?
pub fn pid_alive(pid: i64) -> bool {
    if pid <= 0 {
        return false;
    }
    // SAFETY: probing a foreign pid with the narrowest access right; the
    // handle is closed on every path.
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid as u32);
        if h.is_null() {
            return false;
        }
        let mut code: u32 = 0;
        let alive = GetExitCodeProcess(h, &mut code) != 0 && code == STILL_ACTIVE as u32;
        CloseHandle(h);
        alive
    }
}

/// Best-effort termination of a single process. Hard kill — Windows has no
/// SIGTERM; the child gets no cleanup window.
pub fn terminate_pid(pid: u32) {
    // SAFETY: terminating an explicit pid; the handle is closed on every path.
    unsafe {
        let h = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if h.is_null() {
            return;
        }
        TerminateProcess(h, 1);
        CloseHandle(h);
    }
}

/// Phase-1 stub: no pgid on Windows. Phase 3 assigns the child to a Job
/// Object here so [`kill_tree`] reaps the whole tree.
pub fn set_process_group(_cmd: &mut Command) {}

/// Best-effort termination of the tree rooted at `pid`. Phase 1 kills only the
/// direct child (grandchildren may survive); Job Objects fix that in Phase 3.
pub fn kill_tree(pid: i32) {
    if pid > 0 {
        terminate_pid(pid as u32);
    }
}

/// Compositor shutdown: on Ctrl+C / console close / system shutdown set `flag`
/// and pulse `waker` so the blocking `poll_input` returns and the loop exits
/// gracefully. Must be called inside a tokio runtime.
pub fn install_shutdown_signal(flag: Arc<AtomicBool>, waker: termwiz::terminal::TerminalWaker) {
    tokio::spawn(async move {
        use tokio::signal::windows;
        let (Ok(mut ctrl_c), Ok(mut close), Ok(mut shut)) = (
            windows::ctrl_c(),
            windows::ctrl_close(),
            windows::ctrl_shutdown(),
        ) else {
            return;
        };
        tokio::select! {
            _ = ctrl_c.recv() => {}
            _ = close.recv() => {}
            _ = shut.recv() => {}
        }
        flag.store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = waker.wake();
    });
}

/// Daemon shutdown: notify `shutdown` on Ctrl+C / console close / system
/// shutdown — the same graceful path as the shutdown RPC. Must be called
/// inside a tokio runtime.
pub fn spawn_shutdown_notifier(shutdown: Arc<tokio::sync::Notify>) {
    tokio::spawn(async move {
        use tokio::signal::windows;
        let (Ok(mut ctrl_c), Ok(mut close), Ok(mut shut)) = (
            windows::ctrl_c(),
            windows::ctrl_close(),
            windows::ctrl_shutdown(),
        ) else {
            return;
        };
        tokio::select! {
            _ = ctrl_c.recv() => {}
            _ = close.recv() => {}
            _ = shut.recv() => {}
        }
        shutdown.notify_waiters();
    });
}
