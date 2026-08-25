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
    /// Fields drop in declaration order, so `master` closes before `writer`.
    ///
    /// What pane close on Windows actually costs is measured by
    /// `examples/conpty_teardown_windows`: **0 threads, 0 handles and 0
    /// orphaned `OpenConsole.exe` processes per close**, over 10 panes in each
    /// of the two arms (child exits on its own; child terminated while alive).
    /// Counting the console host matters — it is a separate process, so an
    /// in-process-only count reports a clean zero while whole processes leak.
    ///
    /// A previous version of this comment claimed the order was load-bearing —
    /// that dropping `writer` first deadlocks a terminated child. That came
    /// from a single observation and does **not** reproduce: twelve later runs
    /// completed in every order. The likely culprit is the ConPTY DSR stall (a
    /// child waits for a cursor-position answer that never comes) hitting the
    /// harness, not the drop order. Treat the ordering as unproven rather than
    /// as an invariant, and re-measure before relying on it either way.
    pub master: Box<dyn MasterPty + Send>,
    pub writer: Box<dyn Write + Send>,
    pub pid: Option<u32>,
    /// Set by the reader thread the instant `child.wait()` returns — i.e. the
    /// instant `pid` stops identifying this child and becomes reusable by the
    /// OS. The pane's `Drop` reads it so an explicit reap can never signal a
    /// recycled pid. See [`crate::pane::PtyPane`]'s `Drop`.
    pub reaped: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

