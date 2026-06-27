//! Outer-terminal capability probe (the impure I/O seam).
//!
//! Env detection ([`superzej_core::termcaps::detect`]) is authoritative, but a
//! terminal reached over `ssh`/`tmux` can carry a generic `TERM` / no
//! `COLORTERM` while actually being a modern truecolor emulator. This probe
//! writes a Primary Device Attributes query (`CSI c`) + an XTVERSION query
//! (`CSI > q`) and reads the raw reply, so [`superzej_core::termcaps::apply_probe`]
//! can upgrade the env baseline.
//!
//! **It runs once at startup, after `set_raw_mode()` but BEFORE termwiz's
//! `BufferedTerminal` (and its input reader thread) takes the tty** — so we own
//! the fd, read the response cleanly, and the very first frame already reflects
//! the probe (no re-render flash). termwiz 0.23 can't surface DA/XTVERSION
//! replies through its input layer (they spill as key events — the same limit
//! that disables the kitty keyboard protocol), which is exactly why the read is
//! done here instead.
//!
//! Cost & safety: gated to a real tty on both stdin and stdout (skipped in
//! pipes / CI / tests), and bounded by a short deadline so a terminal that
//! never answers can't stall launch beyond [`PROBE_BUDGET`]. Interactive
//! terminals answer in a few ms; the read returns as soon as the DA terminator
//! arrives.

/// Query the outer terminal and interpret its reply. Returns `None` when the
/// probe is skipped (not a tty, disabled, or non-Unix) so the caller keeps env
/// detection. Never blocks longer than the probe budget.
///
/// Non-Unix targets have no `poll`/`isatty` raw-fd read path here, so the probe
/// is a no-op and env detection stands (which already covers Windows Terminal
/// via `WT_SESSION`).
#[cfg(not(unix))]
pub fn probe_outer_terminal() -> Option<superzej_core::termcaps::ProbeResult> {
    None
}

#[cfg(unix)]
pub use unix::probe_outer_terminal;

#[cfg(unix)]
mod unix {
    use std::io::{Read as _, Write as _};
    use std::os::fd::AsRawFd as _;
    use std::time::{Duration, Instant};

    use superzej_core::termcaps::{ProbeResult, interpret_probe};

    /// Upper bound on how long the probe may wait for a reply before giving up
    /// and falling back to env detection. Kept well under the launch budget;
    /// overridable via `SUPERZEJ_PROBE_MS` (0 disables the probe entirely).
    const PROBE_BUDGET: Duration = Duration::from_millis(80);

    fn is_tty(fd: i32) -> bool {
        // SAFETY: isatty is a pure query on a file descriptor.
        unsafe { libc::isatty(fd) == 1 }
    }

    fn budget() -> Option<Duration> {
        match std::env::var("SUPERZEJ_PROBE_MS") {
            Ok(v) => match v.trim().parse::<u64>() {
                Ok(0) => None, // explicitly disabled
                Ok(ms) => Some(Duration::from_millis(ms)),
                Err(_) => Some(PROBE_BUDGET),
            },
            Err(_) => Some(PROBE_BUDGET),
        }
    }

    /// Query the outer terminal and interpret its reply. Returns `None` when the
    /// probe is skipped (not a tty, or disabled) so the caller keeps env detection.
    /// Never blocks longer than the probe budget.
    pub fn probe_outer_terminal() -> Option<ProbeResult> {
        let stdin = std::io::stdin();
        let stdout = std::io::stdout();
        let in_fd = stdin.as_raw_fd();
        let out_fd = stdout.as_raw_fd();
        if !is_tty(in_fd) || !is_tty(out_fd) {
            return None;
        }
        let deadline_budget = budget()?;

        // XTVERSION first, then Primary DA: terminals answer in order, so seeing the
        // DA terminator (`c`) means any XTVERSION reply already arrived → we can stop.
        {
            let mut out = stdout.lock();
            out.write_all(b"\x1b[>q\x1b[c").ok()?;
            out.flush().ok()?;
        }

        let deadline = Instant::now() + deadline_budget;
        let mut buf = Vec::with_capacity(64);
        let mut chunk = [0u8; 64];
        let mut handle = stdin.lock();
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            if !wait_readable(in_fd, remaining) {
                break; // timeout or error → stop, interpret what we have
            }
            match handle.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    buf.extend_from_slice(&chunk[..n]);
                    // The DA reply (`ESC [ ? … c`) is the last thing we asked for.
                    if let Some(pos) = buf.iter().position(|&b| b == b'?')
                        && buf[pos..].contains(&b'c')
                    {
                        break;
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }

        Some(interpret_probe(&buf))
    }

    /// Block until `fd` is readable or `timeout` elapses. Returns true if readable.
    fn wait_readable(fd: i32, timeout: Duration) -> bool {
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
        // SAFETY: single valid pollfd, count 1.
        let rc = unsafe { libc::poll(&mut pfd, 1, ms) };
        rc > 0 && (pfd.revents & libc::POLLIN) != 0
    }
}
