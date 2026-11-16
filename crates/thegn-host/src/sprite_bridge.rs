//! `thegn sprite-exec <id> [--] cmd…` — the self-bridge that gives a sprites
//! (WSS-native) env a CLI exec prefix with no vendor `sprite` CLI. Two modes,
//! chosen by whether stdin is a terminal:
//!
//! - **Capture** (stdin not a tty): the control-plane READ path — the chrome
//!   git/gh/fs reads and the persisted worktree `GitLoc` shell out through it
//!   (the role `vps-ssh` plays for VPS envs). Runs the command over the native
//!   exec API, relays its captured stdout, and exits with its code, so a caller
//!   like `git -C /workspace status` reads the output exactly as if local.
//! - **TTY** (stdin is a tty): a long-running agent/session launched as
//!   `thegn sprite-exec <id> -- agent…` (the create handle's exec prefix). Opens
//!   a PTY exec, puts the local tty in raw mode, pumps keystrokes and resizes
//!   up and output down, and exits with the remote code. When the stream drops
//!   without an exit frame it reattaches to the announced exec session with
//!   backoff (THE-2047); the held stream is also what keeps the sprite awake
//!   while a thegn session runs. When reattach fails the remote process did
//!   not survive (the sprite suspended): one line on stdout and exit
//!   [`EXIT_REMOTE_LOST`], which `session list` renders as
//!   `cause:remote-exec-lost`.
//!
//! Resolves the sprites provider from the first WSS-native env whose API token
//! resolves, built named with THIS id (exec takes the id as an argument).

use std::io::{IsTerminal as _, Write as _};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow};
use termwiz::caps::{Capabilities, ProbeHints};
use termwiz::input::{InputEvent, KeyEvent};
use termwiz::terminal::{Terminal, new_terminal};
use thegn_core::config::Config;
use thegn_core::config_env_tables::wss_native_provider_kind;
use thegn_svc::provider::{ExecControl, ExecFrame, ExecSession, ExecSpec, Provider};

/// Exit code when the remote exec stream was lost and could not be reattached
/// (`EX_TEMPFAIL`): the remote process did not survive. `session list` keys
/// its `cause:remote-exec-lost` field on it.
pub(crate) const EXIT_REMOTE_LOST: i32 = 75;

/// Reattach attempts after a dropped stream (one per [`reconnect_delays`] slot).
const REATTACH_ATTEMPTS: usize = 5;

/// A reattached stream that stays up this long earns a fresh attempt budget, so
/// a long session survives many separate blips — but a stream that reattaches
/// and immediately drops again cannot retry forever.
const REATTACH_BUDGET_RESET: Duration = Duration::from_secs(60);

/// Exit code when the local terminal went away (stdin closed / tty error) and
/// the remote never reported an exit within [`LOCAL_CLOSE_GRACE`].
const EXIT_LOCAL_CLOSED: i32 = 1;

/// How long to wait for the remote exit frame after the local tty went away.
const LOCAL_CLOSE_GRACE: Duration = Duration::from_secs(5);

/// Run `cmd` inside sprite `id` over the native exec API. A tty on stdin gets
/// the streaming PTY bridge; anything else the one-shot capture used by the
/// control reads. `cmd` arrives already shaped by `GitLoc::provider_command`
/// (`/bin/sh -lc <script>`) or the launch argv; empty falls back to a login
/// shell.
pub fn run(cfg: &Config, id: &str, cmd: &[String]) -> Result<()> {
    let argv: Vec<String> = if cmd.is_empty() {
        vec![
            "/bin/sh".into(),
            "-lc".into(),
            "exec ${SHELL:-/bin/sh} -l".into(),
        ]
    } else {
        cmd.to_vec()
    };
    // Any WSS-native env's provider config works — the sandbox id is the routing
    // key, and exec takes it as an argument (like `vps-ssh` resolving by name).
    let provider = cfg
        .env
        .values()
        .filter(|envc| wss_native_provider_kind(&envc.provider.provider))
        .find_map(|envc| crate::provider_factory::provider_for_named(&envc.provider, id))
        .ok_or_else(|| {
            anyhow!("sprite-exec: no WSS-native env with a resolvable API token for id {id:?}")
        })?;
    if std::io::stdin().is_terminal() {
        let code = run_tty(&provider, id, argv)?;
        std::process::exit(code);
    }
    // Current-thread: one network call does not need a worker per core.
    // See `agent::provider_runtime`.
    let rt = crate::agent::provider_runtime()?;
    let (code, out) = rt.block_on(async { provider.run_exec(id, &argv, None, &[]).await })?;
    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(out.as_bytes()); // best-effort: stdout write: EPIPE on a closed |head pipe is normal
    let _ = stdout.flush(); // best-effort: flush: display-only
    std::process::exit(code);
}

