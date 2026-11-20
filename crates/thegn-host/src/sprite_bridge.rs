//! `thegn sprite-exec <id> [--] cmd…` — the self-bridge that gives a sprites
//! (WSS-native) env a CLI exec prefix with no vendor `sprite` CLI. Two modes,
//! chosen by whether both local streams are terminals:
//!
//! - **Stream** (not both stdin and stdout are terminals): the control-plane
//!   READ path — the chrome git/gh/fs reads and the persisted worktree `GitLoc`
//!   shell out through it (the role `vps-ssh` plays for VPS envs). Runs the
//!   command over the native exec API and relays stdout as it arrives, so a
//!   caller like `git -C /workspace status` reads the output exactly as if local.
//! - **TTY** (stdin and stdout are ttys): a long-running agent/session launched as
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
//! Resolves the sprites provider from the WSS-native env whose account owns the
//! sprite, built named with THIS id (exec takes the id as an argument).

use std::io::{IsTerminal as _, Read as _, Write as _};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow};
use termwiz::caps::{Capabilities, ProbeHints};
use termwiz::input::{InputEvent, KeyEvent, MouseButtons, MouseEvent};
use termwiz::terminal::{Terminal, new_terminal};
use thegn_core::config::Config;
use thegn_core::config_env_tables::wss_native_provider_kind;
use thegn_core::sprite_hold::{HOLD_ENV, ceiling_secs, hold_name, wrap_argv};
use thegn_svc::provider::{ExecControl, ExecFrame, ExecSession, ExecSpec, Provider, StreamingExec};

/// Exit code when the remote exec stream was lost and could not be reattached
/// (`EX_TEMPFAIL`): the remote process did not survive. `session list` keys
/// its `cause:remote-exec-lost` field on it.
pub(crate) const EXIT_REMOTE_LOST: i32 = 75;

pub(crate) const WAKE_NOTICE_AFTER: Duration = Duration::from_secs(3);

const STDERR_TAIL_CAP: u64 = 8 * 1024;

fn tty_mode(stdin_tty: bool, stdout_tty: bool) -> bool {
    stdin_tty && stdout_tty
}

/// Await `fut`; if it is still pending after `after`, call `notice` once and
/// keep awaiting.
pub(crate) async fn with_slow_notice<F: std::future::Future>(
    fut: F,
    after: Duration,
    notice: impl FnOnce(),
) -> F::Output {
    tokio::pin!(fut);
    tokio::select! {
        out = &mut fut => out,
        () = tokio::time::sleep(after) => {
            notice();
            fut.await
        }
    }
}

pub(crate) fn sprite_wake_line(id: &str) -> String {
    let id: String = id.chars().filter(|ch| !ch.is_control()).take(64).collect();
    format!("thegn: waiting for sprite {id} to start (a cold sprite can take up to 90 s)...")
}

fn stderr_line(line: &str) {
    let mut stderr = std::io::stderr();
    if writeln!(stderr, "{line}").is_err() {
        tracing::debug!(target: "thegn::provider", "sprite-exec: stderr gone");
    }
}

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

/// Largest stdin message built from queued input (pane.rs STREAM_STDIN_CHUNK).
const STDIN_COALESCE_MAX: usize = 32 << 10;

/// Run `cmd` inside sprite `id` over the native exec API. When stdin and stdout
/// are both terminals, use the streaming PTY bridge; anything else streams.
/// `cmd` arrives already shaped by `GitLoc::provider_command`
/// (`/bin/sh -lc <script>`) or the launch argv; empty falls back to a login
/// shell.
///
/// Keep-awake (THE-2052): `hold` (the `--hold` flag), else [`HOLD_ENV`] in this
/// process's environment (the daemon sets it for headless sprite launches),
/// wraps the argv once in the in-sprite heartbeat
/// ([`thegn_core::sprite_hold::wrap_argv`]). Both modes honour it; a reattach
/// reuses the same remote process and never re-wraps. The env var is read here
/// on the host and is never forwarded into the sprite's exec env.
pub fn run(cfg: &Config, id: &str, cmd: &[String], hold: Option<Duration>) -> Result<()> {
    let env_hold = std::env::var_os(HOLD_ENV);
    let ceiling = hold_ceiling(
        hold,
        env_hold
            .as_deref()
            .map(|raw| raw.to_str().unwrap_or("\u{fffd}")),
    )?;
    let argv: Vec<String> = if cmd.is_empty() {
        vec![
            "/bin/sh".into(),
            "-lc".into(),
            "exec ${SHELL:-/bin/sh} -l".into(),
        ]
    } else {
        cmd.to_vec()
    };
    let argv = match ceiling {
        Some(ceiling) => {
            let task = hold_task()?;
            tracing::debug!(target: "thegn::sprites", sprite = id, task, ceiling, "sprite-exec: keep-awake hold");
            wrap_argv(&argv, &task, ceiling)
        }
        None => argv,
    };
    let record_account = thegn_svc::vps::registry::read(id)
        .filter(|record| record.provider == "sprites")
        .map(|record| record.account);
    let mut candidates = Vec::new();
    let mut providers = Vec::new();
    for (name, envc) in cfg
        .env
        .iter()
        .filter(|(_, envc)| wss_native_provider_kind(&envc.provider.provider))
    {
        let provider = crate::provider_factory::provider_for_named(&envc.provider, id);
        let account = provider.as_ref().and_then(|provider| match provider {
            Provider::Sprites(provider) => provider.scope().map(|scope| scope.account.clone()),
            _ => None,
        });
        candidates.push((name.clone(), account));
        providers.push(provider);
    }
    let selected = pick_sprites_env_named(id, &candidates, record_account.as_deref())
        .map_err(|error| anyhow!(error))?;
    let provider = providers
        .into_iter()
        .nth(selected)
        .flatten()
        .ok_or_else(|| anyhow!("sprite-exec: selected Sprites env has no resolvable API token"))?;
    if tty_mode(
        std::io::stdin().is_terminal(),
        std::io::stdout().is_terminal(),
    ) {
        let code = run_tty(&provider, id, argv)?;
        std::process::exit(code);
    }
    // Current-thread: one network call does not need a worker per core.
    // See `agent::provider_runtime`.
    let rt = crate::agent::provider_runtime()?;
    let code = rt.block_on(run_streaming(&provider, id, argv))?;
    std::process::exit(code);
}

