//! A single PTY-backed pane: a child process on a pseudo-terminal, its emulator
//! grid, and an input writer. The reader runs on a blocking thread that funnels
//! bytes into a channel (portable-pty masters are blocking file handles — one
//! reader per pane, never a `select!` over N masters in the event loop).

use anyhow::{Context, Result};
use portable_pty::{CommandBuilder, MasterPty, PtySize};
use std::io::Write;

use termwiz::terminal::TerminalWaker;
use tokio::sync::mpsc as tokio_mpsc;

use superzej_core::history::{AnsiStripper, HistoryBuffer, feed_bytes_to_history};

use crate::emulator::{PaneEmulator, Vt100Emulator};

/// What a pane's reader thread emits (tagged with the pane id so one shared
/// channel multiplexes every pane's output to the event loop).
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
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
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

// ── Live process introspection ───────────────────────────────────────────────
// Linux reads these from `/proc`. macOS has no `/proc`; the real implementation
// is `libproc` (`proc_listchildpids`, `proc_pidpath`, `PROC_PIDVNODEPATHINFO` for
// cwd) and is tracked as on-device work (tasks.md §AV / item 701) — until then the
// non-Linux stubs degrade gracefully (no foreground-command capture / cwd
// persistence) without breaking the build.

/// The most-recently-started direct child of `pid` (the shell's foreground job),
/// if any. Walks `/proc/*/stat` for the `ppid` field; ties break to the highest
/// pid (newest).
#[cfg(target_os = "linux")]
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
#[cfg(target_os = "linux")]
fn stat_ppid(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(1)?.parse().ok()
}

/// Parse `/proc/<pid>/cmdline` (NUL-separated argv) into a `Vec`, dropping empty
/// trailing entries. `None` when unreadable or empty (e.g. a kernel thread).
#[cfg(target_os = "linux")]
fn read_cmdline(pid: u32) -> Option<Vec<String>> {
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let argv: Vec<String> = raw
        .split(|&b| b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect();
    (!argv.is_empty()).then_some(argv)
}

/// The working directory of `pid`, read from `/proc/<pid>/cwd`.
#[cfg(target_os = "linux")]
fn pid_cwd(pid: u32) -> Option<std::path::PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
}

// ── macOS (`libproc`/`sysctl` via libc — no /proc) ───────────────────────────
// NOTE: written without an Apple-silicon host to compile against; verify on
// device (tasks.md §AV / item 701). Uses only `libc` (already a dependency); the
// proc_info / KERN_PROCARGS2 ABI is stable across macOS releases.
#[cfg(target_os = "macos")]
fn all_pids() -> Vec<u32> {
    let probe = unsafe { libc::proc_listpids(libc::PROC_ALL_PIDS, 0, std::ptr::null_mut(), 0) };
    if probe <= 0 {
        return Vec::new();
    }
    let cap = probe as usize / std::mem::size_of::<u32>() + 32;
    let mut buf = vec![0u32; cap];
    let bytes = (buf.len() * std::mem::size_of::<u32>()) as libc::c_int;
    let n = unsafe {
        libc::proc_listpids(
            libc::PROC_ALL_PIDS,
            0,
            buf.as_mut_ptr() as *mut libc::c_void,
            bytes,
        )
    };
    if n <= 0 {
        return Vec::new();
    }
    buf.truncate(n as usize / std::mem::size_of::<u32>());
    buf.into_iter().filter(|&p| p != 0).collect()
}

#[cfg(target_os = "macos")]
fn ppid_of(pid: u32) -> Option<u32> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let sz = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let n = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTBSDINFO,
            0,
            &mut info as *mut _ as *mut libc::c_void,
            sz,
        )
    };
    (n >= sz).then_some(info.pbi_ppid)
}

/// Most-recently-started direct child of `pid`, via `proc_listpids` + each pid's
/// BSD-info `pbi_ppid` (the macOS analogue of the `/proc/*/stat` walk).
#[cfg(target_os = "macos")]
fn newest_child(pid: u32) -> Option<u32> {
    let mut best: Option<u32> = None;
    for child in all_pids() {
        if child != pid && ppid_of(child) == Some(pid) {
            best = Some(best.map_or(child, |b| b.max(child)));
        }
    }
    best
}