// ---------------------------------------------------------------------------
// Pure decisions (unit-tested). The streaming below only wires them up.
// ---------------------------------------------------------------------------

/// Backoff before each reattach attempt: 1, 2, 4, 8, 15 s.
pub(crate) fn reconnect_delays() -> [Duration; REATTACH_ATTEMPTS] {
    [1, 2, 4, 8, 15].map(Duration::from_secs)
}

/// Whether to try reattaching (attempt `attempt`, 0-based) after the frame
/// stream ended: only when no exit frame arrived (the remote process may still
/// be running), a session id was announced to reattach to, and the attempt
/// budget is not spent.
pub(crate) fn should_reattach(saw_exit: bool, session_id: Option<&str>, attempt: usize) -> bool {
    !saw_exit
        && session_id.is_some_and(|s| !s.trim().is_empty())
        && attempt < reconnect_delays().len()
}

/// The attempt counter to carry into the next drop: a stream that stayed up
/// for [`REATTACH_BUDGET_RESET`] earns a fresh budget, otherwise the count
/// keeps climbing toward [`should_reattach`]'s limit.
pub(crate) fn attempts_after_stream(attempt: usize, lived: Duration) -> usize {
    if lived >= REATTACH_BUDGET_RESET {
        0
    } else {
        attempt
    }
}

/// The one line written to stdout when the stream is lost for good.
pub(crate) fn lost_line(id: &str, cause: &str) -> String {
    format!(
        "thegn: sprite {id} exec stream lost ({cause}); the sprite may have suspended — the remote process did not survive"
    )
}

/// A usable PTY size: termwiz can report 0×0 for a not-yet-sized pty.
pub(crate) fn pty_size(cols: usize, rows: usize) -> (u16, u16) {
    let clamp = |v: usize, fallback: u16| match u16::try_from(v) {
        Ok(0) => fallback,
        Ok(v) => v,
        Err(_) => u16::MAX,
    };
    (clamp(cols, 80), clamp(rows, 24))
}

/// The terminal modes the REMOTE app set through its output that change how a
/// decoded local key must be re-encoded: DECCKM application cursor keys
/// (`CSI ? 1 h/l`) and bracketed paste (`CSI ? 2004 h/l`). Sequences may split
/// across frames, so an unfinished `CSI ?` sequence is carried over.
#[derive(Debug, Default)]
pub(crate) struct RemoteModes {
    pub(crate) app_cursor: bool,
    pub(crate) bracketed_paste: bool,
    pending: Vec<u8>,
}

impl RemoteModes {
    /// Longest `CSI ? <params>` worth buffering; anything longer is not a mode
    /// set this tracker cares about.
    const MAX_PENDING: usize = 32;

    pub(crate) fn observe(&mut self, bytes: &[u8]) {
        for &b in bytes {
            if self.pending.is_empty() {
                if b == 0x1b {
                    self.pending.push(b);
                }
                continue;
            }
            self.pending.push(b);
            match self.pending.len() {
                2 if b != b'[' => self.restart(b),
                3 if b != b'?' => self.restart(b),
                2 | 3 => {}
                n => match b {
                    b'0'..=b'9' | b';' if n < Self::MAX_PENDING => {}
                    b'h' | b'l' => {
                        self.apply(b == b'h');
                        self.pending.clear();
                    }
                    _ => self.restart(b),
                },
            }
        }
    }

    fn restart(&mut self, b: u8) {
        self.pending.clear();
        if b == 0x1b {
            self.pending.push(b);
        }
    }

    fn apply(&mut self, on: bool) {
        let params = &self.pending[3..self.pending.len() - 1];
        for p in params.split(|&c| c == b';') {
            match p {
                b"1" => self.app_cursor = on,
                b"2004" => self.bracketed_paste = on,
                _ => {}
            }
        }
    }
}

/// What the blocking tty thread forwards to the async pump.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FromTty {
    Key(KeyEvent),
    Paste(String),
    Resize { cols: u16, rows: u16 },
}

