//! `thegn vercel-exec <id> -- cmd…`: the Vercel Sandbox exec self-bridge (the
//! `sprite-exec` role for a sandbox with no vendor CLI and no ssh). Capture
//! mode (non-tty stdin) runs the command and relays its output without ever
//! resuming a stopped sandbox; tty mode resumes and streams the
//! `/interactive` PTY, falling back to stream mode. The token is resolved
//! in-process, never passed in argv or the environment.
//!
//! - **Capture** (stdin not a tty): chrome git/fs reads. `run_exec` under
//!   [`Wake::Never`]; a stopped sandbox exits [`EXIT_STOPPED`] with
//!   [`STOPPED_LINE`] on stderr, so a read never buys compute. No keep-alive.
//! - **TTY** (panes, daemon headless sessions): raw mode, `open_exec` (the
//!   `/interactive` PTY), else `open_stream` with [`STREAM_NOTICE`] once.
//!   While the session runs, the `thegn-vx-keep` thread extends the Vercel
//!   session timeout when due; it is the only extension timer anywhere.
//!   Vercel has no reattach: a stream lost without an exit means the remote
//!   process is gone, so the bridge exits [`EXIT_STOPPED`] (75) with one line.
//!
//! The exec environment is `TERM` plus [`ENV_ALLOWLIST`] only: no token, no
//! `VERCEL_*`, no other host variable reaches the sandbox.

use std::io::{IsTerminal as _, Write as _};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow};
use termwiz::caps::{Capabilities, ProbeHints};
use termwiz::input::InputEvent;
use termwiz::terminal::{Terminal, new_terminal};
use thegn_core::config::{Config, EnvProviderKind};
use thegn_core::vercel::api::SessionStatus;
use thegn_svc::provider::{ExecControl, ExecFrame, ExecSession, ExecSpec};
use thegn_svc::vercel::{VercelProvider, Wake};

use crate::sprite_bridge::{FromTty, RemoteModes, encode_input, pty_size};

/// Exit code for a stopped sandbox (capture) and a lost stream (tty):
/// `EX_TEMPFAIL`, the same code `sprite-exec` uses for a lost stream, which
/// `session list` renders as `cause:remote-exec-lost`.
pub(crate) const EXIT_STOPPED: i32 = 75;

/// The one line for a capture call against a stopped sandbox.
pub(crate) const STOPPED_LINE: &str =
    "vercel sandbox stopped (open a pane or run `thegn env provision` to resume)";

/// The one line printed when `/interactive` is unavailable.
pub(crate) const STREAM_NOTICE: &str =
    "interactive PTY unavailable on this Vercel sandbox; running in stream mode";

/// Host variables (besides `TERM`) that may enter the sandbox exec env.
pub(crate) const ENV_ALLOWLIST: &[&str] = &["COLORTERM", "LANG", "LC_ALL", "LC_CTYPE"];

/// `TERM` when the host has none.
const DEFAULT_TERM: &str = "xterm-256color";

/// Exit code when the local terminal went away and no remote exit arrived.
const EXIT_LOCAL_CLOSED: i32 = 1;

/// How long to wait for the remote exit after a local close.
const LOCAL_CLOSE_GRACE: Duration = Duration::from_secs(5);

/// Run `cmd` inside Vercel sandbox `id`. Empty `cmd` is a login shell.
pub fn run(cfg: &Config, id: &str, cmd: &[String]) -> Result<()> {
    anyhow::ensure!(
        thegn_core::vercel::admit::valid_sandbox_name(id),
        "vercel-exec: invalid sandbox name"
    );
    let argv: Vec<String> = if cmd.is_empty() {
        vec![
            "/bin/sh".into(),
            "-lc".into(),
            "exec ${SHELL:-/bin/sh} -l".into(),
        ]
    } else {
        cmd.to_vec()
    };
    let provider = cfg
        .env
        .values()
        .filter(|envc| {
            matches!(
                EnvProviderKind::of(&envc.provider.provider),
                EnvProviderKind::Vercel
            )
        })
        .find_map(|envc| crate::provider_factory::vercel_provider_for(&envc.provider, id))
        .ok_or_else(|| {
            anyhow!("vercel-exec: no `provider = \"vercel\"` env with a resolvable token")
        })?;
    let env = exec_env(|key| std::env::var(key).ok());
    if std::io::stdin().is_terminal() {
        let code = run_tty(Arc::new(provider), id, argv, env)?;
        std::process::exit(code);
    }
    let code = run_capture(&provider, id, &argv, &env)?;
    std::process::exit(code);
}