/// Spawn `argv` (already composed by `sandbox::enter_argv`) in `cwd` on a fresh
/// PTY of `rows`x`cols`, injecting `env` (key/value pairs) into the child.
/// Reader-thread events arrive on `tx`, tagged with `id` so a shared channel
/// can carry every pane's output.
///
/// `waker` (when present) is pulsed after every send so the main loop's
/// blocking `poll_input(None)` returns immediately to drain PTY output — this
/// is what makes the loop event-driven (zero idle wakeups) rather than polled.
///
/// `feed` (when present) is the pane's off-thread grid sink: the reader parses
/// each chunk into the shared emulator HERE (one lock per ≤64KB read) so the
/// expensive escape parsing never runs on the event loop — unless the paired
/// `loop_fed` flag flips the pane back to on-loop parsing (the corner overlay,
/// whose kitty relay must feed text pieces at exact cursor positions). The
/// pane daemon passes `None` — it keeps no grid.
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
    feed: Option<(
        Box<dyn crate::emulator::FeedSink>,
        std::sync::Arc<std::sync::atomic::AtomicBool>,
    )>,
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

    // Resolve argv0 through PATH+PATHEXT. portable-pty only ever tries
    // `<name>.exe` on Windows, so a pane whose program is a `.cmd` shim --
    // `npm`, `pnpm`, `gh`, most configured `[[agents]]` -- failed to spawn.
    let mut cmd = CommandBuilder::new(thegn_core::util::resolve_program(&argv[0]));
    cmd.args(&argv[1..]);
    if let Some(dir) = cwd {
        cmd.cwd(dir);
    }
    // Clear-then-allowlist: a pane does NOT inherit thegn's whole
    // environment (that leaks the launching shell's GH_TOKEN /
    // ANTHROPIC_API_KEY / SSH_AUTH_SOCK past any identity boundary). Start
    // from an empty env seeded only with curated infrastructure vars
    // (`thegn_core::util::host_base_env` — locale/terminal/display + the
    // XDG/DBus vars a rootless container runtime needs), then layer the
    // caller-supplied identity env on top. This is the shared prerequisite
    // for env-bundles (AU) and process-profiles (H). For sandboxed panes the
    // secret VALUES reach the container via the wrapper argv (`-e K=V` /
    // `--setenv`), so clearing the launcher's own env is safe.
    cmd.env_clear();
    for (k, v) in thegn_core::util::host_base_env() {
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
    let child = pair.slave.spawn_command(cmd).context("spawn child")?;
    // Capture the pid before `child` moves into the reader thread below —
    // it's the handle we use to read the pane's live cwd for persistence.
    let pid = child.process_id();
    // Drop the slave so the master sees EOF when the child exits.
    drop(pair.slave);

    // Who reaps the child and reports its exit code.
    //
    // On unix that is the reader thread: dropping the slave above means the
    // master read returns 0 the moment the child exits, so the read loop ends
    // and reaps on its way out.
    //
    // ConPTY does not work that way. The pseudoconsole outlives the child —
    // dropping the slave does not close it — so the master read simply blocks
    // forever and the reader loop never ends. Left alone, a Windows pane whose
    // command finished never emits `PaneEvent::Exit`: no "process finished"
    // notification, no reap, and `drain_until_exit` waits out its deadline.
    // So on Windows a dedicated waiter thread owns the child and reports the
    // exit, and the reader just ends whenever the master is finally dropped.
    //
    // Waiting on the child alone is not enough to report the exit, though.
    // `child.wait()` returns the instant the process dies, while its final
    // output is still sitting in the pseudoconsole waiting to be read — so a
    // bare waiter races the reader and `Exit` can overtake the last chunk. A
    // consumer that stops on `Exit` (the drain helper, and anything in the loop
    // that tears a pane down on it) then loses the tail: a pane that printed
    // and quit came out blank. Unix has no such race — there the reader itself
    // sees EOF *after* the final read and reports from there.
    //
    // So the Windows waiter reports only once the reader has gone quiet: it
    // watches the reader's byte counter and reports when it stops moving,
    // bounded so a child that keeps a grandchild writing can't defer the exit
    // forever.
    #[cfg(windows)]
    let read_bytes = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    #[cfg(not(windows))]
    let child_to_reap = Some(child);
    #[cfg(windows)]
    let child_to_reap: Option<Box<dyn portable_pty::Child + Send + Sync>> = {
        use std::sync::atomic::Ordering;
        /// How long the reader must be idle before the exit is believed final.
        const QUIET: std::time::Duration = std::time::Duration::from_millis(60);
        /// Ceiling on that wait, so the exit is never withheld indefinitely.
        const MAX_FLUSH: std::time::Duration = std::time::Duration::from_millis(1500);

        let mut child = child;
        let tx_wait = tx.clone();
        let waker_wait = waker.clone();
        let counter = read_bytes.clone();
        std::thread::spawn(move || {
            let code = child.wait().ok().map(|s| s.exit_code() as i32);
            let deadline = std::time::Instant::now() + MAX_FLUSH;
            let mut last = counter.load(Ordering::Relaxed);
            loop {
                std::thread::sleep(QUIET);
                let now = counter.load(Ordering::Relaxed);
                if now == last || std::time::Instant::now() >= deadline {
                    break;
                }
                last = now;
            }
            let _ = tx_wait.blocking_send(PaneEvent::Exit(id, code));
            if let Some(w) = &waker_wait {
                let _ = w.wake();
            }
        });
        None
    };

    let writer = pair.master.take_writer().context("take_writer")?;
    let mut reader = pair.master.try_clone_reader().context("clone_reader")?;

    // Published to the pane so its `Drop` knows whether `pid` is still this
    // child's. Only `wait()` returning makes the pid reusable, so this flips
    // there and nowhere else — a child dropped un-waited stays a zombie, whose
    // pid is still safe to signal.
    let reaped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let reaped_reader = std::sync::Arc::clone(&reaped);

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
        let mut feed = feed;
        let body = std::panic::AssertUnwindSafe(move || {
            // 64KB per read: at full flood this is 8× fewer channel sends +
            // waker pulses than an 8KB buffer at identical throughput (chunk
            // boundaries are arbitrary either way, and the drain's budget is
            // byte-based, so chunk size doesn't affect fairness).
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break, // EOF: child exited or PTY closed
                    Ok(n) => {
                        // Publish progress before the send, so the Windows
                        // waiter above sees output land and holds `Exit` back.
                        #[cfg(windows)]
                        read_bytes.fetch_add(n as u64, std::sync::atomic::Ordering::Relaxed);
                        // Parse into the shared grid here (one lock per chunk)
                        // unless the pane went loop-fed.
                        if let Some((sink, loop_fed)) = feed.as_mut()
                            && !loop_fed.load(std::sync::atomic::Ordering::Relaxed)
                        {
                            sink.advance(&buf[..n]);
                        }
                        // One exact-sized Vec per chunk: ownership must cross
                        // the channel, and the read buffer is reused, so this
                        // is the minimal copy (a buffer pool would add
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
            //
            // `None` means Windows, where the waiter thread spawned above
            // already owns the reap and the Exit report — sending a second one
            // here would double-report the pane's death.
            let Some(mut child) = child_to_reap else {
                return;
            };
            let code = child.wait().ok().map(|s| s.exit_code() as i32);
            // The pid is reusable from here on — tell the pane's Drop to stop
            // treating it as this child's.
            reaped_reader.store(true, std::sync::atomic::Ordering::SeqCst);
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
        reaped,
    })
}