/// The stdin bytes for one decoded local input event, encoded for the remote
/// app's current modes. `None` for an event with no byte form (or a resize).
pub(crate) fn encode_input(ev: &FromTty, modes: &RemoteModes) -> Option<Vec<u8>> {
    match ev {
        FromTty::Key(k) => crate::input::key_bytes_mode(&k.key, k.modifiers, modes.app_cursor),
        FromTty::Paste(text) if modes.bracketed_paste => {
            let mut out = b"\x1b[200~".to_vec();
            out.extend_from_slice(text.as_bytes());
            out.extend_from_slice(b"\x1b[201~");
            Some(out)
        }
        FromTty::Paste(text) => Some(text.as_bytes().to_vec()),
        FromTty::Resize { .. } => None,
    }
}

// ---------------------------------------------------------------------------
// Streaming seam (cov_ignore): a live tty + a network exec stream. Every
// decision above is pure and unit-tested; this only moves bytes.
// ---------------------------------------------------------------------------

/// How one exec stream ended.
enum StreamEnd {
    /// The remote process exited with this code.
    Exit(i32),
    /// The frame stream closed without an exit frame (network drop / suspend).
    Dropped,
    /// The local terminal went away (stdin closed, tty error, stdout gone).
    LocalGone,
}

/// The tty thread plus its stop handshake. Dropping it (any exit path, unwind
/// included) stops the thread, which restores the terminal as it ends.
struct TtyGuard {
    done: Arc<AtomicBool>,
    waker: termwiz::terminal::TerminalWaker,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl TtyGuard {
    fn stop(&mut self) {
        self.done.store(true, Ordering::SeqCst);
        let _ = self.waker.wake(); // best-effort: waker pulse: an input nudge must never fail the calling path
        if let Some(t) = self.thread.take() {
            let _ = t.join(); // best-effort: thread join: a panicked tty thread loses input, not the exit code
        }
    }
}

impl Drop for TtyGuard {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Stream `argv` in sprite `id` over a PTY exec with the local tty in raw mode.
/// Returns the code to exit with (the remote's, or [`EXIT_REMOTE_LOST`]); the
/// tty is restored before this returns on every path. Subprocess/network seam
/// (`cov_ignore`).
fn run_tty(provider: &Provider, id: &str, argv: Vec<String>) -> Result<i32> {
    // Mouse + bracketed paste stay the REMOTE app's call: its own output sets
    // those modes on the outer terminal, so termwiz must not force them on.
    let hints = ProbeHints::new_from_env()
        .mouse_reporting(Some(false))
        .bracketed_paste(Some(false));
    let caps = Capabilities::new_with_hints(hints).context("term capabilities")?;
    let mut term = new_terminal(caps).context("open terminal")?;
    let size = term.get_screen_size().context("screen size")?;
    let (cols, rows) = pty_size(size.cols, size.rows);

    let rt = crate::agent::provider_runtime()?;
    // Open BEFORE raw mode so an open failure never leaves the tty altered.
    let session = rt.block_on(provider.open_exec(
        id,
        &ExecSpec {
            argv,
            tty: true,
            cols,
            rows,
            env: vec![],
            cwd: None,
        },
    ))?;

    term.set_raw_mode().context("raw mode")?;
    let (tty_tx, tty_rx) = tokio::sync::mpsc::channel::<FromTty>(256);
    let done = Arc::new(AtomicBool::new(false));
    let waker = term.waker();
    let thread = {
        let done = done.clone();
        thegn_core::thread_name::spawn_named("thegn-sprite-in", move || {
            crate::platform::qos::set_self(crate::platform::qos::Qos::Interactive); // Interactive: remote session keystrokes.
            tty_loop(term, tty_tx, done)
        })
    };
    let mut guard = TtyGuard {
        done,
        waker,
        thread: Some(thread),
    };

    let outcome = rt.block_on(drive(provider, id, session, tty_rx, (cols, rows)));
    // Restore the tty before anything else is printed or the process exits.
    guard.stop();
    match outcome {
        Ok(code) => Ok(code),
        Err(cause) => {
            let mut out = std::io::stdout();
            let _ = writeln!(out, "{}", lost_line(id, &cause)); // best-effort: stdout write: the pane may already be gone
            let _ = out.flush(); // best-effort: flush: display-only
            Ok(EXIT_REMOTE_LOST)
        }
    }
}

/// Pump one session and reattach across drops. `Ok(code)` is the exit code;
/// `Err(cause)` means the stream is lost for good.
async fn drive(
    provider: &Provider,
    id: &str,
    mut session: ExecSession,
    mut tty_rx: tokio::sync::mpsc::Receiver<FromTty>,
    mut size: (u16, u16),
) -> std::result::Result<i32, String> {
    let mut modes = RemoteModes::default();
    let mut session_id: Option<String> = None;
    let mut attempt = 0usize;
    loop {
        let started = Instant::now();
        let end = pump(&mut session, &mut tty_rx, &mut modes, &mut size).await;
        // A reattached stream may not re-announce; keep the last id seen.
        let announced = session.session_id.borrow().clone();
        if announced.is_some() {
            session_id = announced;
        }
        match end {
            StreamEnd::Exit(code) => return Ok(code),
            StreamEnd::LocalGone => {
                let _ = session.control.send(ExecControl::Close).await; // best-effort: send: the driver may already be gone
                return Ok(await_exit(&mut session).await.unwrap_or(EXIT_LOCAL_CLOSED));
            }
            StreamEnd::Dropped => {}
        }
        attempt = attempts_after_stream(attempt, started.elapsed());
        let mut cause = "stream ended without an exit status".to_string();
        session = loop {
            if !should_reattach(false, session_id.as_deref(), attempt) {
                if session_id.is_none() {
                    cause = format!("{cause}; no exec session id to reattach to");
                }
                return Err(cause);
            }
            tokio::time::sleep(reconnect_delays()[attempt]).await;
            attempt += 1;
            let sid = session_id.as_deref().unwrap_or_default();
            match provider.attach_exec(id, sid, size.0, size.1).await {
                Ok(s) => break s,
                Err(e) => cause = format!("reattach failed: {e:#}"),
            }
        };
    }
}

/// Wait briefly for the remote exit frame after a local close.
async fn await_exit(session: &mut ExecSession) -> Option<i32> {
    tokio::time::timeout(LOCAL_CLOSE_GRACE, async {
        while let Some(frame) = session.frames.recv().await {
            if let ExecFrame::Exit(code) = frame {
                return Some(code);
            }
        }
        None
    })
    .await
    .ok()
    .flatten()
}

/// Move bytes for one exec stream until it ends.
async fn pump(
    session: &mut ExecSession,
    tty_rx: &mut tokio::sync::mpsc::Receiver<FromTty>,
    modes: &mut RemoteModes,
    size: &mut (u16, u16),
) -> StreamEnd {
    let mut out = std::io::stdout();
    loop {
        tokio::select! {
            frame = session.frames.recv() => match frame {
                Some(ExecFrame::Stdout(bytes)) => {
                    modes.observe(&bytes);
                    if out.write_all(&bytes).and_then(|()| out.flush()).is_err() {
                        return StreamEnd::LocalGone;
                    }
                }
                Some(ExecFrame::Exit(code)) => return StreamEnd::Exit(code),
                None => return StreamEnd::Dropped,
            },
            ev = tty_rx.recv() => match ev {
                Some(FromTty::Resize { cols, rows }) => {
                    *size = (cols, rows);
                    let _ = session.control.send(ExecControl::Resize { cols, rows }).await; // best-effort: send: a closed driver surfaces as the frame stream ending
                }
                Some(ev) => {
                    if let Some(bytes) = encode_input(&ev, modes) {
                        let _ = session.control.send(ExecControl::Stdin(bytes)).await; // best-effort: send: a closed driver surfaces as the frame stream ending
                    }
                }
                None => return StreamEnd::LocalGone,
            },
        }
    }
}

/// Blocking tty reader: owns the raw-mode terminal, forwards decoded input and
/// resizes (termwiz's SIGWINCH path) to the pump, and restores cooked mode as
/// it ends — when `done` is set (pulsed via the waker) or the tty errors.
fn tty_loop<T: Terminal>(
    mut term: T,
    tx: tokio::sync::mpsc::Sender<FromTty>,
    done: Arc<AtomicBool>,
) {
    while !done.load(Ordering::SeqCst) {
        let ev = match term.poll_input(None) {
            Ok(Some(InputEvent::Key(k))) => FromTty::Key(k),
            Ok(Some(InputEvent::Paste(text))) => FromTty::Paste(text),
            Ok(Some(InputEvent::Resized { cols, rows })) => {
                let (cols, rows) = pty_size(cols, rows);
                FromTty::Resize { cols, rows }
            }
            // Wake (the stop pulse) / mouse (not forwarded): re-check `done`.
            Ok(Some(_)) | Ok(None) => continue,
            Err(_) => break,
        };
        if tx.blocking_send(ev).is_err() {
            break;
        }
    }
    let _ = term.set_cooked_mode(); // best-effort: tty restore: the terminal drop restores again
    let _ = term.flush(); // best-effort: flush: display-only
}

#[cfg(test)]
mod tests {
    use super::{
        EXIT_REMOTE_LOST, FromTty, REATTACH_BUDGET_RESET, RemoteModes, attempts_after_stream,
        encode_input, lost_line, pty_size, reconnect_delays, should_reattach,
    };
    use std::time::Duration;
    use termwiz::input::{KeyCode, KeyEvent, Modifiers};

