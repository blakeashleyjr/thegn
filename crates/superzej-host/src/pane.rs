//! A single pane: an emulator grid + history fed by a byte stream, plus a way to
//! send it input. Two transports back a pane, behind [`PaneIo`]:
//!   - **PTY** — a child process on a pseudo-terminal (the default). A blocking
//!     reader thread funnels bytes into the shared channel (portable-pty masters
//!     are blocking file handles — one reader per pane, never a `select!` over N
//!     masters in the event loop).
//!   - **Stream** — a managed-sandbox provider's native exec session (PTY over a
//!     WebSocket; see `superzej_svc::provider`), so an interactive pane attaches
//!     over the provider API with no vendor CLI. A tokio task relays the session's
//!     frames into the same channel and forwards stdin/resize back.
//!
//! Both feed the identical `PaneEvent` channel + waker, so the event loop, the
//! emulator, and the render plan are transport-blind.

use anyhow::{Context, Result};
use portable_pty::{CommandBuilder, MasterPty, PtySize};
use std::io::Write;
use std::sync::{Arc, Mutex};

use termwiz::terminal::TerminalWaker;
use tokio::sync::mpsc as tokio_mpsc;

use superzej_core::history::{AnsiStripper, HistoryBuffer, feed_bytes_to_history};
use superzej_svc::provider::{ExecControl, ExecFrame, ExecSession, Provider};

use crate::emulator::{AlacrittyEmulator, PaneEmulator};

/// How a pane talks to its process: a local PTY, or a provider exec session.
enum PaneIo {
    /// A local child on a pseudo-terminal.
    Pty {
        master: Box<dyn MasterPty + Send>,
        writer: Box<dyn Write + Send>,
    },
    /// A native provider exec session: stdin/resize go to the relay task over
    /// `control`; the task owns the underlying socket. `provider`/`sandbox_id`
    /// are retained so the pane's live session can be persisted for reattach.
    Stream {
        control: tokio_mpsc::Sender<ExecControl>,
        provider: String,
        sandbox_id: String,
    },
}

/// How a `Stream` pane opens its provider session.
pub enum ExecOpen {
    /// Start a fresh exec (the login shell etc.).
    Open(superzej_svc::provider::ExecSpec),
    /// Reattach to a persisted session id (the server replays scrollback).
    Attach {
        session: String,
        cols: u16,
        rows: u16,
    },
}

/// What a pane's reader thread emits (tagged with the pane id so one shared
/// channel multiplexes every pane's output to the event loop).
#[derive(Debug)]
pub enum PaneEvent {
    /// PTY output bytes for pane `id`.
    Output(u32, Vec<u8>),
    /// Pane `id`'s child exited (or the PTY closed); its reader thread is done.
    /// Carries the child's exit code when it could be reaped (`None` if the
    /// status was unavailable — e.g. a PTY read error), so the event loop can
    /// distinguish a clean exit from a crash (item 524).
    Exit(u32, Option<i32>),
}

pub struct PtyPane {
    io: PaneIo,
    emulator: Box<dyn PaneEmulator>,
    rows: u16,
    cols: u16,
    /// The launched program's short name (e.g. `lazygit`, `yazi`, `nvim`, or the
    /// shell). Used to key per-program keybind overlays + remaps. Best-effort
    /// from the spawn argv — not a live foreground-process probe.
    program: String,
    /// Plain-text history ring (ANSI-stripped), parallel to the vt100 grid.
    /// Read by the search engine; populated by `feed` on each PTY output chunk.
    pub history: HistoryBuffer,
    /// Bytes of the in-progress line not yet terminated by '\n'. Flushed into
    /// `history` on newline or when it exceeds 4096 bytes.
    history_partial: Vec<u8>,
    /// Stateful ANSI stripper carried across PTY read chunks so sequences that
    /// arrive split at a chunk boundary are handled correctly.
    history_stripper: AnsiStripper,
    /// The child process id, captured before the child handle moves into the
    /// reader thread. Used to read the live working directory (`/proc/<pid>/cwd`)
    /// at persist time so a resurrected pane can respawn where it was.
    pid: Option<u32>,
    /// A foreground command to offer relaunching (e.g. `"nvim src/main.rs"`),
    /// shown as an overlay over the pane. Set when a resurrected pane had a
    /// captured command, or when a crashed pane is kept as a husk; cleared once
    /// the user accepts (Enter) or dismisses it. `None` for an ordinary pane.
    pending_relaunch: Option<String>,
    /// For a `Stream` pane: the provider session id, published by the relay task
    /// once the server announces it, read at persist time for reattach. `None`
    /// for a PTY pane (and until the announcement lands).
    session_cell: Option<Arc<Mutex<Option<String>>>>,
    /// Predictive local-echo state — instant keystroke echo on a high-latency
    /// remote pane (the srtt gate auto-enables only on a slow link). See `predict`.
    predictor: crate::predict::Predictor,
    /// Monotonic base for the predictor's round-trip timing (ms since creation).
    predict_clock: std::time::Instant,
}