// ---------------------------------------------------------------------------
// Pure decisions (unit-tested).
// ---------------------------------------------------------------------------

/// The sandbox exec env: `TERM` (defaulted) plus the allowlisted keys that are
/// set, non-empty and free of NUL/newlines. Nothing else.
pub(crate) fn exec_env(get: impl Fn(&str) -> Option<String>) -> Vec<(String, String)> {
    let clean = |v: String| (!v.is_empty() && !v.contains(['\0', '\n', '\r'])).then_some(v);
    let term = get("TERM")
        .and_then(clean)
        .unwrap_or_else(|| DEFAULT_TERM.to_string());
    let mut env = vec![("TERM".to_string(), term)];
    for key in ENV_ALLOWLIST {
        if let Some(value) = get(key).and_then(clean) {
            env.push(((*key).to_string(), value));
        }
    }
    env
}

/// Whether a session in `status` cannot run a capture command without a
/// resume (everything but a live or starting session).
pub(crate) fn needs_resume(status: SessionStatus) -> bool {
    !matches!(status, SessionStatus::Running | SessionStatus::Pending)
}

/// The keep-alive due calculation: the core policy's next extension.
pub(crate) fn keep_alive_due(
    now_ms: i64,
    expires_at_ms: i64,
    created_at_ms: i64,
    lifetime_cap_secs: u64,
) -> Option<u64> {
    thegn_core::vercel::policy::next_extension(
        now_ms,
        expires_at_ms,
        created_at_ms,
        lifetime_cap_secs,
    )
}

/// The keep-alive cadence.
pub(crate) fn keep_alive_interval() -> Duration {
    Duration::from_secs(thegn_core::vercel::policy::EXTEND_CHECK_SECS)
}

/// The one line written when the stream is lost for good.
pub(crate) fn lost_line(id: &str) -> String {
    format!(
        "thegn: vercel sandbox {id} exec stream lost; Vercel cannot reattach, so the remote process did not survive"
    )
}

/// Stream mode has no remote PTY: what one chunk of local raw-mode input
/// becomes. `send` goes to the command's stdin (CR becomes LF), `echo` is
/// written locally (the remote does not echo), and `interrupt` is a Ctrl-C.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct StreamInput {
    pub(crate) send: Vec<u8>,
    pub(crate) echo: Vec<u8>,
    pub(crate) interrupt: bool,
}

pub(crate) fn stream_input(bytes: &[u8]) -> StreamInput {
    let mut out = StreamInput::default();
    // An escape sequence (arrows, function keys) is one key: send, no echo.
    if bytes.first() == Some(&0x1b) {
        out.send = bytes.to_vec();
        return out;
    }
    for &b in bytes {
        match b {
            0x03 => {
                out.interrupt = true;
                return out;
            }
            b'\r' | b'\n' => {
                out.send.push(b'\n');
                out.echo.extend_from_slice(b"\r\n");
            }
            0x7f => {
                out.send.push(b);
                out.echo.extend_from_slice(b"\x08 \x08");
            }
            0x20..=0x7e | 0x80..=0xff | b'\t' => {
                out.send.push(b);
                out.echo.push(b);
            }
            _ => out.send.push(b),
        }
    }
    out
}

/// Stream-mode output on a raw local tty: a bare LF becomes CRLF. `prev_cr`
/// carries whether the previous chunk ended in CR.
pub(crate) fn stream_output(bytes: &[u8], prev_cr: &mut bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + bytes.len() / 8);
    for &b in bytes {
        if b == b'\n' && !*prev_cr {
            out.push(b'\r');
        }
        out.push(b);
        *prev_cr = b == b'\r';
    }
    out
}

