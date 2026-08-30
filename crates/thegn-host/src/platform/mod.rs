//! OS syscall seams for the host: everything `#[cfg(unix)]` / `#[cfg(windows)]`
//! that touches process control, signals, or raw handles lives behind this
//! module. Call sites stay platform-free; only `unix.rs` / `windows.rs` contain
//! the actual syscalls. Keep the seam *thin*: anything decidable without a
//! syscall belongs in portable code (see `thegn_core::shellinv` for the shell
//! dialect logic).
//!
//! Semantics notes for the per-OS impls:
//! * "terminate" is best-effort and asynchronous — unix delivers `SIGTERM`
//!   (catchable), Windows `TerminateProcess`/`TerminateJobObject` (hard kill;
//!   no graceful window).
//! * [`spawn_grouped`] puts the child in a real pgid on unix (`setpgid` +
//!   `killpg`) and a kill-on-close Job Object on Windows — there, dropping the
//!   last [`GroupHandle`] also reaps the tree (orphan hygiene beyond pgids).

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::*;

/// A signal the monitor's Processes tab can deliver to a selected pid.
///
/// Two rungs only — a graceful ask, then a hard stop — matching the tab's
/// confirm flow (SIGTERM first, SIGKILL only on a second explicit confirmation).
/// Windows has no signals, so both map to `TerminateProcess` (a hard kill) and
/// the confirm text says so. See [`signal_pid`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcSignal {
    /// Graceful termination request (unix `SIGTERM`; Windows hard terminate).
    Terminate,
    /// Forceful kill (unix `SIGKILL`; Windows hard terminate).
    Kill,
}

/// Live per-process introspection (pane cwd / foreground job / argv). Unlike the
/// rest of this module the split is Linux-vs-macOS-vs-other rather than
/// unix-vs-windows, because `/proc` is a Linux facility, not a POSIX one.
pub(crate) mod proc;

/// Per-thread scheduler quality-of-service. macOS-only in effect (it is what
/// steers a thread to the efficiency cores on Apple silicon); a no-op elsewhere.
pub(crate) mod qos;

pub(crate) mod sound;

#[cfg(test)]
pub(crate) fn test_fifo_supported() -> bool {
    cfg!(unix)
}

#[cfg(test)]
pub(crate) fn test_fifo(path: &std::path::Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use nix::sys::stat::Mode;
        use nix::unistd::mkfifo;

        Ok(mkfifo(path, Mode::S_IRUSR | Mode::S_IWUSR)?)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Err(std::io::Error::other(
            "FIFOs are not supported by this test",
        ))
    }
}

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;

/// Redirect process stderr to `$XDG_STATE_HOME/thegn/logs/thegn-stderr.log`
/// for the compositor's lifetime. Returns a guard whose `Drop` restores the
/// original stderr. `None` (no redirect) if any step fails — never blocks
/// startup.
pub fn redirect_stderr_to_logfile() -> Option<StderrGuard> {
    let dir = thegn_core::util::xdg_state_home().join("thegn/logs");
    std::fs::create_dir_all(&dir).ok()?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("thegn-stderr.log"))
        .ok()?;
    redirect_stderr_to(file)
}
