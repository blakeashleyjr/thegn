//! Windows impls of the platform seam.
//!
//! No signals here: "terminate" is `TerminateProcess` (hard kill, no graceful
//! window — the unix side's SIGTERM handlers never run on Windows), and
//! shutdown notification listens to console control events (Ctrl+C / window
//! close / system shutdown) instead of SIGTERM/SIGHUP.
//!
//! Process-tree kills ride Job Objects with `KILL_ON_JOB_CLOSE`: terminating
//! the job reaps the whole tree, and merely *dropping* the last
//! [`GroupHandle`] does too — better orphan hygiene than unix pgids (a thegn
//! that dies mid-run takes its spawned trees with it).

use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, STILL_ACTIVE};
use windows_sys::Win32::System::Console::{GetStdHandle, STD_ERROR_HANDLE, SetStdHandle};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject,
};
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

/// An owned kill-on-close Job Object handle. Closing the last clone (Drop)
/// reaps every process still in the job.
struct JobInner(HANDLE);

// SAFETY: a Job Object HANDLE is process-global kernel state; using it from
// any thread is fine (the watchdog thread terminates it, the spawner drops it).
unsafe impl Send for JobInner {}
unsafe impl Sync for JobInner {}

impl Drop for JobInner {
    fn drop(&mut self) {
        // SAFETY: closing a handle we own; KILL_ON_JOB_CLOSE reaps the tree.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

/// A spawned child's Job Object (process group on unix) — what
/// [`GroupHandle::terminate`] reaps in one call.
#[derive(Clone)]
pub struct GroupHandle {
    pid: u32,
    /// `None` = degraded (job creation/assignment failed): terminate falls
    /// back to the direct child only.
    job: Option<Arc<JobInner>>,
}

impl GroupHandle {
    /// A handle over an already-known pid — for tests and callers that track
    /// pids themselves. No job: terminate is direct-child only.
    #[cfg_attr(not(test), expect(dead_code))]
    pub fn from_pid(pid: i32) -> Self {
        Self {
            pid: pid.max(0) as u32,
            job: None,
        }
    }

    /// Terminate the whole job (hard kill — no SIGTERM window on Windows), or
    /// just the direct child on the degraded path.
    pub fn terminate(&self) {
        match &self.job {
            // SAFETY: terminating a job whose handle we own.
            Some(j) => unsafe {
                TerminateJobObject(j.0, 1);
            },
            None => terminate_pid(self.pid),
        }
    }
}

/// Spawn `cmd` and assign it to a fresh kill-on-close Job Object. Best-effort:
/// if job creation/assignment fails the spawn still succeeds with a degraded
/// (direct-child-only) handle. The spawn→assign window is tiny; grandchildren
/// spawned inside it escape the job (accepted — same exposure as a unix child
/// that changes its own pgid).
pub fn spawn_grouped(cmd: &mut Command) -> std::io::Result<(std::process::Child, GroupHandle)> {
    use std::os::windows::io::AsRawHandle;
    let child = cmd.spawn()?;
    let pid = child.id();
    // SAFETY: standard Job Object setup; every handle is closed on every path
    // (JobInner owns the success case, the explicit CloseHandle the failure).
    let job = unsafe {
        let h = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if h.is_null() {
            None
        } else {
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let ok = SetInformationJobObject(
                h,
                JobObjectExtendedLimitInformation,
                (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            ) != 0
                && AssignProcessToJobObject(h, child.as_raw_handle() as HANDLE) != 0;
            if ok {
                Some(Arc::new(JobInner(h)))
            } else {
                CloseHandle(h);
                None
            }
        }
    };
    Ok((child, GroupHandle { pid, job }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole tree dies on `terminate()`: spawn `cmd /C ping -n 30 …`
    /// (cmd.exe parent + ping child), terminate the job, and verify the
    /// direct child is gone.
    #[test]
    fn job_terminate_reaps_the_tree() {
        let mut cmd = Command::new("cmd.exe");
        cmd.args(["/C", "ping -n 30 127.0.0.1 > NUL"]);
        let (mut child, group) = spawn_grouped(&mut cmd).expect("spawn under job");
        assert!(group.job.is_some(), "job assignment must succeed on CI");
        group.terminate();
        let status = child.wait().expect("wait");
        assert!(!status.success(), "terminated tree exits nonzero");
    }

    /// KILL_ON_JOB_CLOSE: dropping the last handle (no explicit terminate)
    /// also reaps the tree — the orphan-hygiene guarantee.
    #[test]
    fn dropping_the_last_handle_reaps_the_tree() {
        let mut cmd = Command::new("cmd.exe");
        cmd.args(["/C", "ping -n 30 127.0.0.1 > NUL"]);
        let (child, group) = spawn_grouped(&mut cmd).expect("spawn under job");
        assert!(group.job.is_some(), "job assignment must succeed on CI");
        let pid = child.id() as i64;
        drop(group);
        drop(child); // not reaped via wait(); the job close must kill it
        // The kernel reaps asynchronously; give it a moment.
        for _ in 0..50 {
            if !pid_alive(pid) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        panic!("child survived job-handle drop");
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