// ---------------------------------------------------------------------------
// Capture mode.
// ---------------------------------------------------------------------------

/// One command, never resuming. Network seam (`cov_ignore`).
fn run_capture(
    provider: &VercelProvider,
    id: &str,
    argv: &[String],
    env: &[(String, String)],
) -> Result<i32> {
    let rt = crate::agent::provider_runtime()?;
    let outcome = rt.block_on(provider.run_exec(id, argv, None, env, Wake::Never));
    let (code, out) = match outcome {
        Ok(done) => done,
        Err(e) => {
            // Classify without resuming: a stopped sandbox is an expected
            // outcome for a chrome read, not an error.
            let stopped = matches!(
                rt.block_on(provider.reaper_view(id)),
                Ok(Some(view)) if needs_resume(view.status)
            );
            if stopped {
                let mut err = std::io::stderr();
                if writeln!(err, "{STOPPED_LINE}").is_err() {
                    tracing::debug!(target: "thegn::provider", "vercel-exec: stderr gone");
                }
                return Ok(EXIT_STOPPED);
            }
            return Err(e);
        }
    };
    let mut stdout = std::io::stdout();
    // EPIPE on a closed `| head` pipe is normal.
    if stdout
        .write_all(out.as_bytes())
        .and_then(|()| stdout.flush())
        .is_err()
    {
        tracing::debug!(target: "thegn::provider", "vercel-exec: stdout closed");
    }
    Ok(code)
}

// ---------------------------------------------------------------------------
// TTY mode (cov_ignore: a live tty + a network stream; the decisions above are
// pure and tested).
// ---------------------------------------------------------------------------

/// Which transport the session runs on.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Pty,
    Stream,
}

/// How one exec stream ended.
enum StreamEnd {
    Exit(i32),
    Dropped,
    LocalGone,
}

/// The keep-alive thread plus its stop flag. Dropping it (every exit path)
/// sets the flag, wakes the thread and joins it.
struct KeepAlive {
    stop: Arc<(Mutex<bool>, Condvar)>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl KeepAlive {
    /// Start the keep-alive for `id`. Its requests run on `handle`'s runtime
    /// (a multi-thread one, so they progress while the main thread joins).
    fn start(provider: Arc<VercelProvider>, id: String, handle: tokio::runtime::Handle) -> Self {
        let stop = Arc::new((Mutex::new(false), Condvar::new()));
        let flag = Arc::clone(&stop);
        let thread = thegn_core::thread_name::spawn_named("thegn-vx-keep", move || {
            crate::platform::qos::set_self(crate::platform::qos::Qos::Utility); // Utility: a 5-minute session-timeout check.
            while !wait_for_stop(&flag, keep_alive_interval()) {
                match handle.block_on(provider.extend_if_due(&id)) {
                    Ok(Some(ms)) => {
                        tracing::debug!(target: "thegn::provider", extended_ms = ms, "vercel session extended");
                    }
                    Ok(None) => {}
                    Err(e) => {
                        tracing::debug!(target: "thegn::provider", error = %e, "vercel keep-alive check failed");
                    }
                }
            }
        });
        KeepAlive {
            stop,
            thread: Some(thread),
        }
    }

    fn stop(&mut self) {
        let (lock, cvar) = &*self.stop;
        *lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
        cvar.notify_all();
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            tracing::debug!(target: "thegn::provider", "vercel keep-alive thread panicked");
        }
    }
}