    #[test]
    fn reconnect_delays_back_off_1_2_4_8_15() {
        assert_eq!(
            reconnect_delays(),
            [1, 2, 4, 8, 15].map(Duration::from_secs)
        );
        let total: Duration = reconnect_delays().iter().sum();
        assert_eq!(total, Duration::from_secs(30), "rides out a ~20s blip");
    }

    #[test]
    fn should_reattach_needs_no_exit_a_session_id_and_budget() {
        assert!(should_reattach(false, Some("s-1"), 0));
        assert!(should_reattach(false, Some("s-1"), 4));
        assert!(!should_reattach(false, Some("s-1"), 5), "budget spent");
        assert!(!should_reattach(true, Some("s-1"), 0), "remote exited");
        assert!(!should_reattach(false, None, 0), "nothing to reattach to");
        assert!(!should_reattach(false, Some("  "), 0), "blank id");
    }

    #[test]
    fn a_long_lived_stream_earns_a_fresh_attempt_budget() {
        assert_eq!(attempts_after_stream(3, REATTACH_BUDGET_RESET), 0);
        assert_eq!(attempts_after_stream(3, Duration::from_secs(5)), 3);
        assert_eq!(attempts_after_stream(0, Duration::ZERO), 0);
    }

    #[test]
    fn lost_line_names_the_sprite_and_the_cause() {
        assert_eq!(
            lost_line("dev-1", "reattach failed: 404"),
            "thegn: sprite dev-1 exec stream lost (reattach failed: 404); the sprite may have suspended — the remote process did not survive"
        );
        assert!(!lost_line("a", "b").contains('\n'));
        assert_eq!(EXIT_REMOTE_LOST, 75);
    }