/// Derive a pane's program name from its spawn argv. Handles the common
/// `sh -c "exec <prog> …"` / `sh -lc "<prog> …"` tool-launch shape by reaching
/// past the shell to the first word of the command string; otherwise uses the
/// file stem of `argv[0]`. Returns `""` for an empty argv.
pub fn program_name(argv: &[String]) -> String {
    let stem = |s: &str| -> String {
        std::path::Path::new(s)
            .file_stem()
            .map(|o| o.to_string_lossy().into_owned())
            .unwrap_or_default()
    };
    let Some(first) = argv.first() else {
        return String::new();
    };
    let base = stem(first);
    // A shell running an inline command: `sh -c "exec yazi"` → "yazi".
    if is_interactive_shell(&base)
        && let Some(cmd) = argv
            .iter()
            .skip(1)
            .position(|a| a == "-c" || a == "-lc" || a == "-ic")
            .and_then(|i| argv.get(i + 2))
    {
        // Strip a leading `exec ` and take the first bare word.
        let cmd = cmd.trim().strip_prefix("exec ").unwrap_or(cmd.trim());
        if let Some(word) = cmd.split_whitespace().next() {
            // Only descend when the word is a literal program path — a word
            // carrying shell metacharacters (e.g. the `${SHELL:-/bin/sh}`
            // login-shell placeholder) is not a path; stemming it yields
            // garbage like `sh}`, so fall back to the outer shell name.
            if !word.contains(['$', '{', '}', '(', ')', '`']) {
                return stem(word);
            }
        }
    }
    base
}

/// Whether `program` (a short program name from [`program_name`]) is an
/// interactive shell. Used by attention routing (item 524) to keep routine
/// shell closes from generating notifications.
pub fn is_interactive_shell(program: &str) -> bool {
    matches!(program, "sh" | "bash" | "zsh" | "dash" | "fish")
}

/// Whether a *clean* exit of a pane running `program` is routine noise rather
/// than a task worth surfacing. Interactive shells qualify (closing a prompt is
/// not news) and so does an **empty** program name: an empty name means we never
/// resolved what the pane was running (an internal/transient/placeholder pane,
/// or a spawn whose argv we couldn't name), so a "process finished" notification
/// would be meaningless. A non-zero exit is still surfaced as a crash by the
/// caller regardless of this — only clean exits are suppressed here.
pub fn is_routine_pane(program: &str) -> bool {
    program.is_empty() || is_interactive_shell(program)
}

/// Whether `program` is a sandbox/remote wrapper rather than the user's actual
/// foreground program — its `/proc` child is the runtime shim, so relaunching it
/// from the host is meaningless. Used to skip foreground-command capture for
/// containerized/remote panes.
fn is_runtime_wrapper(program: &str) -> bool {
    matches!(
        program,
        "podman" | "docker" | "conmon" | "runc" | "crun" | "bwrap" | "systemd-run" | "ssh"
    )
}

/// The most-recently-started direct child of `pid` (the shell's foreground job),
/// if any. Walks `/proc/*/stat` for the `ppid` field; ties break to the highest
/// pid (newest). Linux-only — returns `None` where `/proc` is absent.
fn newest_child(pid: u32) -> Option<u32> {
    let mut best: Option<u32> = None;
    for ent in std::fs::read_dir("/proc").ok()?.flatten() {
        let Some(child) = ent.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        if child != pid && stat_ppid(child) == Some(pid) {
            best = Some(best.map_or(child, |b| b.max(child)));
        }
    }
    best
}

/// The parent pid from `/proc/<pid>/stat`. The `comm` field (field 2) can itself
/// contain spaces and parens, so the numeric fields are parsed after the final
/// `)`: state is the first token, ppid the second.
fn stat_ppid(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(1)?.parse().ok()
}