// ---------------------------------------------------------------------------
// Pure decisions (unit-tested). The streaming below only wires them up.
// ---------------------------------------------------------------------------

/// Which candidate env serves a sprite. `candidates` contains the env name and
/// the provider account fingerprint, when that provider could be built.
/// `record_account` is the account from the sprite ledger record. Errors name
/// envs only; they never include a token or credential reference.
#[cfg(test)]
pub(crate) fn pick_sprites_env(
    candidates: &[(String, Option<String>)],
    record_account: Option<&str>,
) -> Result<usize, String> {
    pick_sprites_env_named("the requested sprite", candidates, record_account)
}

fn pick_sprites_env_named(
    id: &str,
    candidates: &[(String, Option<String>)],
    record_account: Option<&str>,
) -> Result<usize, String> {
    let names = candidates
        .iter()
        .map(|(name, _)| name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    if let Some(record_account) = record_account {
        let matches = candidates
            .iter()
            .enumerate()
            .filter(|(_, (_, account))| account.as_deref() == Some(record_account))
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        return match matches.as_slice() {
            [index] => Ok(*index),
            _ => Err(format!(
                "sprite-exec: cannot match the Sprites ledger account to exactly one env ({names})"
            )),
        };
    }
    match candidates {
        [_] => Ok(0),
        _ => Err(format!(
            "sprite-exec: cannot tell which Sprites account owns {id}; it has no ledger record and {} Sprites envs are configured ({names})",
            candidates.len()
        )),
    }
}

/// The hold ceiling in seconds for this run, or `None` for an unheld run: the
/// `--hold` flag wins, else the [`HOLD_ENV`] value (`env`, as read from the
/// host environment; empty counts as unset). Both are capped by
/// [`ceiling_secs`]. A set value that is not a positive whole number of
/// seconds is refused rather than silently running unheld.
pub(crate) fn hold_ceiling(flag: Option<Duration>, env: Option<&str>) -> Result<Option<u64>> {
    if let Some(flag) = flag {
        return Ok(Some(ceiling_secs(Some(flag.as_secs().max(1)), 0)));
    }
    let Some(raw) = env.map(str::trim).filter(|raw| !raw.is_empty()) else {
        return Ok(None);
    };
    let secs = Some(raw)
        .filter(|raw| raw.bytes().all(|byte| byte.is_ascii_digit()))
        .and_then(|raw| raw.parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .ok_or_else(|| {
            anyhow!("sprite-exec: {HOLD_ENV}={raw:?} is not a positive number of seconds")
        })?;
    Ok(Some(ceiling_secs(Some(secs), 0)))
}

/// A fresh hold task name, `thegn-hold-<16 hex>`, from the OS CSPRNG.
fn hold_task() -> Result<String> {
    let mut nonce = [0_u8; 8];
    getrandom::fill(&mut nonce).map_err(|error| anyhow!("sprite-exec: hold nonce: {error}"))?;
    hold_task_from(nonce)
}

/// The task name for `nonce` (split out so the encoding is testable).
fn hold_task_from(nonce: [u8; 8]) -> Result<String> {
    let hex: String = nonce.iter().map(|byte| format!("{byte:02x}")).collect();
    hold_name(&hex).ok_or_else(|| anyhow!("sprite-exec: hold nonce {hex:?} is not 16 hex digits"))
}

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

fn tty_env(get: &dyn Fn(&str) -> Option<String>) -> Vec<(String, String)> {
    let mut env = Vec::new();
    let term = get("TERM").unwrap_or_default();
    let colorterm = get("COLORTERM").filter(|value| value == "truecolor" || value == "24bit");
    let locale = crate::remote_term::host_locale(get);
    crate::remote_term::apply(
        &mut env,
        &thegn_core::config::remote_safe_term(&term),
        colorterm.as_deref(),
        locale.as_deref(),
    );
    env
}

/// The terminal modes the REMOTE app set through its output that change how a
/// decoded local input must be re-encoded: DECCKM application cursor keys
/// (`CSI ? 1 h/l`), bracketed paste (`CSI ? 2004 h/l`), and mouse reporting
/// (`CSI ? 9/1000/1002/1003/1006 h/l`). Sequences may split across frames, so
/// an unfinished `CSI ?` sequence is carried over.
#[derive(Debug, Default)]
pub(crate) struct RemoteModes {
    pub(crate) app_cursor: bool,
    pub(crate) bracketed_paste: bool,
    pub(crate) mouse: crate::emulator::MouseMode,
    pub(crate) sgr_mouse: bool,
    mouse_left_down: bool,
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
                b"9" => {
                    self.mouse = if on {
                        crate::emulator::MouseMode::Press
                    } else {
                        crate::emulator::MouseMode::None
                    }
                }
                b"1000" => {
                    self.mouse = if on {
                        crate::emulator::MouseMode::PressRelease
                    } else {
                        crate::emulator::MouseMode::None
                    }
                }
                b"1002" => {
                    self.mouse = if on {
                        crate::emulator::MouseMode::ButtonMotion
                    } else {
                        crate::emulator::MouseMode::None
                    }
                }
                b"1003" => {
                    self.mouse = if on {
                        crate::emulator::MouseMode::AnyMotion
                    } else {
                        crate::emulator::MouseMode::None
                    }
                }
                b"1006" => self.sgr_mouse = on,
                _ => {}
            }
        }
    }
}

