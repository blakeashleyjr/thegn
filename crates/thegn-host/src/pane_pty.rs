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
use std::sync::{Arc, Mutex};
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
    /// Shared child/process-group ownership used by every teardown path.
    pub process: PtyProcessOwner,
    /// Join receipt for the blocking PTY reader; shared by pane/actor owners.
    pub reader: PtyReaderJoin,
}

#[derive(Clone)]
pub(crate) struct PtyReaderJoin(Arc<Mutex<Option<std::thread::JoinHandle<()>>>>);

impl PtyReaderJoin {
    fn new(handle: std::thread::JoinHandle<()>) -> Self {
        Self(Arc::new(Mutex::new(Some(handle))))
    }

    pub(crate) fn join(&self) {
        let handle = self
            .0
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .take();
        if let Some(handle) = handle {
            let _ = handle.join();
        }
    }

    pub(crate) async fn join_off_runtime(&self) {
        let receipt = self.clone();
        let _ = tokio::task::spawn_blocking(move || receipt.join()).await;
    }
}

/// Serializes process-group signaling with child reaping. The child handle
/// anchors the Unix group id until TERM/grace/KILL is complete; Windows keeps
/// the corresponding Job Object in the platform handle.
#[derive(Clone)]
pub(crate) struct PtyProcessOwner(Arc<Mutex<PtyProcessState>>);

struct PtyProcessState {
    child: Option<Box<dyn portable_pty::Child + Send + Sync>>,
    group: Option<crate::platform::GroupHandle>,
    complete: bool,
    exit_code: Option<i32>,
}

impl PtyProcessOwner {
    pub(crate) fn new(
        child: Box<dyn portable_pty::Child + Send + Sync>,
        group: Option<crate::platform::GroupHandle>,
    ) -> Self {
        Self(Arc::new(Mutex::new(PtyProcessState {
            child: Some(child),
            group,
            complete: false,
            exit_code: None,
        })))
    }

    /// Bounded tree teardown. No wait/reap occurs until after the final group
    /// signal, keeping the Unix group id anchored to this child; Windows keeps
    /// the corresponding Job Object in the platform handle.
    pub(crate) fn terminate_and_reap(&self) -> Option<i32> {
        let mut state = self.0.lock().unwrap_or_else(|poison| poison.into_inner());
        if state.complete {
            return state.exit_code;
        }
        if let Some(group) = &state.group {
            group.terminate();
        } else if let Some(child) = state.child.as_mut() {
            let _ = child.kill();
        }
        std::thread::sleep(crate::platform::pty_term_grace());
        if let Some(group) = &state.group {
            force_kill_group(group);
        }
        let code = state
            .child
            .as_mut()
            .and_then(|child| child.wait().ok())
            .map(|status| status.exit_code() as i32);
        state.child.take();
        state.exit_code = code;
        state.complete = true;
        code
    }
}

impl Drop for PtyProcessState {
    fn drop(&mut self) {
        if self.complete {
            return;
        }
        if let Some(group) = &self.group {
            group.terminate();
        } else if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
        }
        std::thread::sleep(crate::platform::pty_term_grace());
        if let Some(group) = &self.group {
            force_kill_group(group);
        }
        if let Some(child) = self.child.as_mut() {
            let _ = child.wait();
        }
    }
}