/// Parse `/proc/<pid>/cmdline` (NUL-separated argv) into a `Vec`, dropping empty
/// trailing entries. `None` when unreadable or empty (e.g. a kernel thread).
fn read_cmdline(pid: u32) -> Option<Vec<String>> {
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let argv: Vec<String> = raw
        .split(|&b| b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect();
    (!argv.is_empty()).then_some(argv)
}

impl PtyPane {
    /// Spawn `argv` (already composed by `sandbox::enter_argv`) in `cwd` on a
    /// fresh PTY of `rows`x`cols`, injecting `env` (key/value pairs) into the
    /// child — agent panes expect `SUPERZEJ_WORKTREE`/`_BRANCH`; a plain pane
    /// passes an empty slice. Reader-thread events arrive on `tx`, tagged with
    /// `id` so a shared channel can carry every pane's output.
    ///
    /// `waker` (when present) is pulsed after every send so the main loop's
    /// blocking `poll_input(None)` returns immediately to drain PTY output — this
    /// is what makes the loop event-driven (zero idle wakeups) rather than polled.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn_with_env(
        id: u32,
        argv: &[String],
        cwd: Option<&std::path::Path>,
        env: &[(String, String)],
        rows: u16,
        cols: u16,
        tx: tokio_mpsc::Sender<PaneEvent>,
        waker: Option<TerminalWaker>,
    ) -> Result<Self> {
        let pty = portable_pty::native_pty_system();
        let pair = pty
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("openpty")?;

        let mut cmd = CommandBuilder::new(&argv[0]);
        cmd.args(&argv[1..]);
        if let Some(dir) = cwd {
            cmd.cwd(dir);
        }
        cmd.env("TERM", "xterm-256color");
        // The emulator parses 24-bit SGR; advertise it so apps (btop, modern
        // CLIs) pick truecolor instead of degraded 256-color ramps.
        cmd.env("COLORTERM", "truecolor");
        for (k, v) in env {
            cmd.env(k, v);
        }
        let mut child = pair.slave.spawn_command(cmd).context("spawn child")?;
        // Capture the pid before `child` moves into the reader thread below —
        // it's the handle we use to read the pane's live cwd for persistence.
        let pid = child.process_id();
        // Drop the slave so the master sees EOF when the child exits.
        drop(pair.slave);

        let writer = pair.master.take_writer().context("take_writer")?;
        let mut reader = pair.master.try_clone_reader().context("clone_reader")?;

        // Use std::thread::spawn for the reader - it doesn't require a Tokio runtime
        // but can still use blocking_send on the tokio channel. The child handle
        // moves in here so that, once the read loop ends on PTY EOF, we can
        // `wait()` for the child's exit status and report its code (item 524).
        // Blocking the *reader* thread on `wait()` is safe — it's about to end
        // anyway and never touches the event loop.
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break, // EOF: child exited or PTY closed
                    Ok(n) => {
                        // One exact-sized Vec per chunk: ownership must cross
                        // the channel, and the 8K stack buffer is reused, so
                        // this is the minimal copy (a buffer pool would add
                        // complexity for no measured win).
                        if tx
                            .blocking_send(PaneEvent::Output(id, buf[..n].to_vec()))
                            .is_err()
                        {
                            return; // consumer gone — don't bother reaping
                        }
                        if let Some(w) = &waker {
                            let _ = w.wake();
                        }
                    }
                    Err(_) => break, // read error: treat as exit, status unknown
                }
            }
            // Reap the child so the exit carries its real code (None if the
            // status can't be retrieved). u32 → i32 keeps the conventional
            // exit-code range; 0 == success.
            let code = child.wait().ok().map(|s| s.exit_code() as i32);
            let _ = tx.blocking_send(PaneEvent::Exit(id, code));
            if let Some(w) = &waker {
                let _ = w.wake();
            }
        });

        Ok(Self {
            io: PaneIo::Pty {
                master: pair.master,
                writer,
            },
            emulator: Box::new(AlacrittyEmulator::new(rows, cols, 10_000)),
            rows,
            cols,
            program: program_name(argv),
            history: HistoryBuffer::new(10_000),
            history_partial: Vec::new(),
            history_stripper: AnsiStripper::default(),
            pid,
            pending_relaunch: None,
            session_cell: None,
            predictor: crate::predict::Predictor::new(),
            predict_clock: std::time::Instant::now(),
        })
    }

    /// Spawn a `Stream` pane backed by a managed-sandbox provider's native exec
    /// API — the CLI-free interactive pane. Non-blocking: a relay task runs on
    /// `rt` that opens the session (`open` ⇒ `open_exec`, or a reattach), pumps
    /// its output into `tx` as [`PaneEvent`]s (pulsing `waker`), forwards
    /// stdin/resize from the pane's control channel, and publishes the provider
    /// session id for persistence. A connect/exec failure surfaces as an `Exit`.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn_stream(
        id: u32,
        provider: Provider,
        provider_name: String,
        sandbox_id: String,
        open: ExecOpen,
        program: String,
        rows: u16,
        cols: u16,
        tx: tokio_mpsc::Sender<PaneEvent>,
        waker: Option<TerminalWaker>,
        rt: &tokio::runtime::Handle,
    ) -> Self {
        let (ctrl_tx, ctrl_rx) = tokio_mpsc::channel::<ExecControl>(256);
        let session_cell: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        rt.spawn(relay_exec(
            id,
            provider,
            provider_name.clone(),
            sandbox_id.clone(),
            open,
            tx,
            waker,
            ctrl_rx,
            session_cell.clone(),
        ));
        Self {
            io: PaneIo::Stream {
                control: ctrl_tx,
                provider: provider_name,
                sandbox_id,
            },
            emulator: Box::new(AlacrittyEmulator::new(rows, cols, 10_000)),
            rows,
            cols,
            program,
            history: HistoryBuffer::new(10_000),
            history_partial: Vec::new(),
            history_stripper: AnsiStripper::default(),
            pid: None,
            pending_relaunch: None,
            session_cell: Some(session_cell),
            predictor: crate::predict::Predictor::new(),
            predict_clock: std::time::Instant::now(),
        }
    }

    /// The pane's current working directory, read live from `/proc/<pid>/cwd`.
    /// `None` when the pid is unknown, the process is gone, or the symlink can't
    /// be resolved (e.g. a sandbox runtime whose cwd isn't host-meaningful — the
    /// caller gates capture on the host backend regardless). Linux-only; other
    /// platforms (where superzej does not run) return `None`.
    pub fn cwd(&self) -> Option<std::path::PathBuf> {
        let pid = self.pid?;
        std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
    }

    /// The pane's live foreground command (argv + cwd), read from `/proc`: the
    /// shell's foreground child job, when it is a real non-shell program. `None`
    /// for an idle shell prompt, a nested shell, a sandbox/remote runtime child,
    /// an unknown pid, or non-Linux. Captured at persist time so a resurrected
    /// or crashed pane can offer to relaunch what was running.
    pub fn foreground_command(&self) -> Option<crate::session::PaneCmd> {
        let shell = self.pid?;
        let child = newest_child(shell)?;
        let argv = read_cmdline(child)?;
        let name = std::path::Path::new(argv.first()?)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        // A bare shell prompt or a nested shell isn't worth relaunching, and a
        // sandbox/remote runtime child (the container shim, not the inner
        // program) can't be relaunched from the host.
        if name.is_empty() || is_interactive_shell(&name) || is_runtime_wrapper(&name) {
            return None;
        }
        let cwd = std::fs::read_link(format!("/proc/{child}/cwd"))
            .ok()
            .map(|p| p.to_string_lossy().into_owned());
        Some(crate::session::PaneCmd { argv, cwd })
    }

    /// The launched program's short name (keys per-program keybind overlays).
    pub fn program(&self) -> &str {
        &self.program
    }

    /// The command this pane is offering to relaunch, if any (drives the
    /// overlay; `Some` means the pane is awaiting an Enter/dismiss).
    pub fn pending_relaunch(&self) -> Option<&str> {
        self.pending_relaunch.as_deref()
    }

    /// Arm (or clear) the relaunch overlay for this pane.
    pub fn set_pending_relaunch(&mut self, cmd: Option<String>) {
        self.pending_relaunch = cmd;
    }

    /// Take the pending relaunch command, clearing the overlay (on Enter).
    pub fn take_pending_relaunch(&mut self) -> Option<String> {
        self.pending_relaunch.take()
    }

    /// Feed PTY output into the emulator grid and the plain-text history ring.
    /// Drain-without-render is just this without a subsequent compose.
    pub fn feed(&mut self, bytes: &[u8]) {
        // Server output is authoritative (and carries the echoed keystrokes), so
        // it retires the prediction overlay + folds a round-trip sample into srtt.
        let now = self.predict_now_ms();
        self.predictor.on_server_output(now);
        self.emulator.advance(bytes);
        feed_bytes_to_history(
            bytes,
            &mut self.history,
            &mut self.history_partial,
            &mut self.history_stripper,
        );
    }

    /// ms since this pane was created — the predictor's round-trip clock.
    fn predict_now_ms(&self) -> u64 {
        self.predict_clock.elapsed().as_millis() as u64
    }

    /// The screen state the predictor's safety gate reads (never predict in a
    /// full-screen/raw TUI, or off the prompt row).
    fn predict_screen_state(&self) -> crate::predict::ScreenState {
        let (rows, _) = self.emulator.size();
        let (cur_row, _) = self.emulator.cursor();
        crate::predict::ScreenState {
            alt_screen: self.emulator.alt_screen(),
            // NB: do NOT gate on bracketed-paste — bash/zsh enable DECSET 2004 at
            // the *prompt* by default, which is exactly where we want to predict.
            // application-cursor (DECCKM) is the real raw/TUI signal.
            app_mode: self.emulator.application_cursor(),
            cursor_row: cur_row as usize,
            rows: rows as usize,
        }
    }

    /// A printable keystroke is being sent: time it (for srtt) and, when the gate
    /// is open (slow link + prompt row + not a TUI), add it to the prediction
    /// overlay. Returns whether the overlay changed (caller marks the pane dirty).
    pub fn predict_key(&mut self, c: char) -> bool {
        let now = self.predict_now_ms();
        let s = self.predict_screen_state();
        if self.predictor.should_predict(&s) {
            self.predictor.on_key(c, now);
            true
        } else {
            self.predictor.note_key(now);
            false
        }
    }

    /// Backspace pops the last prediction; returns whether anything changed.
    pub fn predict_backspace(&mut self) -> bool {
        let had = !self.predictor.is_empty();
        self.predictor.on_backspace();
        had
    }

    /// A line was submitted / a control key pressed — flush the overlay (the
    /// server redraws). Returns whether anything was showing.
    pub fn predict_flush(&mut self) -> bool {
        let had = !self.predictor.is_empty();
        self.predictor.on_enter();
        had
    }

    /// The predicted chars to overlay at the cursor (empty when not predicting).
    pub fn predicted(&self) -> &[char] {
        self.predictor.pending()
    }

    /// Forward user input bytes to the child. Typing snaps the viewport back to
    /// the live tail (the usual terminal behavior when scrolled into history).
    pub fn write_input(&mut self, bytes: &[u8]) -> Result<()> {
        if self.emulator.scrollback() > 0 {
            self.emulator.scroll_reset();
        }
        match &mut self.io {
            PaneIo::Pty { writer, .. } => {
                writer.write_all(bytes).context("pty write")?;
                writer.flush().ok();
            }
            // Drop on a full/closed control channel rather than blocking the
            // loop — a dead session will surface its exit on the frames side.
            PaneIo::Stream { control, .. } => {
                let _ = control.try_send(ExecControl::Stdin(bytes.to_vec()));
            }
        }
        Ok(())
    }

    /// Resize the transport and the emulator together.
    pub fn resize(&mut self, rows: u16, cols: u16) -> Result<()> {
        match &self.io {
            PaneIo::Pty { master, .. } => {
                master
                    .resize(PtySize {
                        rows,
                        cols,
                        pixel_width: 0,
                        pixel_height: 0,
                    })
                    .context("pty resize")?;
            }
            PaneIo::Stream { control, .. } => {
                let _ = control.try_send(ExecControl::Resize { cols, rows });
            }
        }
        self.emulator.resize(rows, cols);
        self.rows = rows;
        self.cols = cols;
        Ok(())
    }

    /// The provider session id for a `Stream` pane, if the server has announced
    /// it yet (`None` for a PTY pane, or before the announcement).
    pub fn session_id(&self) -> Option<String> {
        self.session_cell.as_ref()?.lock().ok()?.clone()
    }

    /// This pane's persistable provider session (`provider` + sandbox `id` +
    /// announced `session`), for reattach on restart. `None` for a PTY pane or a
    /// `Stream` pane whose session id hasn't been announced yet.
    pub fn provider_session(&self) -> Option<crate::session::ProviderSession> {
        let PaneIo::Stream {
            provider,
            sandbox_id,
            ..
        } = &self.io
        else {
            return None;
        };
        let session = self.session_id()?;
        Some(crate::session::ProviderSession {
            provider: provider.clone(),
            id: sandbox_id.clone(),
            session,
        })
    }

    /// Build a `Stream` pane around a ready control channel, for tests that drive
    /// the relay directly (no provider/socket).
    #[cfg(test)]
    fn test_stream(control: tokio_mpsc::Sender<ExecControl>, rows: u16, cols: u16) -> Self {
        Self {
            io: PaneIo::Stream {
                control,
                provider: "sprites".into(),
                sandbox_id: "test".into(),
            },
            emulator: Box::new(AlacrittyEmulator::new(rows, cols, 10_000)),
            rows,
            cols,
            program: "shell".into(),
            history: HistoryBuffer::new(10_000),
            history_partial: Vec::new(),
            history_stripper: AnsiStripper::default(),
            pid: None,
            pending_relaunch: None,
            session_cell: Some(Arc::new(Mutex::new(None))),
            predictor: crate::predict::Predictor::new(),
            predict_clock: std::time::Instant::now(),
        }
    }

    #[allow(dead_code)]
    pub fn size(&self) -> (u16, u16) {
        (self.rows, self.cols)
    }

    pub fn emulator(&self) -> &dyn PaneEmulator {
        self.emulator.as_ref()
    }

    /// Scroll the pane's viewport into/out of scrollback history.
    pub fn scroll_up(&mut self, n: usize) {
        self.emulator.scroll_up(n);
    }
    pub fn scroll_down(&mut self, n: usize) {
        self.emulator.scroll_down(n);
    }
}

