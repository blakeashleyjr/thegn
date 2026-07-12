//! Raise the process open-file-descriptor limit at startup.
//!
//! thegn is its own terminal multiplexer: every pane is a PTY (one master fd) +
//! a reader thread + a child process, and parked-but-resident workspaces keep
//! their panes alive so switching is instant. A long session across many
//! workspaces and terminals therefore holds a lot of fds at once. Under the
//! default soft `RLIMIT_NOFILE` (often 1024) the process can exhaust its fds,
//! at which point *every* git read fails at once — `gix::discover` can't open
//! `.git` and CLI-fallback subprocesses can't spawn — and the panel header
//! collapses to a bare "—". `[workspace].pool_limit` bounds the accumulation;
//! this raises the ceiling so the common case never gets close.
//!
//! Standard practice for terminal multiplexers (tmux, zellij do the same): lift
//! the soft limit to the hard limit, which needs no privilege. Best-effort — a
//! failure never blocks startup; it just leaves the inherited limit in place.

/// Raise the soft `RLIMIT_NOFILE` to the hard limit. Returns the resolved
/// `(soft, hard)` after the attempt, for logging. Best-effort and idempotent:
/// a no-op when the soft limit already equals the hard limit.
#[cfg(unix)]
pub(crate) fn raise_fd_limit() -> Option<(u64, u64)> {
    // SAFETY: `getrlimit`/`setrlimit` on RLIMIT_NOFILE with a valid, stack-owned
    // `rlimit` are standard POSIX calls with no aliasing or lifetime hazards.
    unsafe {
        let mut lim = std::mem::zeroed::<libc::rlimit>();
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) != 0 {
            return None;
        }
        // Already at the ceiling — nothing to do.
        if lim.rlim_cur < lim.rlim_max {
            let want = libc::rlimit {
                rlim_cur: lim.rlim_max,
                rlim_max: lim.rlim_max,
            };
            // best-effort: on failure the inherited soft limit stays in force.
            if libc::setrlimit(libc::RLIMIT_NOFILE, &want) == 0 {
                lim.rlim_cur = lim.rlim_max;
            }
        }
        Some((lim.rlim_cur as u64, lim.rlim_max as u64))
    }
}

/// Non-unix platforms have no `RLIMIT_NOFILE`; nothing to raise.
#[cfg(not(unix))]
pub(crate) fn raise_fd_limit() -> Option<(u64, u64)> {
    None
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn soft_limit_ends_at_or_above_the_original() {
        // Capture the inherited soft limit, then raise. The post-raise soft
        // limit must never be *below* where we started (it climbs to the hard
        // limit, or stays put when already there).
        let before = unsafe {
            let mut lim = std::mem::zeroed::<libc::rlimit>();
            assert_eq!(libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim), 0);
            lim.rlim_cur as u64
        };
        let (soft, hard) = raise_fd_limit().expect("getrlimit succeeds on unix");
        assert!(
            soft >= before,
            "soft {soft} dropped below original {before}"
        );
        assert!(soft <= hard, "soft {soft} exceeds hard {hard}");
    }
}
