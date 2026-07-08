//! Transport-neutral PTY spawn: open a portable-pty pair, launch the child with
//! the curated env, and start the blocking reader thread that funnels output
//! into a [`PaneEvent`] channel.
//!
//! Extracted from `pane.rs` so both pane owners share it: the compositor's
//! [`crate::pane::PtyPane`] (which passes the `TerminalWaker` so the event loop
//! wakes per chunk) and the pane daemon's session actor (which passes
//! `waker: None` — a daemon has no render loop to wake).

use anyhow::{Context, Result};
use portable_pty::{CommandBuilder, MasterPty, PtySize};
use std::io::Write;
use termwiz::terminal::TerminalWaker;
use tokio::sync::mpsc as tokio_mpsc;

use crate::pane::PaneEvent;

/// The owning half of a spawned PTY: the master (for resize), its writer (for
/// input), and the child pid (for `/proc/<pid>/cwd` reads). The reader thread
/// runs detached and reports through the channel given to [`open_pty`].
pub(crate) struct PtyHandle {
    pub master: Box<dyn MasterPty + Send>,
    pub writer: Box<dyn Write + Send>,
    pub pid: Option<u32>,
}

/// Spawn `argv` (already composed by `sandbox::enter_argv`) in `cwd` on a fresh
/// PTY of `rows`x`cols`, injecting `env` (key/value pairs) into the child.
/// Reader-thread events arrive on `tx`, tagged with `id` so a shared channel
/// can carry every pane's output.
///
/// `waker` (when present) is pulsed after every send so the main loop's
/// blocking `poll_input(None)` returns immediately to drain PTY output — this
/// is what makes the loop event-driven (zero idle wakeups) rather than polled.
#[allow(clippy::too_many_arguments)]
pub(crate) fn open_pty(
    id: u32,
    argv: &[String],
    cwd: Option<&std::path::Path>,
    env: &[(String, String)],
    rows: u16,
    cols: u16,
    tx: tokio_mpsc::Sender<PaneEvent>,
    waker: Option<TerminalWaker>,
) -> Result<PtyHandle> {
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
    // Clear-then-allowlist: a pane does NOT inherit szhost's whole
    // environment (that leaks the launching shell's GH_TOKEN /
    // ANTHROPIC_API_KEY / SSH_AUTH_SOCK past any identity boundary). Start
    // from an empty env seeded only with curated infrastructure vars
    // (`superzej_core::util::host_base_env` — locale/terminal/display + the
    // XDG/DBus vars a rootless container runtime needs), then layer the
    // caller-supplied identity env on top. This is the shared prerequisite
    // for env-bundles (AU) and process-profiles (H). For sandboxed panes the
    // secret VALUES reach the container via the wrapper argv (`-e K=V` /
    // `--setenv`), so clearing the launcher's own env is safe.
    cmd.env_clear();
    for (k, v) in superzej_core::util::host_base_env() {
        cmd.env(k, v);
    }
    // Terminal defaults, unless the caller (or base env) already set them.
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
        // Contain panics: an unwinding reader must still deliver an Exit
        // event, or the pane freezes silently and anything the thread
        // held is poisoned. A panic degrades into a normal pane exit.
        let tx_panic = tx.clone();
        let waker_panic = waker.clone();
        let body = std::panic::AssertUnwindSafe(move || {
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
        if std::panic::catch_unwind(body).is_err() {
            tracing::error!("pane {id} reader thread panicked; reporting pane exit");
            let _ = tx_panic.blocking_send(PaneEvent::Exit(id, None));
            if let Some(w) = &waker_panic {
                let _ = w.wake();
            }
        }
    });

    Ok(PtyHandle {
        master: pair.master,
        writer,
        pid,
    })
}
