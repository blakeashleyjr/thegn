//! A single pane: an emulator grid + history fed by a byte stream, plus a way to
//! send it input. Two transports back a pane, behind [`PaneIo`]:
//!   - **PTY** — a child process on a pseudo-terminal (the default). A blocking
//!     reader thread funnels bytes into the shared channel (portable-pty masters
//!     are blocking file handles — one reader per pane, never a `select!` over N
//!     masters in the event loop).
//!   - **Stream** — a managed-sandbox provider's native exec session (PTY over a
//!     WebSocket; see `thegn_svc::provider`), so an interactive pane attaches
//!     over the provider API with no vendor CLI. A tokio task relays the session's
//!     frames into the same channel and forwards stdin/resize back.
//!
//! Both feed the identical `PaneEvent` channel + waker, so the event loop, the
//! emulator, and the render plan are transport-blind.

use anyhow::{Context, Result};
use portable_pty::{MasterPty, PtySize};
use std::io::Write;
use std::sync::{Arc, Mutex};

use termwiz::terminal::TerminalWaker;
use tokio::sync::mpsc as tokio_mpsc;

use thegn_core::history::{AnsiStripper, HistoryBuffer, feed_bytes_to_history};
use thegn_svc::provider::{ExecControl, ExecFrame, ExecSession};

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
    Open(thegn_svc::provider::ExecSpec),
    /// Reattach to a persisted session id (the server replays scrollback).
    Attach {
        session: String,
        cols: u16,
        rows: u16,
        /// Fresh-open spec to RE-OPEN the exec if the reattach is a dead/stale
        /// session — e.g. one that didn't survive a thegn restart or a sandbox
        /// suspend. Without it a stale reattach can only re-attach to the corpse:
        /// a broken/frozen shell that flaps back to the loading splash. With it,
        /// the reconnect loop opens a fresh working shell (fs/cwd preserved).
        fallback: thegn_svc::provider::ExecSpec,
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
    /// Pane `id`'s warm reattach found its persisted session gone (lease
    /// expired / daemon restarted — e.g. after a reboot) and degraded to a
    /// fresh session. The loop applies the pane's [`FallbackRestore`]:
    /// repaint the persisted scrollback tail + arm the relaunch overlay.
    SessionFallback(u32),
}

/// What a stream pane restores when its warm reattach falls back to a fresh
/// session (see [`PaneEvent::SessionFallback`]): the persisted scrollback tail
/// to repaint and the recorded foreground command to offer relaunching.
/// Stashed on the pane at materialize time, taken by the loop on the event.
pub struct FallbackRestore {
    pub scrollback: String,
    pub relaunch: Option<String>,
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
    /// For a `Stream` pane: whether dropping the pane DETACHES its session
    /// (keeps the server-side process running) instead of killing it. Default
    /// false — an explicit close must not leak a live process into a relay
    /// lease; quit marks its center-tree panes detached before returning.
    detach_on_drop: Option<Arc<std::sync::atomic::AtomicBool>>,
    /// For a `Stream` pane: the session's PTY child pid on the local host
    /// (the pane daemon's child; 0 = unknown), published by the relay task.
    /// Lets the `/proc`-based cwd/foreground-command capture work for daemon
    /// panes. `None` for a PTY pane (which carries `pid` directly).
    pid_cell: Option<Arc<std::sync::atomic::AtomicU32>>,
    /// Restore payload applied when this pane's warm reattach degrades to a
    /// fresh session (see [`PaneEvent::SessionFallback`]).
    fallback_restore: Option<FallbackRestore>,
    /// Predictive local-echo state — instant keystroke echo on a high-latency
    /// remote pane (the srtt gate auto-enables only on a slow link). See `predict`.
    predictor: crate::predict::Predictor,
    /// Monotonic base for the predictor's round-trip timing (ms since creation).
    predict_clock: std::time::Instant,
    /// Time-travel recording ring (`[replay]`). `None` when replay is disabled —
    /// then `feed` does a single null check and allocates nothing.
    record: Option<crate::replay::Recording>,
    /// Who parses this pane's output into the grid. `false` (default for PTY
    /// panes): the READER THREAD feeds via the emulator's
    /// [`crate::emulator::FeedSink`] — the
    /// expensive escape parsing runs off the event loop, which then only scans
    /// the bytes (queries/OSC/history/predictor). `true`: the loop's
    /// [`PtyPane::feed`] advances the emulator — required for the corner
    /// overlay pane (the kitty relay must feed text pieces at exact positions)
    /// and for Stream panes (no reader thread). Exactly one side ever parses a
    /// given stream — two parsers would split escape state.
    loop_fed: Arc<std::sync::atomic::AtomicBool>,
    /// When the last output chunk was fed to this pane ([`Self::feed`]). Only
    /// touched on the run-loop thread. Feeds the activity FSM's output-hint
    /// signal (see `agent_output`); scrollback repaint on restore bypasses
    /// `feed` and never stamps.
    last_output_at: Option<std::time::Instant>,
    /// When the user last wrote to this pane ([`Self::write_input`]) — output
    /// shortly after user input is keystroke echo, not agent work. Host-generated
    /// protocol replies go through [`Self::write_reply`] and don't stamp.
    last_input_at: Option<std::time::Instant>,
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

/// Short program name for an agent exec pane: the file stem of the command's
/// first word when it is a literal path (`~/.thegn/pi/bin/pi --acp` → `pi`),
/// else the agent's config name (a word carrying shell metacharacters isn't a
/// path — stemming it yields garbage).
pub fn agent_program_name(cmd: &str, choice: &str) -> String {
    cmd.split_whitespace()
        .next()
        .filter(|w| !w.contains(['$', '{', '}', '(', ')', '`']))
        .and_then(|w| std::path::Path::new(w).file_stem())
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| choice.to_string())
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
    /// child — agent panes expect `THEGN_WORKTREE`/`_BRANCH`; a plain pane
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
        // Off-thread grid feed: the reader thread owns a FeedSink (the shared
        // FairMutex<Term> + its own Processor — alacritty's shipped
        // reader/render split), so the expensive escape parsing runs on the
        // reader, off the event loop. The loop still receives the raw bytes
        // for its cheap scans (queries/OSC/history/predictor) and damage
        // marking. `loop_fed` opts a pane back to on-loop parsing (corner
        // overlay; Stream panes).
        let emulator: Box<dyn PaneEmulator> = Box::new(AlacrittyEmulator::new(rows, cols, 10_000));
        let feeder = emulator.feeder();
        let loop_fed = Arc::new(std::sync::atomic::AtomicBool::new(feeder.is_none()));