/// The `Stream` pane's relay task (runs on the host runtime): open or reattach the
/// provider exec session, then bridge it to the shared `PaneEvent` channel — the
/// exact contract the PTY reader thread fulfills, so the event loop is blind to
/// which transport a pane uses. Forwards stdin/resize from `ctrl_rx`, publishes
/// the announced session id into `session_cell`, and ends on exit/close/drop.
/// How a single [`relay_session`] ended — drives reconnect vs propagate.
#[derive(Debug, PartialEq, Eq)]
enum SessionEnd {
    /// The server reported a command exit (terminal — propagate it).
    Exited(i32),
    /// The socket dropped without an exit; `progressed` = forwarded any output
    /// this session (resets the reconnect budget).
    Dropped { progressed: bool },
    /// The pane was dropped (its control channel closed) — stop, no exit event.
    PaneGone,
}

/// Max consecutive reconnects that make no progress before giving up.
const MAX_DEAD_RECONNECTS: u32 = 3;

#[allow(clippy::too_many_arguments)]
#[tracing::instrument(
    target = "szhost::frame",
    name = "native_pane",
    skip_all,
    fields(pane = id, provider = %provider_name)
)]
async fn relay_exec(
    id: u32,
    provider: Provider,
    provider_name: String,
    sandbox_id: String,
    open: ExecOpen,
    tx: tokio_mpsc::Sender<PaneEvent>,
    waker: Option<TerminalWaker>,
    mut ctrl_rx: tokio_mpsc::Receiver<ExecControl>,
    session_cell: Arc<Mutex<Option<String>>>,
) {
    let wake = || {
        if let Some(w) = &waker {
            let _ = w.wake();
        }
    };
    let (cols, rows) = match &open {
        ExecOpen::Open(spec) => (spec.cols, spec.rows),
        ExecOpen::Attach { cols, rows, .. } => (*cols, *rows),
    };
    // Keep the open spec so a permanently-dropped session can be RE-OPENED fresh
    // (not just reattached) — opening a new exec resumes a suspended/restarted
    // sandbox and restores input. `None` for an Attach-only pane (restart
    // reattach), which has no spec to reopen from.
    let reopen_spec = match &open {
        ExecOpen::Open(spec) => Some(spec.clone()),
        ExecOpen::Attach { .. } => None,
    };
    tracing::debug!(
        target: "szhost::sandbox",
        provider = %provider_name, sandbox = %sandbox_id, %cols, %rows,
        attach = matches!(open, ExecOpen::Attach { .. }),
        "native exec: opening interactive session"
    );
    let opened = match open {
        ExecOpen::Open(spec) => provider.open_exec(&sandbox_id, &spec).await,
        ExecOpen::Attach {
            session,
            cols,
            rows,
        } => {
            provider
                .attach_exec(&sandbox_id, &session, cols, rows)
                .await
        }
    };
    let mut session = match opened {
        Ok(s) => {
            crate::agent::native_exec_report(&provider_name, true);
            s
        }
        Err(e) => {
            // Mark the provider unhealthy so `exec=auto` panes fall back to the
            // CLI during the cooldown; surface the failure + a non-zero exit.
            crate::agent::native_exec_report(&provider_name, false);
            let _ = tx
                .send(PaneEvent::Output(
                    id,
                    format!("\r\n[native exec failed: {e}]\r\n").into_bytes(),
                ))
                .await;
            wake();
            let _ = tx.send(PaneEvent::Exit(id, Some(1))).await;
            wake();
            return;
        }
    };

    // Reconnect loop: a transient socket drop with a known session id reattaches
    // (replaying scrollback). Bounded so a permanently-dead session still exits.
    let mut dead = 0u32;
    loop {
        match relay_session(id, session, &tx, &waker, &mut ctrl_rx, &session_cell).await {
            SessionEnd::Exited(code) => {
                tracing::debug!(
                    target: "szhost::sandbox",
                    pane = id, sandbox = %sandbox_id, code,
                    "exec session exited (command returned)"
                );
                let _ = tx.send(PaneEvent::Exit(id, Some(code))).await;
                wake();
                return;
            }
            SessionEnd::PaneGone => return,
            SessionEnd::Dropped { progressed } => {
                dead = if progressed { 0 } else { dead + 1 };
                tracing::debug!(
                    target: "szhost::sandbox",
                    pane = id, sandbox = %sandbox_id, progressed, dead,
                    "exec session dropped (socket closed, no exit); reconnecting"
                );
                if dead < MAX_DEAD_RECONNECTS {
                    let sid = session_cell.lock().ok().and_then(|c| c.clone());
                    // 1. Prefer reattaching the SAME session: a transient socket
                    //    drop replays scrollback with the shell state preserved.
                    if let Some(sid) = &sid
                        && let Ok(s) = provider.attach_exec(&sandbox_id, sid, cols, rows).await
                    {
                        tracing::debug!(target: "szhost::sandbox", pane = id, "reattached exec session");
                        session = s;
                        continue;
                    }
                    // 2. Reattach failed — the session is genuinely gone (the
                    //    sandbox was suspended/restarted). Open a FRESH exec: this
                    //    resumes the sandbox and restores a working shell (a new
                    //    shell process; fs/cwd preserved). This is the
                    //    "suspend-idle, recover-on-return" path so a backgrounded
                    //    remote pane never becomes a permanently dead shell.
                    if let Some(spec) = &reopen_spec {
                        if let Ok(mut c) = session_cell.lock() {
                            *c = None; // drop the stale id; the fresh session announces a new one
                        }
                        if let Ok(s) = provider.open_exec(&sandbox_id, spec).await {
                            tracing::debug!(
                                target: "szhost::sandbox",
                                pane = id, sandbox = %sandbox_id,
                                "re-opened a FRESH exec session (resumed the sandbox)"
                            );
                            crate::agent::native_exec_report(&provider_name, true);
                            session = s;
                            continue;
                        }
                    }
                }
                tracing::warn!(
                    target: "szhost::sandbox",
                    pane = id, sandbox = %sandbox_id, dead,
                    "exec session gone after reconnect attempts; pane exits"
                );
                let _ = tx.send(PaneEvent::Exit(id, None)).await;
                wake();
                return;
            }
        }
    }
}