/// What the blocking tty thread forwards to the async pump.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FromTty {
    Key(KeyEvent),
    Mouse(MouseEvent),
    Paste(String),
    Resize { cols: u16, rows: u16 },
}

/// The stdin bytes for one decoded local input event, encoded for the remote
/// app's current modes. `None` for an event with no byte form (or a resize).
pub(crate) fn encode_input(ev: &FromTty, modes: &mut RemoteModes) -> Option<Vec<u8>> {
    match ev {
        FromTty::Key(k) => crate::input::key_bytes_mode(&k.key, k.modifiers, modes.app_cursor),
        FromTty::Mouse(mouse) => {
            if modes.mouse == crate::emulator::MouseMode::None {
                modes.mouse_left_down = false;
                return None;
            }
            let left = mouse.mouse_buttons.contains(MouseButtons::LEFT);
            let pane_mouse = if mouse.mouse_buttons.contains(MouseButtons::VERT_WHEEL) {
                if mouse.mouse_buttons.contains(MouseButtons::WHEEL_POSITIVE) {
                    crate::input::PaneMouse::WheelUp
                } else {
                    crate::input::PaneMouse::WheelDown
                }
            } else if left && !modes.mouse_left_down {
                crate::input::PaneMouse::Press(0)
            } else if left && modes.mouse_left_down {
                crate::input::PaneMouse::Drag(0)
            } else if !left && modes.mouse_left_down {
                crate::input::PaneMouse::Release(0)
            } else {
                crate::input::PaneMouse::Move
            };
            modes.mouse_left_down = left;
            // termwiz reports the outer terminal's 1-based SGR cell; encode_mouse wants 0-based (run.rs does the same)
            crate::input::encode_mouse(
                pane_mouse,
                modes.mouse,
                modes.sgr_mouse,
                mouse.x.saturating_sub(1),
                mouse.y.saturating_sub(1),
            )
        }
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

fn is_lone_escape(ev: &FromTty, bytes: &[u8]) -> bool {
    matches!(ev, FromTty::Key(_)) && bytes == [0x1b]
}

/// One coalesced batch: the bytes to send, a resize to send after them, and a
/// lone Escape held back for its own message.
pub(crate) type CoalescedInput = (Vec<u8>, Option<(u16, u16)>, Option<Vec<u8>>);

/// Encode `first` plus every input event already queued behind it as ONE
/// stdin write, up to STDIN_COALESCE_MAX bytes (a single larger event, e.g.
/// a bracketed paste, is sent whole). A lone Escape is returned as a separate
/// message so it cannot merge with the next key. Stops at a resize and returns
/// it so the caller sends it AFTER the bytes; events after the resize stay
/// queued.
pub(crate) fn coalesce_input(
    first: &FromTty,
    rx: &mut tokio::sync::mpsc::Receiver<FromTty>,
    modes: &mut RemoteModes,
) -> CoalescedInput {
    let mut bytes = encode_input(first, modes).unwrap_or_default();
    if is_lone_escape(first, &bytes) {
        return (bytes, None, None);
    }
    while bytes.len() < STDIN_COALESCE_MAX {
        match rx.try_recv() {
            Ok(FromTty::Resize { cols, rows }) => return (bytes, Some((cols, rows)), None),
            Ok(ev) => {
                if let Some(b) = encode_input(&ev, modes) {
                    if is_lone_escape(&ev, &b) {
                        return (bytes, None, Some(b));
                    }
                    bytes.extend_from_slice(&b);
                }
            }
            Err(_) => break,
        }
    }
    (bytes, None, None)
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

/// Stream a non-tty exec: stdout as it arrives (no cap), remote stderr to
/// stderr at the end, a non-terminal stdin forwarded then half-closed.
async fn run_streaming(provider: &Provider, id: &str, argv: Vec<String>) -> Result<i32> {
    let spec = ExecSpec {
        argv,
        tty: false,
        cols: 0,
        rows: 0,
        env: Vec::new(),
        cwd: None,
    };
    let StreamingExec {
        session,
        stderr,
        verdict,
    } = provider.open_exec_streaming(id, &spec).await?;
    let ExecSession {
        mut frames,
        control,
        ..
    } = session;

    if !std::io::stdin().is_terminal() {
        let input_control = control.clone();
        thegn_core::thread_name::spawn_named("thegn-sprite-in", move || {
            let mut input = std::io::stdin().lock();
            let mut chunk = [0u8; 32 * 1024];
            loop {
                match input.read(&mut chunk) {
                    Ok(0) => break,
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(error) => {
                        tracing::debug!(
                            target: "thegn::provider",
                            error = %error,
                            "sprite-exec: stdin read failed; sending EOF"
                        );
                        break;
                    }
                    Ok(len) => {
                        if input_control
                            .blocking_send(ExecControl::Stdin(chunk[..len].to_vec()))
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }
        });
    }
    drop(control);

    let frame_result = {
        let stdout = std::io::stdout();
        let mut stdout = stdout.lock();
        stream_frames(&mut frames, &mut stdout).await
    };
    let verdict_result = tokio::time::timeout(Duration::from_secs(3), verdict).await;
    let result = match verdict_result {
        Ok(Ok(Some(fault))) => Err(anyhow::Error::new(fault)),
        _ => frame_result.map_err(anyhow::Error::msg),
    };
    let rendered = stderr.render();
    let total = stderr.total();
    if total > STDERR_TAIL_CAP || !rendered.is_empty() {
        let mut output = std::io::stderr();
        if total > STDERR_TAIL_CAP {
            let marker = format!(
                "[thegn: remote stderr, last {STDERR_TAIL_CAP} of {} bytes]\n",
                total
            );
            if output.write_all(marker.as_bytes()).is_err() {
                tracing::debug!(target: "thegn::provider", "sprite-exec: stderr gone");
            }
        }
        if !rendered.is_empty() && output.write_all(rendered.as_bytes()).is_err() {
            tracing::debug!(target: "thegn::provider", "sprite-exec: stderr gone");
        }
    }
    result
}

async fn stream_frames(
    frames: &mut tokio::sync::mpsc::Receiver<ExecFrame>,
    out: &mut dyn std::io::Write,
) -> std::result::Result<i32, String> {
    while let Some(frame) = frames.recv().await {
        match frame {
            ExecFrame::Stdout(bytes) => {
                if let Err(error) = out.write_all(&bytes).and_then(|()| out.flush()) {
                    return if error.kind() == std::io::ErrorKind::BrokenPipe {
                        Ok(141)
                    } else {
                        Err(error.to_string())
                    };
                }
            }
            ExecFrame::Exit(code) => {
                if let Err(error) = out.flush() {
                    return if error.kind() == std::io::ErrorKind::BrokenPipe {
                        Ok(141)
                    } else {
                        Err(error.to_string())
                    };
                }
                return Ok(code);
            }
        }
    }
    Err("exec stream ended without an exit status".to_string())
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
    let env = tty_env(&|key| std::env::var(key).ok());

    let rt = crate::agent::provider_runtime()?;
    let spec = ExecSpec {
        argv,
        tty: true,
        cols,
        rows,
        env,
        cwd: None,
    };
    // Open BEFORE raw mode so an open failure never leaves the tty altered.
    let session = rt.block_on(with_slow_notice(
        provider.open_exec(id, &spec),
        WAKE_NOTICE_AFTER,
        || stderr_line(&sprite_wake_line(id)),
    ))?;

    term.set_raw_mode().context("raw mode")?;
    // A burst of per-key events can then coalesce into one message of up to about 4 KiB.
    let (tty_tx, tty_rx) = tokio::sync::mpsc::channel::<FromTty>(4096);
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
    struct PumpStats {
        frames: u64,
        bytes: u64,
        write: Duration,
        max_write: Duration,
    }

    let mut stats = PumpStats {
        frames: 0,
        bytes: 0,
        write: Duration::ZERO,
        max_write: Duration::ZERO,
    };
    let log_stats = |stats: &PumpStats| {
        tracing::debug!(
            target: "thegn::provider",
            frames = stats.frames,
            bytes = stats.bytes,
            write_ms = stats.write.as_millis() as u64,
            max_write_ms = stats.max_write.as_millis() as u64,
            "sprite-exec output pump"
        );
    };
    let mut out = std::io::stdout();
    loop {
        tokio::select! {
            frame = session.frames.recv() => match frame {
                Some(ExecFrame::Stdout(bytes)) => {
                    modes.observe(&bytes);
                    let began = Instant::now();
                    let result = out.write_all(&bytes).and_then(|()| out.flush());
                    let elapsed = began.elapsed();
                    stats.frames += 1;
                    stats.bytes += bytes.len() as u64;
                    stats.write += elapsed;
                    stats.max_write = stats.max_write.max(elapsed);
                    if result.is_err() {
                        log_stats(&stats);
                        return StreamEnd::LocalGone;
                    }
                }
                Some(ExecFrame::Exit(code)) => {
                    log_stats(&stats);
                    return StreamEnd::Exit(code);
                }
                None => {
                    log_stats(&stats);
                    return StreamEnd::Dropped;
                }
            },
            ev = tty_rx.recv() => match ev {
                Some(FromTty::Resize { cols, rows }) => {
                    *size = (cols, rows);
                    let _ = session.control.send(ExecControl::Resize { cols, rows }).await; // best-effort: send: a closed driver surfaces as the frame stream ending
                }
                Some(ev) => {
                    let (bytes, resize, isolated_escape) = coalesce_input(&ev, tty_rx, modes);
                    if !bytes.is_empty()
                        && session
                            .control
                            .send(ExecControl::Stdin(bytes))
                            .await
                            .is_err()
                    {
                        tracing::debug!(target: "thegn::provider", "sprite-exec: stdin not delivered; the stream is ending");
                    }
                    if let Some(bytes) = isolated_escape
                        && session
                            .control
                            .send(ExecControl::Stdin(bytes))
                            .await
                            .is_err()
                    {
                        tracing::debug!(target: "thegn::provider", "sprite-exec: isolated escape not delivered; the stream is ending");
                    }
                    if let Some((cols, rows)) = resize {
                        *size = (cols, rows);
                        if session
                            .control
                            .send(ExecControl::Resize { cols, rows })
                            .await
                            .is_err()
                        {
                            tracing::debug!(target: "thegn::provider", "sprite-exec: resize not delivered; the stream is ending");
                        }
                    }
                }
                None => {
                    log_stats(&stats);
                    return StreamEnd::LocalGone;
                }
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
            Ok(Some(InputEvent::Mouse(m))) => FromTty::Mouse(m),
            Ok(Some(InputEvent::Paste(text))) => FromTty::Paste(text),
            Ok(Some(InputEvent::Resized { cols, rows })) => {
                let (cols, rows) = pty_size(cols, rows);
                FromTty::Resize { cols, rows }
            }
            // Wake / pixel mouse (not forwarded): re-check `done`.
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
        EXIT_REMOTE_LOST, ExecFrame, FromTty, REATTACH_BUDGET_RESET, RemoteModes,
        WAKE_NOTICE_AFTER, attempts_after_stream, coalesce_input, encode_input, hold_ceiling,
        hold_task_from, lost_line, pick_sprites_env, pty_size, reconnect_delays, should_reattach,
        stream_frames, tty_env, tty_mode, with_slow_notice,
    };
    use std::cell::Cell;
    use std::time::Duration;
    use termwiz::input::{KeyCode, KeyEvent, Modifiers, MouseButtons, MouseEvent};
    use thegn_core::sprite_hold::{HOLD_ENV, HOLD_MAX_CEILING_SECS, HOLD_SCRIPT, wrap_argv};

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

    #[tokio::test(start_paused = true)]
    async fn slow_future_prints_the_notice_once() {
        let notices = Cell::new(0);
        let output = with_slow_notice(
            async {
                tokio::time::sleep(Duration::from_secs(10)).await;
                "done"
            },
            WAKE_NOTICE_AFTER,
            || notices.set(notices.get() + 1),
        )
        .await;

        assert_eq!(output, "done");
        assert_eq!(notices.get(), 1);
    }

    #[tokio::test]
    async fn fast_future_prints_nothing() {
        let notices = Cell::new(0);
        let output = with_slow_notice(async { "done" }, WAKE_NOTICE_AFTER, || {
            notices.set(notices.get() + 1)
        })
        .await;

        assert_eq!(output, "done");
        assert_eq!(notices.get(), 0);
    }

    #[test]
    fn tty_mode_needs_both_terminals() {
        for (stdin_tty, stdout_tty, expected) in [
            (false, false, false),
            (false, true, false),
            (true, false, false),
            (true, true, true),
        ] {
            assert_eq!(tty_mode(stdin_tty, stdout_tty), expected);
        }
    }

    #[tokio::test]
    async fn stream_frames_passes_three_mib_through_without_a_marker() {
        let (tx, mut frames) = tokio::sync::mpsc::channel(4);
        let chunk = vec![b'x'; 1 << 20];
        for _ in 0..3 {
            tx.send(ExecFrame::Stdout(chunk.clone())).await.unwrap();
        }
        tx.send(ExecFrame::Exit(0)).await.unwrap();
        drop(tx);

        let mut output = Vec::new();
        assert_eq!(stream_frames(&mut frames, &mut output).await, Ok(0));
        assert_eq!(output, vec![b'x'; 3 << 20]);
        assert!(!output.starts_with(b"[thegn:"));
    }

    struct FlushCountingWriter {
        flushes: usize,
    }

    impl std::io::Write for FlushCountingWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            self.flushes += 1;
            Ok(())
        }
    }

    struct BrokenPipeWriter;

    impl std::io::Write for BrokenPipeWriter {
        fn write(&mut self, _bytes: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn stream_frames_flushes_each_frame() {
        let (tx, mut frames) = tokio::sync::mpsc::channel(4);
        tx.send(ExecFrame::Stdout(b"one".to_vec())).await.unwrap();
        tx.send(ExecFrame::Stdout(b"two".to_vec())).await.unwrap();
        tx.send(ExecFrame::Exit(0)).await.unwrap();

        let mut output = FlushCountingWriter { flushes: 0 };
        assert_eq!(stream_frames(&mut frames, &mut output).await, Ok(0));
        assert!(output.flushes >= 2);
    }

    #[tokio::test]
    async fn stream_frames_stops_on_a_closed_pipe() {
        let (tx, mut frames) = tokio::sync::mpsc::channel(4);
        tx.send(ExecFrame::Stdout(b"first".to_vec())).await.unwrap();
        tx.send(ExecFrame::Stdout(b"second".to_vec()))
            .await
            .unwrap();
        drop(tx);

        let mut output = BrokenPipeWriter;
        assert_eq!(stream_frames(&mut frames, &mut output).await, Ok(141));
    }

    #[tokio::test]
    async fn stream_frames_returns_the_remote_code() {
        let (tx, mut frames) = tokio::sync::mpsc::channel(1);
        tx.send(ExecFrame::Exit(3)).await.unwrap();

        let mut output = Vec::new();
        assert_eq!(stream_frames(&mut frames, &mut output).await, Ok(3));
    }

    #[tokio::test]
    async fn stream_frames_without_exit_is_an_error() {
        let (tx, mut frames) = tokio::sync::mpsc::channel(1);
        tx.send(ExecFrame::Stdout(b"partial".to_vec()))
            .await
            .unwrap();
        drop(tx);

        let mut output = Vec::new();
        assert_eq!(
            stream_frames(&mut frames, &mut output).await,
            Err("exec stream ended without an exit status".to_string())
        );
    }

    #[test]
    fn pick_sprites_env_table() {
        let candidates = vec![
            ("sprites-a".to_string(), Some("acct-a".to_string())),
            ("sprites-b".to_string(), Some("acct-b".to_string())),
        ];
        assert_eq!(pick_sprites_env(&candidates, Some("acct-b")), Ok(1));
        assert!(pick_sprites_env(&candidates, Some("acct-missing")).is_err());

        let duplicate = vec![
            ("sprites-a".to_string(), Some("acct-a".to_string())),
            ("sprites-b".to_string(), Some("acct-a".to_string())),
        ];
        assert!(pick_sprites_env(&duplicate, Some("acct-a")).is_err());

        let one = vec![("sprites-a".to_string(), None)];
        assert_eq!(pick_sprites_env(&one, None), Ok(0));
        let error = pick_sprites_env(&candidates, None).expect_err("ambiguous envs");
        assert!(
            error.contains("sprites-a") && error.contains("sprites-b"),
            "{error}"
        );
    }

    #[test]
    fn pick_sprites_env_errors_never_include_a_token() {
        let _env = thegn_core::testenv::EnvGuard::set(&[("SPRITES_TOKEN", "token-sentinel")]);
        let candidates = vec![
            ("sprites-a".to_string(), Some("acct-a".to_string())),
            ("sprites-b".to_string(), Some("acct-b".to_string())),
        ];
        for result in [
            pick_sprites_env(&candidates, Some("acct-missing")),
            pick_sprites_env(&candidates, None),
        ] {
            let error = result.expect_err("ambiguous account");
            assert!(!error.contains("token-sentinel"), "{error}");
        }
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
    fn tty_env_builds_remote_terminal_identity_from_injected_host_values() {
        let env = tty_env(&|key| match key {
            "TERM" => Some("xterm-ghostty".into()),
            "COLORTERM" => Some("truecolor".into()),
            "LANG" => Some("en_US.UTF-8".into()),
            _ => None,
        });
        assert_eq!(
            env,
            vec![
                ("TERM".into(), "xterm-256color".into()),
                ("COLORTERM".into(), "truecolor".into()),
                ("LANG".into(), "C.UTF-8".into()),
            ]
        );

        let env = tty_env(&|key| match key {
            "TERM" => Some("tmux-256color".into()),
            _ => None,
        });
        assert_eq!(env, vec![("TERM".into(), "tmux-256color".into())]);
    }

    #[test]
    fn encode_input_honors_remote_modes() {
        let up = FromTty::Key(KeyEvent {
            key: KeyCode::UpArrow,
            modifiers: Modifiers::NONE,
        });
        let mut m = RemoteModes::default();
        assert_eq!(encode_input(&up, &mut m).as_deref(), Some(&b"\x1b[A"[..]));
        m.observe(b"\x1b[?1h");
        assert_eq!(encode_input(&up, &mut m).as_deref(), Some(&b"\x1bOA"[..]));

        let paste = FromTty::Paste("ls".into());
        assert_eq!(encode_input(&paste, &mut m).as_deref(), Some(&b"ls"[..]));
        m.observe(b"\x1b[?2004h");
        assert_eq!(
            encode_input(&paste, &mut m).as_deref(),
            Some(&b"\x1b[200~ls\x1b[201~"[..])
        );
        assert_eq!(
            encode_input(&FromTty::Resize { cols: 1, rows: 1 }, &mut m),
            None
        );
    }

    fn mouse(x: u16, y: u16, mouse_buttons: MouseButtons) -> FromTty {
        FromTty::Mouse(MouseEvent {
            x,
            y,
            mouse_buttons,
            modifiers: Modifiers::NONE,
        })
    }

    #[tokio::test]
    async fn coalesce_input_combines_keys_and_unbracketed_paste() {
        let first = FromTty::Key(KeyEvent {
            key: KeyCode::Char('a'),
            modifiers: Modifiers::NONE,
        });
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        tx.send(FromTty::Key(KeyEvent {
            key: KeyCode::Char('b'),
            modifiers: Modifiers::NONE,
        }))
        .await
        .unwrap();
        tx.send(FromTty::Paste("cd".into())).await.unwrap();

        let (bytes, resize, isolated_escape) =
            coalesce_input(&first, &mut rx, &mut RemoteModes::default());
        assert_eq!(bytes, b"abcd");
        assert_eq!(resize, None);
        assert_eq!(isolated_escape, None);
    }

    #[tokio::test]
    async fn coalesce_input_stops_at_resize_and_leaves_following_input_queued() {
        let first = FromTty::Key(KeyEvent {
            key: KeyCode::Char('x'),
            modifiers: Modifiers::NONE,
        });
        let third = FromTty::Key(KeyEvent {
            key: KeyCode::Char('z'),
            modifiers: Modifiers::NONE,
        });
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        tx.send(FromTty::Resize {
            cols: 100,
            rows: 30,
        })
        .await
        .unwrap();
        tx.send(third.clone()).await.unwrap();

        let (bytes, resize, isolated_escape) =
            coalesce_input(&first, &mut rx, &mut RemoteModes::default());
        assert_eq!(bytes, b"x");
        assert_eq!(resize, Some((100, 30)));
        assert_eq!(isolated_escape, None);
        assert_eq!(rx.try_recv().unwrap(), third);
    }

    #[tokio::test]
    async fn coalesce_input_caps_queued_pastes_but_sends_one_large_paste_whole() {
        let paste = || FromTty::Paste("x".repeat(8 * 1024));
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        for _ in 0..5 {
            tx.send(paste()).await.unwrap();
        }

        let (bytes, resize, isolated_escape) =
            coalesce_input(&paste(), &mut rx, &mut RemoteModes::default());
        assert_eq!(bytes.len(), 32 * 1024);
        assert_eq!(resize, None);
        assert_eq!(isolated_escape, None);
        assert!(matches!(rx.try_recv(), Ok(FromTty::Paste(text)) if text.len() == 8 * 1024));
        assert!(matches!(rx.try_recv(), Ok(FromTty::Paste(text)) if text.len() == 8 * 1024));
        assert!(rx.try_recv().is_err());

        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        drop(tx);
        let mut modes = RemoteModes::default();
        modes.observe(b"\x1b[?2004h");
        let (bytes, resize, isolated_escape) =
            coalesce_input(&FromTty::Paste("x".repeat(64 * 1024)), &mut rx, &mut modes);
        assert_eq!(bytes.len(), 64 * 1024 + b"\x1b[200~\x1b[201~".len());
        assert!(bytes.starts_with(b"\x1b[200~"));
        assert!(bytes.ends_with(b"\x1b[201~"));
        assert_eq!(resize, None);
        assert_eq!(isolated_escape, None);
    }

    #[tokio::test]
    async fn coalesce_input_ignores_mouse_when_mouse_mode_is_disabled() {
        let first = mouse(1, 1, MouseButtons::LEFT);
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        tx.send(FromTty::Key(KeyEvent {
            key: KeyCode::Char('x'),
            modifiers: Modifiers::NONE,
        }))
        .await
        .unwrap();

        let (bytes, resize, isolated_escape) =
            coalesce_input(&first, &mut rx, &mut RemoteModes::default());
        assert_eq!(bytes, b"x");
        assert_eq!(resize, None);
        assert_eq!(isolated_escape, None);
    }

    #[tokio::test]
    async fn coalesce_input_with_an_empty_queue_returns_the_first_event_only() {
        let first = FromTty::Key(KeyEvent {
            key: KeyCode::Char('q'),
            modifiers: Modifiers::NONE,
        });
        let (_tx, mut rx) = tokio::sync::mpsc::channel(64);

        let (bytes, resize, isolated_escape) =
            coalesce_input(&first, &mut rx, &mut RemoteModes::default());
        assert_eq!(bytes, b"q");
        assert_eq!(resize, None);
        assert_eq!(isolated_escape, None);
    }

    #[tokio::test]
    async fn coalesce_input_keeps_escape_separate_from_following_key() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        tx.send(FromTty::Key(KeyEvent {
            key: KeyCode::Char('j'),
            modifiers: Modifiers::NONE,
        }))
        .await
        .unwrap();

        let escape = FromTty::Key(KeyEvent {
            key: KeyCode::Escape,
            modifiers: Modifiers::NONE,
        });
        let (first, resize, isolated_escape) =
            coalesce_input(&escape, &mut rx, &mut RemoteModes::default());
        assert_eq!(first, b"\x1b");
        assert_eq!(resize, None);
        assert_eq!(isolated_escape, None);
        let next = rx.try_recv().unwrap();
        let (second, resize, isolated_escape) =
            coalesce_input(&next, &mut rx, &mut RemoteModes::default());
        assert_eq!(second, b"j");
        assert_eq!(resize, None);
        assert_eq!(isolated_escape, None);
    }

    #[tokio::test]
    async fn coalesce_input_keeps_queued_escape_as_its_own_message() {
        let first = FromTty::Key(KeyEvent {
            key: KeyCode::Char('a'),
            modifiers: Modifiers::NONE,
        });
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        tx.send(FromTty::Key(KeyEvent {
            key: KeyCode::Escape,
            modifiers: Modifiers::NONE,
        }))
        .await
        .unwrap();
        tx.send(FromTty::Key(KeyEvent {
            key: KeyCode::Char('j'),
            modifiers: Modifiers::NONE,
        }))
        .await
        .unwrap();

        let (first_bytes, resize, isolated_escape) =
            coalesce_input(&first, &mut rx, &mut RemoteModes::default());
        assert_eq!(first_bytes, b"a");
        assert_eq!(resize, None);
        assert_eq!(isolated_escape, Some(b"\x1b".to_vec()));

        let next = rx.try_recv().unwrap();
        assert!(matches!(
            next,
            FromTty::Key(KeyEvent {
                key: KeyCode::Char('j'),
                ..
            })
        ));
    }

    #[tokio::test]
    async fn coalesce_input_keeps_alt_key_and_arrows_intact() {
        let alt_j = FromTty::Key(KeyEvent {
            key: KeyCode::Char('j'),
            modifiers: Modifiers::ALT,
        });
        let (_tx, mut rx) = tokio::sync::mpsc::channel(64);
        let (bytes, resize, isolated_escape) =
            coalesce_input(&alt_j, &mut rx, &mut RemoteModes::default());
        assert_eq!(bytes, b"\x1bj");
        assert_eq!(resize, None);
        assert_eq!(isolated_escape, None);

        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        tx.send(FromTty::Key(KeyEvent {
            key: KeyCode::UpArrow,
            modifiers: Modifiers::NONE,
        }))
        .await
        .unwrap();
        let up = FromTty::Key(KeyEvent {
            key: KeyCode::UpArrow,
            modifiers: Modifiers::NONE,
        });
        let (bytes, resize, isolated_escape) =
            coalesce_input(&up, &mut rx, &mut RemoteModes::default());
        assert_eq!(bytes, b"\x1b[A\x1b[A");
        assert_eq!(resize, None);
        assert_eq!(isolated_escape, None);
    }

    #[test]
    fn encode_input_honors_remote_mouse_modes_and_sgr() {
        let mut m = RemoteModes::default();
        let press = mouse(5, 10, MouseButtons::LEFT);
        let release = mouse(5, 10, MouseButtons::NONE);
        let wheel = mouse(
            5,
            10,
            MouseButtons::VERT_WHEEL | MouseButtons::WHEEL_POSITIVE,
        );
        assert_eq!(encode_input(&press, &mut m), None);

        m.observe(b"\x1b[?1000h\x1b[?1006h");
        assert_eq!(
            encode_input(&press, &mut m).as_deref(),
            Some(&b"\x1b[<0;5;10M"[..])
        );
        assert_eq!(
            encode_input(&release, &mut m).as_deref(),
            Some(&b"\x1b[<0;5;10m"[..])
        );
        assert_eq!(
            encode_input(&wheel, &mut m).as_deref(),
            Some(&b"\x1b[<64;5;10M"[..])
        );
    }

    #[test]
    fn encode_input_uses_legacy_mouse_when_sgr_is_off() {
        let mut m = RemoteModes::default();
        m.observe(b"\x1b[?1000h");
        assert_eq!(
            encode_input(&mouse(1, 1, MouseButtons::LEFT), &mut m),
            Some(vec![0x1b, b'[', b'M', 32, 33, 33])
        );
    }

    #[test]
    fn encode_input_any_motion_reports_move_and_split_modes_apply() {
        let mut m = RemoteModes::default();
        m.observe(b"\x1b[?100");
        assert!(!m.sgr_mouse);
        m.observe(b"6h");
        assert!(m.sgr_mouse);
        m.observe(b"\x1b[?1003h");
        assert_eq!(
            encode_input(&mouse(2, 3, MouseButtons::NONE), &mut m).as_deref(),
            Some(&b"\x1b[<35;2;3M"[..])
        );
    }

    #[test]
    fn hold_ceiling_prefers_the_flag_then_the_env() {
        let flag = Some(Duration::from_secs(90));
        assert_eq!(hold_ceiling(flag, Some("garbage")).unwrap(), Some(90));
        assert_eq!(
            hold_ceiling(Some(Duration::from_secs(48 * 3600)), None).unwrap(),
            Some(HOLD_MAX_CEILING_SECS)
        );
        // A sub-second flag still holds for at least a second.
        assert_eq!(
            hold_ceiling(Some(Duration::from_millis(10)), None).unwrap(),
            Some(1)
        );
        assert_eq!(hold_ceiling(None, Some("14400")).unwrap(), Some(14400));
        assert_eq!(hold_ceiling(None, Some(" 600\n")).unwrap(), Some(600));
        assert_eq!(
            hold_ceiling(None, Some("999999999")).unwrap(),
            Some(HOLD_MAX_CEILING_SECS)
        );
    }

    #[test]
    fn hold_ceiling_unset_or_empty_env_runs_unheld() {
        assert_eq!(hold_ceiling(None, None).unwrap(), None);
        assert_eq!(hold_ceiling(None, Some("")).unwrap(), None);
        assert_eq!(hold_ceiling(None, Some("  ")).unwrap(), None);
    }

    #[test]
    fn hold_ceiling_refuses_garbage_env() {
        for raw in [
            "0",
            "-5",
            "+5",
            "4h",
            "1.5",
            "abc",
            "\u{fffd}",
            "99999999999999999999999",
        ] {
            let error = hold_ceiling(None, Some(raw)).expect_err(raw).to_string();
            assert!(error.contains(HOLD_ENV), "{raw}: {error}");
        }
    }

    #[test]
    fn hold_task_is_the_lowercase_hex_name() {
        assert_eq!(
            hold_task_from([0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef]).unwrap(),
            "thegn-hold-0123456789abcdef"
        );
        assert_eq!(
            hold_task_from([0; 8]).unwrap(),
            "thegn-hold-0000000000000000"
        );
    }

    #[test]
    fn a_held_argv_never_carries_the_host_hold_env() {
        let argv = vec!["claude".to_owned(), "-p".to_owned(), "do it".to_owned()];
        let wrapped = wrap_argv(&argv, "thegn-hold-0123456789abcdef", 600);
        assert_eq!(wrapped[2], HOLD_SCRIPT);
        assert_eq!(&wrapped[6..], argv.as_slice());
        assert!(wrapped.iter().all(|arg| !arg.contains(HOLD_ENV)));
    }
}