        // The PTY open + reader thread live in `pane_pty` so the pane daemon's
        // session actor spawns the identical PTY (with `waker: None` and no
        // feed sink — a daemon keeps no grid).
        let pty = crate::pane_pty::open_pty(
            id,
            argv,
            cwd,
            env,
            rows,
            cols,
            tx,
            waker,
            feeder.map(|f| (f, Arc::clone(&loop_fed))),
        )?;

        Ok(Self {
            io: PaneIo::Pty {
                master: pty.master,
                writer: pty.writer,
            },
            emulator,
            rows,
            cols,
            program: program_name(argv),
            history: HistoryBuffer::new(10_000),
            history_partial: Vec::new(),
            history_stripper: AnsiStripper::default(),
            pid: pty.pid,
            pending_relaunch: None,
            session_cell: None,
            detach_on_drop: None,
            pid_cell: None,
            fallback_restore: None,
            predictor: crate::predict::Predictor::new(),
            predict_clock: std::time::Instant::now(),
            record: None,
            loop_fed,
            last_output_at: None,
            last_input_at: None,
        })
    }

    /// Spawn a `Stream` pane backed by an exec-session source — a managed
    /// sandbox's native exec API, or the pane daemon (see
    /// [`crate::pane_source::ExecSource`]). Non-blocking: a relay task runs on
    /// `rt` that opens the session (`open` ⇒ `source.open`, or a reattach), pumps
    /// its output into `tx` as [`PaneEvent`]s (pulsing `waker`), forwards
    /// stdin/resize from the pane's control channel, and publishes the provider
    /// session id for persistence. A connect/exec failure surfaces as an `Exit`.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn_stream(
        id: u32,
        source: Arc<dyn crate::pane_source::ExecSource>,
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
        let detach_on_drop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let pid_cell = Arc::new(std::sync::atomic::AtomicU32::new(0));
        rt.spawn(relay_exec(
            id,
            source,
            provider_name.clone(),
            sandbox_id.clone(),
            open,
            tx,
            waker,
            ctrl_rx,
            session_cell.clone(),
            detach_on_drop.clone(),
            pid_cell.clone(),
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
            detach_on_drop: Some(detach_on_drop),
            pid_cell: Some(pid_cell),
            fallback_restore: None,
            predictor: crate::predict::Predictor::new(),
            predict_clock: std::time::Instant::now(),
            record: None,
            // Stream frames arrive via the relay task's Output events with no
            // reader-side feeder — the loop parses them.
            loop_fed: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            last_output_at: None,
            last_input_at: None,
        }
    }

    /// The pane's child pid on the local host: the PTY child directly, or —
    /// for a daemon-backed stream pane — the daemon session's child as
    /// published by the relay (0 in the cell = not announced yet). `None` for
    /// remote/provider streams, whose pid isn't host-meaningful.
    fn live_pid(&self) -> Option<u32> {
        if self.pid.is_some() {
            return self.pid;
        }
        match self
            .pid_cell
            .as_ref()?
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            0 => None,
            p => Some(p),
        }
    }

    /// The pane's current working directory, read live from `/proc/<pid>/cwd`.
    /// `None` when the pid is unknown, the process is gone, or the symlink can't
    /// be resolved (e.g. a sandbox runtime whose cwd isn't host-meaningful — the
    /// caller gates capture on the host backend regardless). Linux-only; other
    /// platforms (where thegn does not run) return `None`.
    pub fn cwd(&self) -> Option<std::path::PathBuf> {
        let pid = self.live_pid()?;
        std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
    }

    /// The pane's live foreground command (argv + cwd), read from `/proc`: the
    /// shell's foreground child job, when it is a real non-shell program. `None`
    /// for an idle shell prompt, a nested shell, a sandbox/remote runtime child,
    /// an unknown pid, or non-Linux. Captured at persist time so a resurrected
    /// or crashed pane can offer to relaunch what was running.
    pub fn foreground_command(&self) -> Option<crate::session::PaneCmd> {
        let shell = self.live_pid()?;
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

    /// Mark this pane detached-on-drop: dropping it DETACHES its server-side
    /// session (the process keeps running, reattachable by the next launch)
    /// instead of killing it. Quit marks its center-tree panes; the default
    /// (kill) is what every explicit close path needs. No-op for PTY panes.
    pub fn set_detach_on_drop(&self, on: bool) {
        if let Some(flag) = &self.detach_on_drop {
            flag.store(on, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Whether this pane is backed by the pane daemon (survives UI exit).
    pub fn is_daemon_backed(&self) -> bool {
        matches!(&self.io, PaneIo::Stream { provider, .. } if provider == "daemon")
    }

    /// Stash the restore payload for a possible reattach fallback (set at
    /// materialize time from the tab's persisted scrollback/command hints).
    pub fn set_fallback_restore(&mut self, scrollback: Option<String>, relaunch: Option<String>) {
        self.fallback_restore =
            (scrollback.is_some() || relaunch.is_some()).then(|| FallbackRestore {
                scrollback: scrollback.unwrap_or_default(),
                relaunch,
            });
    }

    /// Take the fallback-restore payload (on [`PaneEvent::SessionFallback`]).
    pub fn take_fallback_restore(&mut self) -> Option<FallbackRestore> {
        self.fallback_restore.take()
    }

    /// Feed PTY output into the emulator grid (loop-fed panes only — a
    /// reader-fed pane's grid was already advanced on its reader thread) and
    /// the plain-text history ring. Drain-without-render is just this without
    /// a subsequent compose.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.last_output_at = Some(std::time::Instant::now());
        // Server output is authoritative (and carries the echoed keystrokes), so
        // it retires the prediction overlay + folds a round-trip sample into srtt.
        let now = self.predict_now_ms();
        self.predictor.on_server_output(now);
        if self.loop_fed.load(std::sync::atomic::Ordering::Relaxed) {
            self.emulator.advance(bytes);
        }
        feed_bytes_to_history(
            bytes,
            &mut self.history,
            &mut self.history_partial,
            &mut self.history_stripper,
        );
        // Time-travel recording tap: a third sink beside the emulator and the
        // history ring. `None` (replay disabled) ⇒ one null check, zero alloc.
        if let Some(rec) = &mut self.record {
            rec.push_bytes(bytes, std::time::Instant::now());
        }
    }

    /// Route this pane's grid parsing back onto the event loop (the corner
    /// overlay: the kitty relay must feed text pieces at exact cursor
    /// positions). One-way in practice — a loop-fed pane stays loop-fed.
    pub fn set_loop_fed(&self, on: bool) {
        self.loop_fed
            .store(on, std::sync::atomic::Ordering::Relaxed);
    }

    /// Attach a fresh recording ring so this pane's output is captured for
    /// time-travel replay. Called after spawn when `[replay] enabled`.
    pub fn enable_recording(&mut self, rec: crate::replay::Recording) {
        self.record = Some(rec);
    }

    /// This pane's recording ring, if replay is enabled — for the replay overlay
    /// to reconstruct and search past frames.
    pub fn recording(&self) -> Option<&crate::replay::Recording> {
        self.record.as_ref()
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
        self.last_input_at = Some(std::time::Instant::now());
        if self.emulator.scrollback() > 0 {
            self.emulator.scroll_reset();
        }
        self.write_bytes(bytes)
    }

    /// Write a host-generated protocol reply (DA/DSR/kitty query answers) to the
    /// child: bytes WITHOUT counting as user input — a reply must not mask the
    /// querying app's own output as "keystroke echo" for the activity signal —
    /// and without snapping the viewport out of scrollback.
    pub fn write_reply(&mut self, bytes: &[u8]) -> Result<()> {
        self.write_bytes(bytes)
    }

    /// `(last_output_at, last_input_at)` for the activity output-hint publisher.
    pub fn output_stamps(&self) -> (Option<std::time::Instant>, Option<std::time::Instant>) {
        (self.last_output_at, self.last_input_at)
    }

    fn write_bytes(&mut self, bytes: &[u8]) -> Result<()> {
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
        // No-op when the geometry is unchanged. Re-issuing the winsize would
        // SIGWINCH the child for nothing, and a stray relayout (e.g. a same-size
        // window re-configure on Wayland refocus) must not make full-screen TUIs
        // clear+redraw. Recording a no-op resize would also be pointless.
        if rows == self.rows && cols == self.cols {
            return Ok(());
        }
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
        // Record the resize so replay re-`resize()`s the scratch emulator at the
        // right moment (geometry is part of the reconstructed grid).
        if let Some(rec) = &mut self.record {
            rec.record_resize(rows, cols, std::time::Instant::now());
        }
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
            detach_on_drop: Some(Arc::new(std::sync::atomic::AtomicBool::new(false))),
            pid_cell: Some(Arc::new(std::sync::atomic::AtomicU32::new(0))),
            fallback_restore: None,
            predictor: crate::predict::Predictor::new(),
            predict_clock: std::time::Instant::now(),
            record: None,
            loop_fed: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            last_output_at: None,
            last_input_at: None,
        }
    }

    #[allow(dead_code)]
    pub fn size(&self) -> (u16, u16) {
        (self.rows, self.cols)
    }

    pub fn emulator(&self) -> &dyn PaneEmulator {
        self.emulator.as_ref()
    }

    /// The last `n` lines of this pane's plain-text history, newline-joined
    /// (ANSI already stripped by the history ring). Empty when `n` is 0 or the
    /// pane has produced no output. Captured at persist time for the session
    /// snapshot; trailing blank lines are trimmed so a restored pane doesn't
    /// repaint a wall of emptiness.
    pub fn history_tail(&self, n: usize) -> String {
        if n == 0 || self.history.is_empty() {
            return String::new();
        }
        let total = self.history.len();
        let start = total.saturating_sub(n);
        let mut lines: Vec<&str> = (start..total).filter_map(|i| self.history.get(i)).collect();
        while lines.last().is_some_and(|l| l.trim().is_empty()) {
            lines.pop();
        }
        lines.join("\n")
    }

    /// Repaint captured scrollback into the emulator on restore, so a resurrected
    /// pane shows its recent history before the (fresh) shell produces new output.
    /// The text is fed straight to the emulator (CRLF-normalized) — it is context,
    /// not live output, so it is neither re-recorded into the history ring nor the
    /// replay tap. No-op on empty text.
    pub fn repaint_scrollback(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        // Newlines in the stored tail are bare '\n'; a terminal needs CRLF to
        // return the cursor to column 0, and a trailing CRLF so the live prompt
        // starts on its own line below the restored history.
        let mut bytes = text.replace('\n', "\r\n").into_bytes();
        bytes.extend_from_slice(b"\r\n");
        self.emulator.advance(&bytes);
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
    target = "thegn::frame",
    name = "native_pane",
    skip_all,
    fields(pane = id, provider = %provider_name)
)]
async fn relay_exec(
    id: u32,
    source: Arc<dyn crate::pane_source::ExecSource>,
    provider_name: String,
    sandbox_id: String,
    open: ExecOpen,
    tx: tokio_mpsc::Sender<PaneEvent>,
    waker: Option<TerminalWaker>,
    mut ctrl_rx: tokio_mpsc::Receiver<ExecControl>,
    session_cell: Arc<Mutex<Option<String>>>,
    detach_on_drop: Arc<std::sync::atomic::AtomicBool>,
    pid_cell: Arc<std::sync::atomic::AtomicU32>,
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
        ExecOpen::Attach { fallback, .. } => Some(fallback.clone()),
    };
    tracing::debug!(
        target: "thegn::sandbox",
        provider = %provider_name, sandbox = %sandbox_id, %cols, %rows,
        attach = matches!(open, ExecOpen::Attach { .. }),
        "native exec: opening interactive session"
    );
    let mut fell_back = false;
    let opened = match open {
        ExecOpen::Open(spec) => source.open(&spec).await,
        ExecOpen::Attach {
            session,
            cols,
            rows,
            fallback,
        } => match source.attach(&session, cols, rows).await {
            Ok(s) => Ok(s),
            // The persisted session is gone (lease expired / the daemon
            // restarted — e.g. after a reboot). Degrade to a FRESH session
            // instead of an error husk; `SessionFallback` tells the loop to
            // repaint the persisted scrollback tail + arm the relaunch
            // overlay. Only both failing surfaces the husk below.
            Err(attach_err) => {
                tracing::debug!(
                    target: "thegn::sandbox",
                    pane = id, sandbox = %sandbox_id, %attach_err,
                    "initial reattach failed; opening a fresh session"
                );
                match source.open(&fallback).await {
                    Ok(s) => {
                        fell_back = true;
                        Ok(s)
                    }
                    Err(open_err) => Err(open_err),
                }
            }
        },
    };
    let mut session = match opened {
        Ok(s) => {
            source.report_health(true);
            s
        }
        Err(e) => {
            // Mark the provider unhealthy so `exec=auto` panes fall back to the
            // CLI during the cooldown; surface the failure + a non-zero exit.
            source.report_health(false);
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

    if fell_back {
        let _ = tx.send(PaneEvent::SessionFallback(id)).await;
        wake();
    }

    // Reconnect loop: a transient socket drop with a known session id reattaches
    // (replaying scrollback). Bounded so a permanently-dead session still exits.
    let mut dead = 0u32;
    loop {
        // Publish the (re)connected session's local child pid, when the source
        // knows it (the pane daemon) — persist-time cwd/cmd capture reads it.
        // The daemon adapter announces the id at construction, so it's already
        // in the watch here; provider sources just answer `None`. (Bound
        // before the await: the watch's borrow guard is !Send.)
        let sid_now = session.session_id.borrow().clone();
        if let Some(sid) = sid_now
            && let Some(pid) = source.session_pid(&sid).await
        {
            pid_cell.store(pid, std::sync::atomic::Ordering::Relaxed);
        }
        match relay_session(id, session, &tx, &waker, &mut ctrl_rx, &session_cell).await {
            SessionEnd::Exited(code) => {
                tracing::debug!(
                    target: "thegn::sandbox",
                    pane = id, sandbox = %sandbox_id, code,
                    "exec session exited (command returned)"
                );
                let _ = tx.send(PaneEvent::Exit(id, Some(code))).await;
                wake();
                return;
            }
            SessionEnd::PaneGone => {
                // The pane was dropped. Unless it was marked detached (quit
                // keeps center-tree panes running), this is an explicit close:
                // kill the server-side session so it can't leak a live
                // process into a relay lease. Best-effort — the daemon also
                // reaps on its own terms.
                if !detach_on_drop.load(std::sync::atomic::Ordering::Relaxed)
                    && let Some(sid) = session_cell.lock().ok().and_then(|c| c.clone())
                    && let Err(e) = source.kill_session(&sid).await
                {
                    tracing::debug!(
                        target: "thegn::daemon",
                        pane = id, session = %sid,
                        "close-time session kill failed: {e}"
                    );
                }
                return;
            }
            SessionEnd::Dropped { progressed } => {
                dead = if progressed { 0 } else { dead + 1 };
                tracing::debug!(
                    target: "thegn::sandbox",
                    pane = id, sandbox = %sandbox_id, progressed, dead,
                    "exec session dropped (socket closed, no exit); reconnecting"
                );
                if dead < MAX_DEAD_RECONNECTS {
                    let sid = session_cell.lock().ok().and_then(|c| c.clone());
                    // 1. Prefer reattaching the SAME session: a transient socket
                    //    drop replays scrollback with the shell state preserved.
                    if let Some(sid) = &sid
                        && let Ok(s) = source.attach(sid, cols, rows).await
                    {
                        tracing::debug!(target: "thegn::sandbox", pane = id, "reattached exec session");
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
                        if let Ok(s) = source.open(spec).await {
                            tracing::debug!(
                                target: "thegn::sandbox",
                                pane = id, sandbox = %sandbox_id,
                                "re-opened a FRESH exec session (resumed the sandbox)"
                            );
                            source.report_health(true);
                            session = s;
                            continue;
                        }
                    }
                }
                tracing::warn!(
                    target: "thegn::sandbox",
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

    // Seed from the CURRENT watch value: a watch's initial value is born
    // "seen", so `changed()` below never fires for a source that announces
    // the id at construction (the pane daemon's adapter) — only for one that
    // sends it later (providers). Without this the daemon sid never reached
    // `session_cell`: sessions weren't persisted for reattach, and the
    // close-time kill had no id to kill.
    let mut sid_done = false;
    if let Some(sid) = session_id.borrow().clone()
        && let Ok(mut cell) = session_cell.lock()
    {
        *cell = Some(sid);
        sid_done = true;
    }
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
            Some(PaneEvent::SessionFallback(_)) => {}
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
    fn agent_program_name_stems_or_falls_back() {
        assert_eq!(agent_program_name("claude --continue", "claude"), "claude");
        assert_eq!(
            agent_program_name("/home/u/.thegn/pi/bin/pi --acp", "pi"),
            "pi"
        );
        // Shell metacharacters aren't a path: fall back to the config name.
        assert_eq!(agent_program_name("${AGENT:-claude}", "claude"), "claude");
        assert_eq!(agent_program_name("", "codex"), "codex");
    }

    #[test]
    fn output_input_stamps_track_feed_and_user_input_only() {
        let (ctrl_tx, _ctrl_rx) = tokio_mpsc::channel(8);
        let mut pane = PtyPane::test_stream(ctrl_tx, 24, 80);
        assert_eq!(pane.output_stamps(), (None, None));
        // A host-generated protocol reply stamps neither side — it must not
        // mask the querying app's own output as keystroke echo.
        pane.write_reply(b"\x1b[?6c").unwrap();
        assert_eq!(pane.output_stamps(), (None, None));
        // Restore-time scrollback repaint bypasses `feed`: not live output.
        pane.repaint_scrollback("old history");
        assert_eq!(pane.output_stamps(), (None, None));
        // Live output stamps the output side only.
        pane.feed(b"spinner frame");
        let (out, inp) = pane.output_stamps();
        assert!(out.is_some() && inp.is_none());
        // User input stamps the input side.
        pane.write_input(b"y").unwrap();
        assert!(pane.output_stamps().1.is_some());
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
    fn spawn_with_env_firewalls_launcher_creds_but_keeps_infra() {
        // The clear-then-allowlist firewall: a credential-shaped var present in
        // thegn's OWN environment must NOT reach a spawned pane, while curated
        // infrastructure (PATH) still does. Setting GH_TOKEN here is safe under
        // test parallelism because `host_base_env` filters it out regardless —
        // it can never enter any child — so a transient set corrupts nothing.
        unsafe { std::env::set_var("GH_TOKEN", "leak-me-if-you-can") };
        let (tx, mut rx) = tokio_mpsc::channel(1024);
        let mut pane = PtyPane::spawn_with_env(
            0,
            &sh(r#"printf 'PATH=%s TOK=[%s]' "${PATH:+set}" "$GH_TOKEN""#),
            None,
            &[],
            24,
            80,
            tx,
            None,
        )
        .unwrap();
        assert!(
            drain_until_exit(&mut pane, &mut rx, 5000),
            "child should exit"
        );
        unsafe { std::env::remove_var("GH_TOKEN") };
        let line = pane
            .emulator()
            .row_text(0)
            .map(|r| r.trim_end().to_string())
            .unwrap_or_default();
        assert!(
            line.contains("PATH=set"),
            "base infra env (PATH) must reach the pane: {line:?}"
        );
        assert!(
            line.contains("TOK=[]"),
            "launcher-shell GH_TOKEN must be firewalled out of the pane: {line:?}"
        );
    }

    #[test]
    fn history_tail_captures_recent_output_and_repaint_repaints_it() {
        // A pane that prints three lines: the history ring should hold them, and
        // history_tail returns the bounded, blank-trimmed tail.
        let (tx, mut rx) = tokio_mpsc::channel(1024);
        let mut pane = PtyPane::spawn_with_env(
            0,
            &sh(r"printf 'l1\nl2\nl3\n'"),
            None,
            &[],
            24,
            80,
            tx,
            None,
        )
        .unwrap();
        assert!(drain_until_exit(&mut pane, &mut rx, 5000), "child exits");
        let tail = pane.history_tail(10);
        assert!(
            tail.contains("l2") && tail.contains("l3"),
            "tail keeps recent history: {tail:?}"
        );
        // A cap of 1 keeps at most the single last non-blank line; 0 disables.
        assert!(pane.history_tail(1).lines().count() <= 1);
        assert_eq!(pane.history_tail(0), "");

        // repaint_scrollback feeds captured text straight into a fresh pane's
        // emulator so the restored history lands in the grid before new output.
        let (tx2, _rx2) = tokio_mpsc::channel(1024);
        let mut fresh =
            PtyPane::spawn_with_env(0, &sh("sleep 0.2"), None, &[], 24, 80, tx2, None).unwrap();
        fresh.repaint_scrollback("restored-a\nrestored-b");
        let seen = |needle: &str| {
            (0..24).any(|r| {
                fresh
                    .emulator()
                    .row_text(r)
                    .unwrap_or_default()
                    .contains(needle)
            })
        };
        assert!(seen("restored-a"), "first repainted line lands in the grid");
        assert!(
            seen("restored-b"),
            "second repainted line lands in the grid"
        );
        // Empty text is a no-op (no panic, nothing painted over row 0).
        fresh.repaint_scrollback("");
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
    fn same_size_resize_is_a_noop() {
        // A resize to the current geometry must not touch the child — no winsize
        // change is sent. This guards the Wayland/niri refocus path, where a
        // same-size window re-configure would otherwise SIGWINCH every pane and
        // make full-screen TUIs clear+redraw (a visible black flash).
        let (ctrl_tx, mut ctrl_rx) = tokio_mpsc::channel::<ExecControl>(16);
        let mut pane = PtyPane::test_stream(ctrl_tx, 24, 80);

        // Same size → nothing emitted on the control channel.
        pane.resize(24, 80).unwrap();
        assert!(
            ctrl_rx.try_recv().is_err(),
            "same-size resize must not signal the child"
        );

        // A genuine size change → a Resize control frame is emitted.
        pane.resize(30, 90).unwrap();
        match ctrl_rx.try_recv() {
            Ok(ExecControl::Resize { cols, rows }) => assert_eq!((cols, rows), (90, 30)),
            other => panic!("expected a Resize control frame, got {other:?}"),
        }
    }

    #[test]
    fn stream_pane_relays_frames_input_resize_and_session_id() {
        use std::time::Duration;
        use thegn_svc::provider::{ExecControl, ExecFrame, ExecSession};

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

    /// A hand-built [`crate::pane_source::ExecSource`] for relay tests:
    /// `attach` always fails (a reaped/expired session), `open` hands out the
    /// stashed session once, and `kill_session` records what it was asked to
    /// kill.
    struct TestSource {
        session: Mutex<Option<ExecSession>>,
        kills: Arc<Mutex<Vec<String>>>,
    }

    impl crate::pane_source::ExecSource for TestSource {
        fn open<'a>(
            &'a self,
            _spec: &'a thegn_svc::provider::ExecSpec,
        ) -> futures::future::BoxFuture<'a, Result<ExecSession>> {
            Box::pin(async move {
                self.session
                    .lock()
                    .unwrap()
                    .take()
                    .ok_or_else(|| anyhow::anyhow!("open exhausted"))
            })
        }
        fn attach<'a>(
            &'a self,
            _session: &'a str,
            _cols: u16,
            _rows: u16,
        ) -> futures::future::BoxFuture<'a, Result<ExecSession>> {
            Box::pin(async move { Err(anyhow::anyhow!("session gone (reaped)")) })
        }
        fn kill_session<'a>(
            &'a self,
            session: &'a str,
        ) -> futures::future::BoxFuture<'a, Result<()>> {
            Box::pin(async move {
                self.kills.lock().unwrap().push(session.to_string());
                Ok(())
            })
        }
    }

    /// Drive `relay_exec` with a dead persisted session: the initial attach
    /// fails, the relay falls back to a FRESH open (no error husk), announces
    /// it with `SessionFallback`, seeds `session_cell` from the watch's
    /// initial value, and — because the pane was dropped without a detach
    /// mark — kills the fresh session server-side on `PaneGone`.
    #[test]
    fn dead_reattach_falls_back_fresh_then_close_kills() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let (frames_tx, frames_rx) = tokio_mpsc::channel::<ExecFrame>(8);
        let (prov_ctrl_tx, _prov_ctrl_rx) = tokio_mpsc::channel::<ExecControl>(8);
        // The daemon adapter's shape: the sid is announced at construction.
        let (_sid_tx, sid_rx) = tokio::sync::watch::channel(Some("fresh-sid".to_string()));
        let kills = Arc::new(Mutex::new(Vec::new()));
        let source = Arc::new(TestSource {
            session: Mutex::new(Some(ExecSession {
                frames: frames_rx,
                control: prov_ctrl_tx,
                session_id: sid_rx,
            })),
            kills: kills.clone(),
        });

        let (tx, mut rx) = tokio_mpsc::channel::<PaneEvent>(64);
        let (ctrl_tx, ctrl_rx) = tokio_mpsc::channel::<ExecControl>(8);
        let cell = Arc::new(Mutex::new(None));
        let detach = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let pid_cell = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let spec = thegn_svc::provider::ExecSpec {
            argv: vec!["/bin/sh".into()],
            tty: true,
            cols: 80,
            rows: 24,
            env: vec![],
            cwd: None,
        };
        rt.spawn(relay_exec(
            9,
            source,
            "daemon".into(),
            "local".into(),
            ExecOpen::Attach {
                session: "dead-sid".into(),
                cols: 80,
                rows: 24,
                fallback: spec,
            },
            tx,
            None,
            ctrl_rx,
            cell.clone(),
            detach,
            pid_cell,
        ));

        // 1. The degraded reattach is announced (no husk, no Exit).
        match rx.blocking_recv() {
            Some(PaneEvent::SessionFallback(9)) => {}
            other => panic!("expected SessionFallback, got {other:?}"),
        }
        // 2. The fresh session's output relays normally.
        frames_tx
            .blocking_send(ExecFrame::Stdout(b"fresh".to_vec()))
            .unwrap();
        match rx.blocking_recv() {
            Some(PaneEvent::Output(9, b)) => assert_eq!(b, b"fresh"),
            other => panic!("expected Output, got {other:?}"),
        }
        // 3. The construction-announced sid was seeded into the cell (a
        //    watch's initial value never fires `changed()`).
        assert_eq!(
            cell.lock().unwrap().as_deref(),
            Some("fresh-sid"),
            "sid must seed from the watch's initial value"
        );
        // 4. Dropping the pane WITHOUT a detach mark = explicit close: the
        //    relay kills the server-side session.
        drop(ctrl_tx);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while kills.lock().unwrap().is_empty() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(
            kills.lock().unwrap().as_slice(),
            ["fresh-sid".to_string()],
            "close must kill the session, not leak a lease"
        );
    }

    /// The quit path marks panes detached — dropping one must NOT kill its
    /// session (it keeps running for the next launch to reattach).
    #[test]
    fn detached_pane_drop_does_not_kill_session() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let (frames_tx, frames_rx) = tokio_mpsc::channel::<ExecFrame>(8);
        let (prov_ctrl_tx, _prov_ctrl_rx) = tokio_mpsc::channel::<ExecControl>(8);
        let (_sid_tx, sid_rx) = tokio::sync::watch::channel(Some("kept-sid".to_string()));
        let kills = Arc::new(Mutex::new(Vec::new()));
        let source = Arc::new(TestSource {
            session: Mutex::new(Some(ExecSession {
                frames: frames_rx,
                control: prov_ctrl_tx,
                session_id: sid_rx,
            })),
            kills: kills.clone(),
        });
        let (tx, mut rx) = tokio_mpsc::channel::<PaneEvent>(64);
        let (ctrl_tx, ctrl_rx) = tokio_mpsc::channel::<ExecControl>(8);
        let detach = Arc::new(std::sync::atomic::AtomicBool::new(true)); // quit marked it
        let spec = thegn_svc::provider::ExecSpec {
            argv: vec!["/bin/sh".into()],
            tty: true,
            cols: 80,
            rows: 24,
            env: vec![],
            cwd: None,
        };
        rt.spawn(relay_exec(
            3,
            source,
            "daemon".into(),
            "local".into(),
            ExecOpen::Open(spec),
            tx,
            None,
            ctrl_rx,
            Arc::new(Mutex::new(None)),
            detach,
            Arc::new(std::sync::atomic::AtomicU32::new(0)),
        ));
        // Prove the relay is live, then drop the pane.
        frames_tx
            .blocking_send(ExecFrame::Stdout(b"up".to_vec()))
            .unwrap();
        assert!(matches!(rx.blocking_recv(), Some(PaneEvent::Output(3, _))));
        drop(ctrl_tx);
        // The relay ends on PaneGone; give it a beat, then assert no kill.
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(
            kills.lock().unwrap().is_empty(),
            "a detached pane's drop must keep the session running"
        );
    }

    #[test]
    fn backpressure_does_not_deadlock_on_a_flood() {
        // A chatty child must not block the reader; we drain a bounded window
        // and drop the pane (reader thread exits when the channel sender errors).
        let (tx, mut rx) = tokio_mpsc::channel(1024);
        let mut pane = PtyPane::spawn_with_env(
            0,
            &sh("yes thegn | head -c 200000"),
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
        let seen = (0..rows).any(|r| emu.row_text(r).unwrap_or_default().contains("thegn"));
        assert!(
            seen,
            "expected the repeated token somewhere in the visible grid"
        );
    }
}