/// Bridge an already-open [`ExecSession`] to the pane's `PaneEvent` channel until
/// it exits/closes or the pane is dropped, returning *why* it ended (so the
/// caller can reconnect on a transient drop). Split out from [`relay_exec`] so
/// it's unit-testable with a hand-built session (no live socket).
async fn relay_session(
    id: u32,
    session: ExecSession,
    tx: &tokio_mpsc::Sender<PaneEvent>,
    waker: &Option<TerminalWaker>,
    ctrl_rx: &mut tokio_mpsc::Receiver<ExecControl>,
    session_cell: &Arc<Mutex<Option<String>>>,
) -> SessionEnd {
    let wake = || {
        if let Some(w) = waker {
            let _ = w.wake();
        }
    };
    let ExecSession {
        mut frames,
        control,
        mut session_id,
    } = session;

    let mut sid_done = false;
    let mut progressed = false;
    loop {
        tokio::select! {
            frame = frames.recv() => match frame {
                Some(ExecFrame::Stdout(b)) => {
                    if tx.send(PaneEvent::Output(id, b)).await.is_err() {
                        return SessionEnd::PaneGone;
                    }
                    progressed = true;
                    wake();
                }
                Some(ExecFrame::Exit(code)) => return SessionEnd::Exited(code),
                None => return SessionEnd::Dropped { progressed },
            },
            ctrl = ctrl_rx.recv() => match ctrl {
                Some(c) => {
                    if control.send(c).await.is_err() {
                        return SessionEnd::Dropped { progressed }; // driver/socket gone
                    }
                }
                None => return SessionEnd::PaneGone, // pane dropped
            },
            res = session_id.changed(), if !sid_done => {
                match res {
                    Ok(()) => {
                        if let Some(sid) = session_id.borrow().clone()
                            && let Ok(mut cell) = session_cell.lock()
                        {
                            *cell = Some(sid);
                            sid_done = true; // announced once; stop watching
                        }
                    }
                    Err(_) => sid_done = true, // sender gone
                }
            }
        }
    }
}