/// Force-terminate the whole owned group, unconditionally.
///
/// On Unix this group was created by us with `setpgid` and its leader stays
/// unreaped until after teardown. **A PGID cannot be reused while any member
/// remains**, so signalling it after the leader exits is safe and is exactly what
/// stops surviving descendants — unlike a stored *PID*, which can be recycled
/// once reaped, which is why `terminate_proxy_pid` re-verifies identity first.
///
/// On Windows teardown is an immediate Job Object hard kill by design: there is no
/// graceful TERM interval and no process-group probe to make.
///
/// Both platforms therefore do the same thing here, so this deliberately carries
/// no `#[cfg]` — the difference is in what `GroupHandle::kill` means per platform,
/// which is `src/platform/`'s business, not this call site's.
fn force_kill_group(group: &crate::platform::GroupHandle) {
    group.kill();
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

    let mut cmd = CommandBuilder::new(&argv[0]);
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
    let group = crate::platform::pty_group(&*child);
    let process = PtyProcessOwner::new(child, group);
    // Drop the slave so the master sees EOF when the child exits.
    drop(pair.slave);

    let writer = pair.master.take_writer().context("take_writer")?;
    let mut reader = pair.master.try_clone_reader().context("clone_reader")?;

    // Published to the pane so its `Drop` knows whether `pid` is still this
    // child's. Only `wait()` returning makes the pid reusable, so this flips
    // there and nowhere else — a child dropped un-waited stays a zombie, whose
    // pid is still safe to signal.
    let process_reader = process.clone();

    // Use std::thread::spawn for the reader - it doesn't require a Tokio runtime
    // but can still use blocking_send on the tokio channel. The child handle
    // moves in here so that, once the read loop ends on PTY EOF, we can
    // `wait()` for the child's exit status and report its code (item 524).
    // Blocking the *reader* thread on `wait()` is safe — it's about to end
    // anyway and never touches the event loop.
    let reader = std::thread::spawn(move || {
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
                            // Consumer gone ⇒ the session is over, which is
                            // exactly when this child must be reaped. `break`,
                            // not `return`: returning here skipped the
                            // `terminate_and_reap()` below and leaked the pane's
                            // shell as a zombie for the life of the process.
                            //
                            // The daemon is what made that unbounded — it holds
                            // the `PtyProcessOwner` so a detached session
                            // survives UI detach, so `PtyProcessState::drop`
                            // (which does kill and wait) never ran either.
                            // Measured: 28 `bash`/`bwrap` zombies parented to one
                            // daemon over three days, every zombie on the
                            // machine. See THE-704.
                            break;
                        }
                        if let Some(w) = &waker {
                            let _ = w.wake(); // best-effort: waker pulse: an input nudge must never fail the calling path
                        }
                    }
                    Err(_) => break, // read error: treat as exit, status unknown
                }
            }
            // Reap the child so the exit carries its real code (None if the
            // status can't be retrieved). u32 → i32 keeps the conventional
            // exit-code range; 0 == success.
            let code = process_reader.terminate_and_reap();
            let _ = tx.blocking_send(PaneEvent::Exit(id, code)); // best-effort: send: the consumer may be gone; a closed channel is the consumer going away
            if let Some(w) = &waker {
                let _ = w.wake(); // best-effort: waker pulse: an input nudge must never fail the calling path
            }
        });
        if std::panic::catch_unwind(body).is_err() {
            tracing::error!("pane {id} reader thread panicked; reporting pane exit");
            let _ = tx_panic.blocking_send(PaneEvent::Exit(id, None)); // best-effort: send: the consumer may be gone; a closed channel is the consumer going away
            if let Some(w) = &waker_panic {
                let _ = w.wake(); // best-effort: waker pulse: an input nudge must never fail the calling path
            }
        }
    });

    Ok(PtyHandle {
        master: pair.master,
        writer,
        pid,
        process,
        reader: PtyReaderJoin::new(reader),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Our own child processes, in any state. A leaked child is a leak whether
    /// it is still running or already a zombie, so this counts both.
    ///
    /// A runtime probe rather than a compile-time platform gate: the host crate's
    /// platform ratchet keeps per-OS compilation out of call sites, and where
    /// `/proc` is absent there is nothing to count.
    fn own_children() -> Vec<(String, String)> {
        let me = std::process::id();
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for entry in entries.filter_map(Result::ok) {
            let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
                continue;
            };
            // comm can hold spaces and parens, so split on the LAST ") ".
            let Some((head, rest)) = stat.rsplit_once(") ") else {
                continue;
            };
            let comm = head.split_once(" (").map(|(_, c)| c).unwrap_or("?");
            let mut fields = rest.split_whitespace();
            let state = fields.next().unwrap_or("?");
            if fields.next().and_then(|p| p.parse::<u32>().ok()) == Some(me) {
                out.push((state.to_string(), comm.to_string()));
            }
        }
        out
    }

    /// A pane whose consumer goes away must still have its child reaped.
    ///
    /// This is THE-704. The reader used to `return` on a failed send — "consumer
    /// gone, don't bother reaping" — which skipped the `terminate_and_reap()`
    /// below the loop. The daemon holds the `PtyProcessOwner` so a detached
    /// session outlives its UI, so `PtyProcessState::drop` never ran either, and
    /// the pane's shell was left unwaited for the life of the process. Measured
    /// on the live daemon: 28 orphans in three days, every zombie on the machine.
    ///
    /// Counting real `/proc` children rather than mocking, because the defect is
    /// precisely that a handle was never waited on — only the process table shows
    /// that.
    #[test]
    fn a_pane_whose_consumer_vanishes_leaves_no_child_behind() {
        // No procfs, no observation.
        if !std::path::Path::new("/proc/self/stat").exists() {
            return;
        }
        let before = own_children().len();

        // Bounded output, then a short sleep: enough writes to fill the channel
        // and force a failed send, and — if the fix regresses — a child that
        // expires on its own rather than a runaway the test suite inherits.
        let argv: Vec<String> = [
            "sh",
            "-c",
            "i=0; while [ $i -lt 5000 ]; do echo xxxxxxxx; i=$((i+1)); done; sleep 10",
        ]
        .iter()
        .map(|s| (*s).to_string())
        .collect();

        // Capacity 1: the reader blocks on the second send, so dropping the
        // receiver reliably lands us on the failed-send path under test.
        let (tx, rx) = tokio_mpsc::channel(1);
        let handle = open_pty(1, &argv, None, &[], 24, 80, tx, None, None).expect("open pty");

        // Let the child produce and the reader fill the channel.
        std::thread::sleep(std::time::Duration::from_millis(250));
        drop(rx); // the consumer goes away, exactly as a UI detach does

        // The reader observes the failed send and must reap before it ends.
        handle.reader.join();

        let after = own_children();
        assert!(
            after.len() <= before,
            "a pane whose consumer vanished left {} child process(es) behind \
             (before={before}, after={}): {after:?}",
            after.len().saturating_sub(before),
            after.len()
        );
    }
}