impl Drop for KeepAlive {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Wait up to `timeout` for the stop flag; `true` once stop is requested.
fn wait_for_stop(stop: &(Mutex<bool>, Condvar), timeout: Duration) -> bool {
    let (lock, cvar) = stop;
    let guard = lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (guard, _timed_out) = cvar
        .wait_timeout_while(guard, timeout, |stopped| !*stopped)
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *guard
}

/// The tty thread plus its stop handshake; dropping it restores the terminal.
struct TtyGuard {
    done: Arc<AtomicBool>,
    waker: termwiz::terminal::TerminalWaker,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl TtyGuard {
    fn stop(&mut self) {
        self.done.store(true, Ordering::SeqCst);
        if self.waker.wake().is_err() {
            tracing::debug!(target: "thegn::provider", "vercel-exec: tty waker failed");
        }
        if let Some(t) = self.thread.take()
            && t.join().is_err()
        {
            tracing::debug!(target: "thegn::provider", "vercel-exec: tty thread panicked");
        }
    }
}

impl Drop for TtyGuard {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The bridge runtime: one worker, so driver tasks and the keep-alive's
/// `Handle::block_on` calls progress whatever the main thread is doing.
fn bridge_runtime() -> Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .thread_name("thegn-vx-rt")
        .enable_all()
        .build()
        .map_err(|e| anyhow!("tokio runtime: {e}"))
}

fn run_tty(
    provider: Arc<VercelProvider>,
    id: &str,
    argv: Vec<String>,
    env: Vec<(String, String)>,
) -> Result<i32> {
    let hints = ProbeHints::new_from_env()
        .mouse_reporting(Some(false))
        .bracketed_paste(Some(false));
    let caps = Capabilities::new_with_hints(hints).context("term capabilities")?;
    let mut term = new_terminal(caps).context("open terminal")?;
    let size = term.get_screen_size().context("screen size")?;
    let (cols, rows) = pty_size(size.cols, size.rows);
    let spec = ExecSpec {
        argv,
        tty: true,
        cols,
        rows,
        env,
        cwd: None,
    };

    let rt = bridge_runtime()?;
    // Open BEFORE raw mode so an open failure never leaves the tty altered.
    let (session, mode) = match rt.block_on(provider.open_exec(id, &spec)) {
        Ok(session) => (session, Mode::Pty),
        Err(e) => {
            tracing::debug!(target: "thegn::provider", error = %e, "vercel /interactive unavailable");
            let session = rt.block_on(Arc::clone(&provider).open_stream(id, &spec))?;
            let mut err = std::io::stderr();
            if writeln!(err, "{STREAM_NOTICE}").is_err() {
                tracing::debug!(target: "thegn::provider", "vercel-exec: stderr gone");
            }
            (session, Mode::Stream)
        }
    };

    // Never in capture mode: only a live tty session extends the timeout.
    let mut keep = KeepAlive::start(Arc::clone(&provider), id.to_string(), rt.handle().clone());

    // On error `keep` drops here, which stops and joins it.
    term.set_raw_mode().context("raw mode")?;
    let (tty_tx, tty_rx) = tokio::sync::mpsc::channel::<FromTty>(256);
    let done = Arc::new(AtomicBool::new(false));
    let waker = term.waker();
    let thread = {
        let done = done.clone();
        thegn_core::thread_name::spawn_named("thegn-vx-in", move || {
            crate::platform::qos::set_self(crate::platform::qos::Qos::Interactive); // Interactive: remote session keystrokes.
            tty_loop(term, tty_tx, done)
        })
    };
    let mut tty = TtyGuard {
        done,
        waker,
        thread: Some(thread),
    };

    let outcome = rt.block_on(drive(session, tty_rx, mode));
    // Restore the tty first, then stop the keep-alive, then report.
    tty.stop();
    keep.stop();
    Ok(match outcome {
        Some(code) => code,
        None => {
            let mut out = std::io::stdout();
            if writeln!(out, "{}", lost_line(id))
                .and_then(|()| out.flush())
                .is_err()
            {
                tracing::debug!(target: "thegn::provider", "vercel-exec: stdout gone");
            }
            EXIT_STOPPED
        }
    })
}

/// Pump the session to its end. `Some(code)` to exit with; `None` when the
/// stream was lost without an exit (no reattach on Vercel).
async fn drive(
    mut session: ExecSession,
    mut tty_rx: tokio::sync::mpsc::Receiver<FromTty>,
    mode: Mode,
) -> Option<i32> {
    match pump(&mut session, &mut tty_rx, mode).await {
        StreamEnd::Exit(code) => Some(code),
        StreamEnd::Dropped => None,
        StreamEnd::LocalGone => {
            if session.control.send(ExecControl::Close).await.is_err() {
                tracing::debug!(target: "thegn::provider", "vercel-exec: driver already gone");
            }
            Some(await_exit(&mut session).await.unwrap_or(EXIT_LOCAL_CLOSED))
        }
    }
}

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
    .unwrap_or(None)
}

async fn pump(
    session: &mut ExecSession,
    tty_rx: &mut tokio::sync::mpsc::Receiver<FromTty>,
    mode: Mode,
) -> StreamEnd {
    let mut out = std::io::stdout();
    let mut modes = RemoteModes::default();
    let mut prev_cr = false;
    loop {
        tokio::select! {
            frame = session.frames.recv() => match frame {
                Some(ExecFrame::Stdout(bytes)) => {
                    let shown = match mode {
                        Mode::Pty => {
                            modes.observe(&bytes);
                            bytes
                        }
                        Mode::Stream => stream_output(&bytes, &mut prev_cr),
                    };
                    if out.write_all(&shown).and_then(|()| out.flush()).is_err() {
                        return StreamEnd::LocalGone;
                    }
                }
                Some(ExecFrame::Exit(code)) => return StreamEnd::Exit(code),
                None => return StreamEnd::Dropped,
            },
            ev = tty_rx.recv() => match ev {
                Some(FromTty::Resize { cols, rows }) => {
                    if mode == Mode::Pty
                        && session.control.send(ExecControl::Resize { cols, rows }).await.is_err()
                    {
                        tracing::debug!(target: "thegn::provider", "vercel-exec: resize after driver end");
                    }
                }
                Some(ev) => {
                    let Some(bytes) = encode_input(&ev, &modes) else { continue };
                    let send = match mode {
                        Mode::Pty => bytes,
                        Mode::Stream => {
                            let input = stream_input(&bytes);
                            if input.interrupt {
                                // Ctrl-C ends the command (SIGTERM, then SIGKILL).
                                return StreamEnd::LocalGone;
                            }
                            if !input.echo.is_empty()
                                && out.write_all(&input.echo).and_then(|()| out.flush()).is_err()
                            {
                                return StreamEnd::LocalGone;
                            }
                            input.send
                        }
                    };
                    if !send.is_empty()
                        && session.control.send(ExecControl::Stdin(send)).await.is_err()
                    {
                        tracing::debug!(target: "thegn::provider", "vercel-exec: stdin after driver end");
                    }
                }
                None => return StreamEnd::LocalGone,
            },
        }
    }
}

/// Blocking tty reader (copied from `sprite_bridge::tty_loop`): forwards
/// decoded input and resizes, and restores cooked mode as it ends.
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
            Ok(Some(_)) | Ok(None) => continue,
            Err(_) => break,
        };
        if tx.blocking_send(ev).is_err() {
            break;
        }
    }
    if term.set_cooked_mode().is_err() {
        tracing::debug!(target: "thegn::provider", "vercel-exec: cooked mode restore failed");
    }
    if term.flush().is_err() {
        tracing::debug!(target: "thegn::provider", "vercel-exec: terminal flush failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_env_is_term_plus_the_allowlist_only() {
        let host = |key: &str| -> Option<String> {
            match key {
                "TERM" => Some("xterm-ghostty".into()),
                "LANG" => Some("en_US.UTF-8".into()),
                "LC_ALL" => Some(String::new()),
                "LC_CTYPE" => Some("bad\nvalue".into()),
                "VERCEL_TOKEN" | "VERCEL_OIDC_TOKEN" => Some("secret".into()),
                "HOME" | "PATH" | "SSH_AUTH_SOCK" => Some("/host".into()),
                _ => None,
            }
        };
        let env = exec_env(host);
        assert_eq!(
            env,
            vec![
                ("TERM".to_string(), "xterm-ghostty".to_string()),
                ("LANG".to_string(), "en_US.UTF-8".to_string()),
            ]
        );
        assert!(
            env.iter()
                .all(|(k, v)| !k.starts_with("VERCEL") && v != "secret")
        );
        for key in ENV_ALLOWLIST {
            assert!(!key.starts_with("VERCEL"), "{key}");
        }
    }

    #[test]
    fn exec_env_defaults_term() {
        assert_eq!(
            exec_env(|_| None),
            vec![("TERM".to_string(), DEFAULT_TERM.to_string())]
        );
    }

    #[test]
    fn exit_code_and_notice_text_are_fixed() {
        assert_eq!(EXIT_STOPPED, 75);
        assert_eq!(EXIT_STOPPED, crate::sprite_bridge::EXIT_REMOTE_LOST);
        assert_eq!(
            STOPPED_LINE,
            "vercel sandbox stopped (open a pane or run `thegn env provision` to resume)"
        );
        assert_eq!(
            STREAM_NOTICE,
            "interactive PTY unavailable on this Vercel sandbox; running in stream mode"
        );
        let lost = lost_line("tg-a");
        assert!(lost.contains("tg-a") && !lost.contains('\n'), "{lost}");
    }

    #[test]
    fn only_a_live_session_runs_a_capture_without_resume() {
        assert!(!needs_resume(SessionStatus::Running));
        assert!(!needs_resume(SessionStatus::Pending));
        for status in [
            SessionStatus::Stopped,
            SessionStatus::Stopping,
            SessionStatus::Snapshotting,
            SessionStatus::Failed,
            SessionStatus::Aborted,
            SessionStatus::Unknown,
        ] {
            assert!(needs_resume(status), "{status:?}");
        }
    }

    #[test]
    fn keep_alive_due_follows_the_core_policy() {
        use thegn_core::vercel::policy::{EXTEND_STEP_MS, EXTEND_WHEN_REMAINING_MS};
        let now: i64 = 10_000_000;
        let created = now - 60_000;
        let day = 24 * 3600;
        // Plenty of time left: no extension.
        let far = now + i64::try_from(EXTEND_WHEN_REMAINING_MS).expect("ms") * 2;
        assert_eq!(keep_alive_due(now, far, created, day), None);
        // Under the threshold, far from the lifetime cap: one full step.
        let near = now + 60_000;
        assert_eq!(
            keep_alive_due(now, near, created, day),
            Some(EXTEND_STEP_MS)
        );
        // At the lifetime cap: never extend past it.
        let old = now - i64::try_from(day * 1000).expect("ms");
        assert_eq!(keep_alive_due(now, near, old, day), None);
        assert_eq!(keep_alive_interval(), Duration::from_secs(300));
    }

    #[test]
    fn keep_alive_stop_flag_wakes_the_wait_immediately() {
        let stop = (Mutex::new(false), Condvar::new());
        assert!(
            !wait_for_stop(&stop, Duration::from_millis(1)),
            "times out unset"
        );
        *stop.0.lock().expect("lock") = true;
        let started = std::time::Instant::now();
        assert!(wait_for_stop(&stop, Duration::from_secs(60)));
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "a set flag returns at once instead of waiting out the interval"
        );
    }

    #[test]
    fn stream_input_maps_enter_echoes_and_catches_ctrl_c() {
        assert_eq!(
            stream_input(b"ls\r"),
            StreamInput {
                send: b"ls\n".to_vec(),
                echo: b"ls\r\n".to_vec(),
                interrupt: false,
            }
        );
        assert!(stream_input(b"a\x03b").interrupt);
        let esc = stream_input(b"\x1b[A");
        assert_eq!(esc.send, b"\x1b[A".to_vec());
        assert!(esc.echo.is_empty(), "an escape sequence is not echoed");
        let bs = stream_input(b"\x7f");
        assert_eq!(
            (bs.send, bs.echo),
            (b"\x7f".to_vec(), b"\x08 \x08".to_vec())
        );
        let ctl = stream_input(b"\x04");
        assert_eq!((ctl.send, ctl.echo), (b"\x04".to_vec(), Vec::new()));
    }

    #[test]
    fn stream_output_turns_bare_lf_into_crlf_across_chunks() {
        let mut cr = false;
        assert_eq!(stream_output(b"a\nb\r\n", &mut cr), b"a\r\nb\r\n".to_vec());
        assert_eq!(stream_output(b"x\r", &mut cr), b"x\r".to_vec());
        assert!(cr);
        assert_eq!(stream_output(b"\ny", &mut cr), b"\ny".to_vec());
        assert!(!cr);
    }
}
