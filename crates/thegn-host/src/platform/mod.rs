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