    #[test]
    fn pty_size_falls_back_on_zero_and_saturates() {
        assert_eq!(pty_size(0, 0), (80, 24));
        assert_eq!(pty_size(120, 40), (120, 40));
        assert_eq!(pty_size(100_000, 30), (u16::MAX, 30));
    }

    #[test]
    fn remote_modes_track_cursor_and_paste_across_split_frames() {
        let mut m = RemoteModes::default();
        m.observe(b"hello\x1b[?1h");
        assert!(m.app_cursor);
        m.observe(b"\x1b[?20");
        assert!(!m.bracketed_paste, "incomplete sequence not applied yet");
        m.observe(b"04hmore");
        assert!(m.bracketed_paste);
        m.observe(b"\x1b[?1;2004l");
        assert!(!m.app_cursor && !m.bracketed_paste);
        // Unrelated private modes and plain CSI leave both alone.
        m.observe(b"\x1b[?25h\x1b[1h\x1b[?1049h");
        assert!(!m.app_cursor && !m.bracketed_paste);
        // An ESC inside a broken sequence restarts parsing.
        m.observe(b"\x1b[?1\x1b[?1h");
        assert!(m.app_cursor);
    }

    #[test]
    fn encode_input_honors_remote_modes() {
        let up = FromTty::Key(KeyEvent {
            key: KeyCode::UpArrow,
            modifiers: Modifiers::NONE,
        });
        let mut m = RemoteModes::default();
        assert_eq!(encode_input(&up, &m).as_deref(), Some(&b"\x1b[A"[..]));
        m.observe(b"\x1b[?1h");
        assert_eq!(encode_input(&up, &m).as_deref(), Some(&b"\x1bOA"[..]));

        let paste = FromTty::Paste("ls".into());
        assert_eq!(encode_input(&paste, &m).as_deref(), Some(&b"ls"[..]));
        m.observe(b"\x1b[?2004h");
        assert_eq!(
            encode_input(&paste, &m).as_deref(),
            Some(&b"\x1b[200~ls\x1b[201~"[..])
        );
        assert_eq!(
            encode_input(&FromTty::Resize { cols: 1, rows: 1 }, &m),
            None
        );
    }
}
