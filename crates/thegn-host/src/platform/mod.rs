//! OS syscall seams for the host: everything `#[cfg(unix)]` / `#[cfg(windows)]`
//! that touches process control, signals, or raw handles lives behind this
//! module. Call sites stay platform-free; only `unix.rs` / `windows.rs` contain
//! the actual syscalls. Keep the seam *thin*: anything decidable without a
//! syscall belongs in portable code (see `thegn_core::shellinv` for the shell
//! dialect logic).
//!
//! Semantics notes for the per-OS impls:
//! * "terminate" is best-effort and asynchronous — unix delivers `SIGTERM`
//!   (catchable), Windows `TerminateProcess` (hard kill; no graceful window).
//! * "process group" on unix is a real pgid (`setpgid` + `killpg`); on Windows
//!   Phase 1 tracks only the direct child pid (`TerminateProcess`), upgraded to
//!   Job Objects (whole-tree kill) in Phase 3.

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::*;

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
