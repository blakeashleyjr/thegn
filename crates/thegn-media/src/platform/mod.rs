//! Per-operating-system media implementations.
//!
//! Platform selection belongs at this module boundary so the leaf's pure model
//! and provider seam remain cross-target compilable without leaking D-Bus,
//! WinRT, or AppleScript details into callers.

#[cfg(target_os = "linux")]
pub mod linux;

/// Put a helper into its own process group so the whole tree can be signalled.
/// On non-unix targets this is a no-op: only the direct child is killed (via
/// `kill_on_drop`); no Job Object containment exists in this leaf crate.
#[cfg(unix)]
pub(crate) fn prepare_group(cmd: &mut tokio::process::Command) {
    cmd.process_group(0);
}
#[cfg(not(unix))]
pub(crate) fn prepare_group(_cmd: &mut tokio::process::Command) {}

/// SIGKILL the helper's process group. Callers must only pass the pid of a
/// leader that has NOT been reaped yet: after `wait` the pgid may be recycled.
#[cfg(unix)]
pub(crate) fn kill_group(leader: u32) {
    if let Ok(pid) = i32::try_from(leader) {
        if pid > 1 {
            // SAFETY: killpg on a pgid we created via process_group(0); the
            // leader is unreaped so the group id cannot have been recycled.
            unsafe {
                libc::killpg(pid, libc::SIGKILL);
            }
        }
    }
}
#[cfg(not(unix))]
pub(crate) fn kill_group(_leader: u32) {}