/// Block (with a deadline) draining `rx` into the pane until the child exits or
/// the deadline passes. Test/helper for headless round-trips; the interactive
/// loop drains the same channel via `try_recv`.
#[allow(dead_code)]
pub fn drain_until_exit(
    pane: &mut PtyPane,
    rx: &mut tokio_mpsc::Receiver<PaneEvent>,
    deadline_ms: u64,
) -> bool {
    use std::time::Instant;
    let start = Instant::now();
    loop {
        let remaining = deadline_ms.saturating_sub(start.elapsed().as_millis() as u64);
        if remaining == 0 {
            return false;
        }
        // Use blocking recv in a loop with timeout
        match rx.blocking_recv() {
            Some(PaneEvent::Output(_, b)) => pane.feed(&b),
            Some(PaneEvent::Exit(..)) => return true,
            None => return false,
        }
        if start.elapsed().as_millis() as u64 >= deadline_ms {
            return false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str) -> Vec<String> {
        vec!["/bin/sh".into(), "-c".into(), script.into()]
    }

    #[test]
    fn program_name_uses_argv0_stem() {
        assert_eq!(program_name(&["/usr/bin/lazygit".into()]), "lazygit");
        assert_eq!(program_name(&["nvim".into(), "file".into()]), "nvim");
        assert_eq!(program_name(&[]), "");
    }

    #[test]
    fn is_interactive_shell_matches_known_shells() {
        for s in ["sh", "bash", "zsh", "dash", "fish"] {
            assert!(is_interactive_shell(s), "{s} should be a shell");
        }
        for s in ["cargo", "make", "nvim", "lazygit", ""] {
            assert!(!is_interactive_shell(s), "{s} should not be a shell");
        }
    }

    #[test]
    fn is_routine_pane_covers_shells_and_unnamed_panes() {
        // Shells and unnamed (empty-program) panes are routine: a clean exit is
        // noise, not a "<x> finished" notification.
        for s in ["sh", "bash", "zsh", "dash", "fish", ""] {
            assert!(is_routine_pane(s), "{s:?} should be routine");
        }
        // Real, named programs are tasks worth surfacing on a clean exit.
        for s in ["cargo", "make", "nvim", "lazygit", "yazi"] {
            assert!(!is_routine_pane(s), "{s} should not be routine");
        }
    }

    #[test]
    fn program_name_reaches_past_shell_to_inline_command() {
        // The tool-drawer pattern: `sh -c "exec yazi"` → "yazi".
        assert_eq!(program_name(&sh("exec yazi")), "yazi");
        assert_eq!(program_name(&sh("lazygit --version")), "lazygit");
        // A login shell with no inline command is just the shell.
        assert_eq!(program_name(&["/bin/zsh".into(), "-i".into()]), "zsh");
        // Regression: the `${SHELL:-/bin/sh} -l` login-shell placeholder must
        // not be stemmed into garbage like `sh}` — fall back to the outer
        // shell name instead.
        assert_eq!(
            program_name(&[
                "/bin/zsh".into(),
                "-lc".into(),
                "${SHELL:-/bin/sh} -l".into(),
            ]),
            "zsh"
        );
    }

    #[test]
    fn pty_round_trip_lands_output_in_grid() {
        let (tx, mut rx) = tokio_mpsc::channel(1024);
        let mut pane =
            PtyPane::spawn_with_env(0, &sh("printf 'hello-pty'"), None, &[], 24, 80, tx, None)
                .unwrap();
        assert!(
            drain_until_exit(&mut pane, &mut rx, 5000),
            "child should exit"
        );
        assert_eq!(
            pane.emulator()
                .row_text(0)
                .map(|r| r.trim_end().to_string()),
            Some("hello-pty".to_string())
        );
    }

    #[test]
    fn resize_propagates_to_child_via_winsize() {
        // `stty size` prints "rows cols" read from the PTY winsize.
        let (tx, mut rx) = tokio_mpsc::channel(1024);
        let mut pane =
            PtyPane::spawn_with_env(0, &sh("stty size"), None, &[], 30, 100, tx, None).unwrap();
        assert!(drain_until_exit(&mut pane, &mut rx, 5000));
        assert_eq!(
            pane.emulator()
                .row_text(0)
                .map(|r| r.trim_end().to_string()),
            Some("30 100".to_string())
        );
    }

    #[test]
    fn stream_pane_relays_frames_input_resize_and_session_id() {
        use std::time::Duration;
        use superzej_svc::provider::{ExecControl, ExecFrame, ExecSession};

        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();

        // server→loop, provider→relay (frames), pane→relay (control),
        // relay→provider (control), session-id announce.
        let (tx, mut rx) = tokio_mpsc::channel::<PaneEvent>(64);
        let (frames_tx, frames_rx) = tokio_mpsc::channel::<ExecFrame>(64);
        let (pane_ctrl_tx, pane_ctrl_rx) = tokio_mpsc::channel::<ExecControl>(64);
        let (prov_ctrl_tx, mut prov_ctrl_rx) = tokio_mpsc::channel::<ExecControl>(64);
        let (sid_tx, sid_rx) = tokio::sync::watch::channel::<Option<String>>(None);
        let cell = Arc::new(Mutex::new(None));
        let session = ExecSession {
            frames: frames_rx,
            control: prov_ctrl_tx,
            session_id: sid_rx,
        };
        let cell_task = cell.clone();
        let (done_tx, done_rx) = std::sync::mpsc::channel::<SessionEnd>();
        rt.spawn(async move {
            let mut pane_ctrl_rx = pane_ctrl_rx;
            let end = relay_session(7, session, &tx, &None, &mut pane_ctrl_rx, &cell_task).await;
            let _ = done_tx.send(end);
        });

        // 1. provider stdout → PaneEvent::Output, and it lands in the grid.
        frames_tx
            .blocking_send(ExecFrame::Stdout(b"hello-stream".to_vec()))
            .unwrap();
        let mut pane = PtyPane::test_stream(pane_ctrl_tx, 24, 80);
        match rx.blocking_recv() {
            Some(PaneEvent::Output(7, b)) => {
                assert_eq!(b, b"hello-stream");
                pane.feed(&b);
            }
            other => panic!("expected Output, got {other:?}"),
        }
        assert_eq!(
            pane.emulator()
                .row_text(0)
                .map(|r| r.trim_end().to_string()),
            Some("hello-stream".to_string())
        );

        // 2. pane input + resize are forwarded to the provider side.
        pane.write_input(b"abc").unwrap();
        assert_eq!(
            prov_ctrl_rx.blocking_recv(),
            Some(ExecControl::Stdin(b"abc".to_vec()))
        );
        pane.resize(30, 100).unwrap();
        assert_eq!(
            prov_ctrl_rx.blocking_recv(),
            Some(ExecControl::Resize {
                cols: 100,
                rows: 30
            })
        );

        // 3. the announced session id is published for reattach.
        sid_tx.send(Some("sess-9".into())).unwrap();
        let mut got = None;
        for _ in 0..200 {
            if let Some(s) = cell.lock().unwrap().clone() {
                got = Some(s);
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(got.as_deref(), Some("sess-9"));

        // 4. a provider exit ends the session as Exited(code) (relay_exec is what
        // turns that into PaneEvent::Exit).
        frames_tx.blocking_send(ExecFrame::Exit(0)).unwrap();
        assert_eq!(
            done_rx.recv_timeout(Duration::from_secs(2)),
            Ok(SessionEnd::Exited(0))
        );
    }

    #[test]
    fn backpressure_does_not_deadlock_on_a_flood() {
        // A chatty child must not block the reader; we drain a bounded window
        // and drop the pane (reader thread exits when the channel sender errors).
        let (tx, mut rx) = tokio_mpsc::channel(1024);
        let mut pane = PtyPane::spawn_with_env(
            0,
            &sh("yes superzej | head -c 200000"),
            None,
            &[],
            24,
            80,
            tx,
            None,
        )
        .unwrap();
        let exited = drain_until_exit(&mut pane, &mut rx, 5000);
        assert!(
            exited,
            "flood should drain and the child should exit cleanly"
        );
        // The flood scrolled through the grid; some visible row holds the token.
        let emu = pane.emulator();
        let (rows, _) = emu.size();
        let seen = (0..rows).any(|r| emu.row_text(r).unwrap_or_default().contains("superzej"));
        assert!(
            seen,
            "expected the repeated token somewhere in the visible grid"
        );
    }
}