/// Full argv of `pid` via `sysctl(KERN_PROCARGS2)`. Layout is
/// `[argc:i32][exec_path\0][\0…padding][argv0\0 argv1\0 …][env…]`.
#[cfg(target_os = "macos")]
fn read_cmdline(pid: u32) -> Option<Vec<String>> {
    let mut argmax: libc::c_int = 0;
    let mut sz = std::mem::size_of::<libc::c_int>();
    let mut mib_max = [libc::CTL_KERN, libc::KERN_ARGMAX];
    unsafe {
        libc::sysctl(
            mib_max.as_mut_ptr(),
            2,
            &mut argmax as *mut _ as *mut libc::c_void,
            &mut sz,
            std::ptr::null_mut(),
            0,
        );
    }
    if argmax <= 0 {
        return None;
    }
    let mut buf = vec![0u8; argmax as usize];
    let mut len = buf.len();
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid as libc::c_int];
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            buf.as_mut_ptr() as *mut libc::c_void,
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 || len < 4 {
        return None;
    }
    buf.truncate(len);
    let argc = i32::from_ne_bytes([buf[0], buf[1], buf[2], buf[3]]).max(0) as usize;
    let mut i = 4;
    while i < buf.len() && buf[i] != 0 {
        i += 1; // skip exec_path
    }
    while i < buf.len() && buf[i] == 0 {
        i += 1; // skip NUL padding
    }
    let mut argv = Vec::with_capacity(argc);
    for _ in 0..argc {
        if i >= buf.len() {
            break;
        }
        let start = i;
        while i < buf.len() && buf[i] != 0 {
            i += 1;
        }
        if i > start {
            argv.push(String::from_utf8_lossy(&buf[start..i]).into_owned());
        }
        i += 1; // skip NUL
    }
    (!argv.is_empty()).then_some(argv)
}

/// Working directory of `pid` via `proc_pidinfo(PROC_PIDVNODEPATHINFO)`.
#[cfg(target_os = "macos")]
fn pid_cwd(pid: u32) -> Option<std::path::PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let mut info: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
    let sz = std::mem::size_of::<libc::proc_vnodepathinfo>() as libc::c_int;
    let n = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDVNODEPATHINFO,
            0,
            &mut info as *mut _ as *mut libc::c_void,
            sz,
        )
    };
    if n < sz {
        return None;
    }
    let path = &info.pvi_cdir.vip_path;
    let bytes = unsafe { std::slice::from_raw_parts(path.as_ptr() as *const u8, path.len()) };
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    (end > 0).then(|| std::path::PathBuf::from(std::ffi::OsStr::from_bytes(&bytes[..end])))
}

// ── Other targets (superzej does not run here) ───────────────────────────────
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn newest_child(_pid: u32) -> Option<u32> {
    None
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn read_cmdline(_pid: u32) -> Option<Vec<String>> {
    None
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn pid_cwd(_pid: u32) -> Option<std::path::PathBuf> {
    None
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
            master: pair.master,
            writer,
            emulator: Box::new(Vt100Emulator::new(rows, cols, 10_000)),
            rows,
            cols,
            program: program_name(argv),
            history: HistoryBuffer::new(10_000),
            history_partial: Vec::new(),
            history_stripper: AnsiStripper::default(),
            pid,
            pending_relaunch: None,
        })
    }

    /// The pane's current working directory, read live from `/proc/<pid>/cwd`.
    /// `None` when the pid is unknown, the process is gone, or the symlink can't
    /// be resolved (e.g. a sandbox runtime whose cwd isn't host-meaningful — the
    /// caller gates capture on the host backend regardless). Linux-only; other
    /// platforms (where superzej does not run) return `None`.
    pub fn cwd(&self) -> Option<std::path::PathBuf> {
        let pid = self.pid?;
        pid_cwd(pid)
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
        let cwd = pid_cwd(child).map(|p| p.to_string_lossy().into_owned());
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
        self.emulator.advance(bytes);
        feed_bytes_to_history(
            bytes,
            &mut self.history,
            &mut self.history_partial,
            &mut self.history_stripper,
        );
    }

    /// Forward user input bytes to the child. Typing snaps the viewport back to
    /// the live tail (the usual terminal behavior when scrolled into history).
    pub fn write_input(&mut self, bytes: &[u8]) -> Result<()> {
        if self.emulator.scrollback() > 0 {
            self.emulator.scroll_reset();
        }
        self.writer.write_all(bytes).context("pty write")?;
        self.writer.flush().ok();
        Ok(())
    }

    /// Resize the PTY and the emulator together.
    pub fn resize(&mut self, rows: u16, cols: u16) -> Result<()> {
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("pty resize")?;
        self.emulator.resize(rows, cols);
        self.rows = rows;
        self.cols = cols;
        Ok(())
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
