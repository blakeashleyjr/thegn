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
use std::ffi::OsStr;
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

    #[cfg(test)]
    pub(crate) fn is_complete(&self) -> bool {
        self.0
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .complete
    }

    /// Bounded tree teardown. No wait/reap occurs until after the final group
    /// signal, keeping the Unix group id anchored to this child; Windows keeps
    /// the corresponding Job Object in the platform handle.
    pub(crate) fn terminate_and_reap(&self) -> Option<i32> {
        let mut state = self.0.lock().unwrap_or_else(|poison| poison.into_inner());
        if state.complete {
            return state.exit_code;
        }
        let code = {
            let PtyProcessState { child, group, .. } = &mut *state;
            terminate_and_reap_parts(child.as_mut(), group.as_ref())
        };
        state.child.take();
        state.exit_code = code;
        state.complete = true;
        code
    }

    /// Run the bounded, blocking process-tree teardown away from a Tokio
    /// worker. The cloned owner preserves serialization with the PTY reader.
    pub(crate) async fn terminate_and_reap_off_runtime(&self) -> Option<i32> {
        let owner = self.clone();
        match tokio::task::spawn_blocking(move || owner.terminate_and_reap()).await {
            Ok(code) => code,
            Err(error) => {
                tracing::warn!(
                    target: "thegn::daemon",
                    %error,
                    "PTY termination/reap task failed"
                );
                None
            }
        }
    }

    /// Start teardown off-thread, then release pane-owned state on the reaper
    /// thread. The shared parcel lets the spawn-failure path keep ownership
    /// until its inline reap has completed.
    pub(crate) fn terminate_and_reap_detached_with(&self, parcel: Box<dyn Send>) {
        self.terminate_and_reap_detached_with_spawner(parcel, |worker| {
            std::thread::Builder::new()
                .name("thegn-pty-reap".into())
                .spawn(worker)
                .map(|_| ())
        });
    }

    fn terminate_and_reap_detached_with_spawner(
        &self,
        parcel: Box<dyn Send>,
        spawn_worker: impl FnOnce(Box<dyn FnOnce() + Send>) -> std::io::Result<()>,
    ) {
        let owner = self.clone();
        let parcel = Arc::new(Mutex::new(Some(parcel)));
        let worker_parcel = Arc::clone(&parcel);
        {
            let (in_flight, _) = &DETACHED_REAPERS;
            let mut in_flight = in_flight
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            *in_flight += 1;
        }
        // Share it because either the worker or inline fallback must decrement only after reap and parcel drop.
        let guard = Arc::new(Mutex::new(Some(DetachedReaperGuard)));
        let worker_guard = Arc::clone(&guard);
        let worker = Box::new(move || {
            crate::platform::qos::set_self(crate::platform::qos::Qos::Background);
            let reaper_guard = worker_guard
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .take();
            owner.terminate_and_reap();
            drop(
                worker_parcel
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .take(),
            );
            drop(reaper_guard);
        });
        let spawn = spawn_worker(worker);
        if let Err(error) = spawn {
            tracing::debug!(target: "thegn::pty", %error, "unable to spawn detached PTY reaper; reaping inline");
            self.terminate_and_reap();
            drop(
                parcel
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .take(),
            );
            drop(
                guard
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .take(),
            );
        }
    }
}

/// Replacement emulator installed during pane drop, so moving the original
/// emulator into the detached reaper does not leave expensive state on-loop.
pub(crate) struct InertPaneEmulator;

impl crate::emulator::PaneEmulator for InertPaneEmulator {
    fn advance(&mut self, _bytes: &[u8]) {}

    fn resize(&mut self, _rows: u16, _cols: u16) {}

    fn size(&self) -> (u16, u16) {
        (0, 0)
    }

    fn cell(&self, _row: u16, _col: u16) -> Option<crate::emulator::GridCell> {
        None
    }

    fn cursor(&self) -> (u16, u16) {
        (0, 0)
    }
}

impl Drop for PtyProcessState {
    fn drop(&mut self) {
        if self.complete {
            return;
        }
        let child = self.child.take();
        let group = self.group.take();
        let teardown = Arc::new(Mutex::new(Some((child, group))));
        let worker_teardown = Arc::clone(&teardown);
        let spawn = std::thread::Builder::new()
            .name("thegn-pty-reap".into())
            .spawn(move || {
                crate::platform::qos::set_self(crate::platform::qos::Qos::Background);
                let parts = worker_teardown
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .take();
                if let Some((mut child, group)) = parts {
                    terminate_and_reap_parts(child.as_mut(), group.as_ref());
                }
            });
        if let Err(error) = spawn {
            tracing::debug!(target: "thegn::pty", %error, "unable to spawn PTY reaper; reaping inline");
            let parts = teardown
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .take();
            if let Some((mut child, group)) = parts {
                terminate_and_reap_parts(child.as_mut(), group.as_ref());
            }
        }
    }
}

fn terminate_and_reap_parts(
    mut child: Option<&mut Box<dyn portable_pty::Child + Send + Sync>>,
    group: Option<&crate::platform::GroupHandle>,
) -> Option<i32> {
    if let Some(group) = group {
        group.terminate();
    } else if let Some(child) = child.as_deref_mut() {
        let _ = child.kill();
    }
    std::thread::sleep(crate::platform::pty_term_grace());
    if let Some(group) = group {
        force_kill_group(group);
    }
    child
        .and_then(|child| child.wait().ok())
        .map(|status| status.exit_code() as i32)
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

static DETACHED_REAPERS: (Mutex<usize>, std::sync::Condvar) =
    (Mutex::new(0), std::sync::Condvar::new());

struct DetachedReaperGuard;

impl Drop for DetachedReaperGuard {
    fn drop(&mut self) {
        let (in_flight, settled) = &DETACHED_REAPERS;
        let mut in_flight = in_flight
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        *in_flight -= 1;
        settled.notify_all();
    }
}

/// Wait for detached pane reapers to finish, bounded by `timeout`.
pub(crate) fn drain_detached_reapers(timeout: std::time::Duration) -> bool {
    let (in_flight, settled) = &DETACHED_REAPERS;
    let in_flight = in_flight
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let started = std::time::Instant::now();
    let mut in_flight = in_flight;
    while *in_flight > 0 {
        let remaining = timeout.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return false;
        }
        let (next, result) = settled
            .wait_timeout(in_flight, remaining)
            .unwrap_or_else(|poison| poison.into_inner());
        in_flight = next;
        if result.timed_out() && *in_flight > 0 {
            return false;
        }
    }
    true
}

#[derive(Debug, PartialEq, Eq)]
enum LauncherError {
    NotOnHostPath(&'static str),
}

impl std::fmt::Display for LauncherError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotOnHostPath(name) => write!(
                formatter,
                "sandbox launcher `{name}` is not on thegn's own PATH; refusing to resolve it from the pane environment"
            ),
        }
    }
}

impl std::error::Error for LauncherError {}

fn host_launcher_path(
    name: &'static str,
    host_path: Option<&OsStr>,
) -> Result<String, LauncherError> {
    let directories = host_path.into_iter().flat_map(std::env::split_paths);
    host_launcher_path_in(name, directories)
}

fn host_launcher_path_in(
    name: &'static str,
    directories: impl IntoIterator<Item = std::path::PathBuf>,
) -> Result<String, LauncherError> {
    directories
        .into_iter()
        .filter(|directory| directory.is_absolute())
        .map(|directory| directory.join(name))
        .find_map(|path| {
            if path.is_file() {
                path.to_str().map(str::to_owned)
            } else {
                None
            }
        })
        .ok_or(LauncherError::NotOnHostPath(name))
}

fn is_loader_var(key: &str) -> bool {
    key.starts_with("LD_") || key.starts_with("DYLD_")
}

fn pin_outer_launcher(
    argv: &[String],
    env: Vec<(String, String)>,
    host_path: Option<&OsStr>,
) -> Result<(Vec<String>, Vec<(String, String)>), LauncherError> {
    let mut pinned_argv = argv.to_vec();
    let mut bwrap_index = None;
    let outer_name = match argv.first().map(String::as_str) {
        Some("bwrap") => Some("bwrap"),
        Some("systemd-run") => Some("systemd-run"),
        _ => None,
    };

    if let Some(name) = outer_name {
        pinned_argv[0] = host_launcher_path(name, host_path)?;
        if name == "bwrap" {
            bwrap_index = Some(0);
        } else if let Some(index) = argv
            .windows(2)
            .position(|pair| pair[0] == "--" && pair[1] == "bwrap")
            .map(|index| index + 1)
        {
            pinned_argv[index] = host_launcher_path("bwrap", host_path)?;
            bwrap_index = Some(index);
        }
    }

    if outer_name.is_none() {
        return Ok((pinned_argv, env));
    }

    let mut loader_vars = Vec::new();
    let mut filtered_env = Vec::with_capacity(env.len());
    for (key, value) in env {
        if is_loader_var(&key) {
            loader_vars.push((key, value));
        } else {
            filtered_env.push((key, value));
        }
    }

    if let Some(index) = bwrap_index {
        let last_occurrences: Vec<usize> = loader_vars
            .iter()
            .enumerate()
            .filter_map(|(i, (key, _))| {
                (!loader_vars[i + 1..].iter().any(|(later, _)| later == key)).then_some(i)
            })
            .collect();
        let setenv_args: Vec<String> = last_occurrences
            .into_iter()
            .flat_map(|pos| {
                let (key, value) = &loader_vars[pos];
                ["--setenv".to_owned(), key.clone(), value.clone()]
            })
            .collect();
        pinned_argv.splice(index + 1..index + 1, setenv_args);
    } else if !loader_vars.is_empty() {
        let keys: Vec<&str> = loader_vars.iter().map(|(key, _)| key.as_str()).collect();
        tracing::debug!(target: "thegn::pty", keys = ?keys, "dropped loader variables from systemd-run environment");
    }

    Ok((pinned_argv, filtered_env))
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
    let mut launch_env = thegn_core::util::host_base_env();
    // Match the existing CommandBuilder environment layering exactly.
    // Terminal defaults, unless the caller (or base env) already set them.
    launch_env.push(("TERM".to_owned(), "xterm-256color".to_owned()));
    // The emulator parses 24-bit SGR; advertise it so apps (btop, modern
    // CLIs) pick truecolor instead of degraded 256-color ramps.
    launch_env.push(("COLORTERM".to_owned(), "truecolor".to_owned()));
    launch_env.extend(env.iter().cloned());
    let (argv, launch_env) =
        pin_outer_launcher(argv, launch_env, std::env::var_os("PATH").as_deref())
            .map_err(anyhow::Error::new)?;

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
    for (k, v) in launch_env {
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
    let reader = thegn_core::thread_name::spawn_named("thegn-pty-read", move || {
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

    struct LauncherFixture {
        _root: tempfile::TempDir,
        host: std::path::PathBuf,
        agent: std::path::PathBuf,
        host_path: std::ffi::OsString,
    }

    impl LauncherFixture {
        fn new(with_host_bwrap: bool) -> Self {
            let root = tempfile::tempdir().expect("private tempdir");
            let host = root.path().join("host");
            let agent = root.path().join("agent");
            std::fs::create_dir(&host).expect("create host bin dir");
            std::fs::create_dir(&agent).expect("create agent bin dir");
            if with_host_bwrap {
                std::fs::write(host.join("bwrap"), b"").expect("create host bwrap");
            }
            std::fs::write(host.join("systemd-run"), b"").expect("create host systemd-run");
            std::fs::write(agent.join("bwrap"), b"").expect("create agent bwrap");
            let host_path = std::env::join_paths([&host]).expect("join host PATH");
            Self {
                _root: root,
                host,
                agent,
                host_path,
            }
        }

        fn host_path(&self) -> &OsStr {
            &self.host_path
        }
    }

    #[test]
    fn bwrap_is_pinned_to_host_path_and_loader_vars_are_contained() {
        let fixture = LauncherFixture::new(true);
        let argv = ["bwrap", "--", "sh"].map(str::to_owned).to_vec();
        let env = vec![
            (
                "PATH".to_owned(),
                fixture.agent.to_string_lossy().into_owned(),
            ),
            ("LD_PRELOAD".to_owned(), "/x/evil.so".to_owned()),
            ("LD_LIBRARY_PATH".to_owned(), "/y".to_owned()),
        ];

        let (pinned, filtered) =
            pin_outer_launcher(&argv, env, Some(fixture.host_path())).expect("host bwrap resolves");

        assert_eq!(
            pinned[0],
            fixture.host.join("bwrap").to_string_lossy().into_owned()
        );
        assert_eq!(
            pinned[1..7].iter().map(String::as_str).collect::<Vec<_>>(),
            vec![
                "--setenv",
                "LD_PRELOAD",
                "/x/evil.so",
                "--setenv",
                "LD_LIBRARY_PATH",
                "/y"
            ]
        );
        assert_eq!(
            pinned[7..].iter().map(String::as_str).collect::<Vec<_>>(),
            vec!["--", "sh"]
        );
        assert!(!filtered.iter().any(|(key, _)| is_loader_var(key)));
        let agent_path = fixture.agent.to_string_lossy().into_owned();
        assert!(
            filtered
                .iter()
                .any(|(key, value)| { key == "PATH" && value == &agent_path })
        );
    }

    #[test]
    fn systemd_scope_pins_outer_and_inner_bwrap() {
        let fixture = LauncherFixture::new(true);
        let argv = [
            "systemd-run",
            "--user",
            "--scope",
            "--",
            "bwrap",
            "--",
            "sh",
        ]
        .map(str::to_owned)
        .to_vec();
        let env = vec![("LD_PRELOAD".to_owned(), "/x/evil.so".to_owned())];

        let (pinned, filtered) = pin_outer_launcher(&argv, env, Some(fixture.host_path()))
            .expect("both host launchers resolve");

        assert_eq!(
            pinned[0],
            fixture
                .host
                .join("systemd-run")
                .to_string_lossy()
                .into_owned()
        );
        let pinned_bwrap = fixture.host.join("bwrap").to_string_lossy().into_owned();
        let inner = pinned
            .iter()
            .position(|arg| arg == &pinned_bwrap)
            .expect("pinned inner bwrap is present");
        assert_eq!(
            pinned[inner + 1..inner + 4]
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            vec!["--setenv", "LD_PRELOAD", "/x/evil.so"]
        );
        assert_eq!(
            pinned[inner + 4..]
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            vec!["--", "sh"]
        );
        assert!(filtered.is_empty());
    }

    #[test]
    fn systemd_without_bwrap_drops_loader_vars() {
        let fixture = LauncherFixture::new(true);
        let argv = ["systemd-run", "--user", "--pty", "sh"]
            .map(str::to_owned)
            .to_vec();
        let env = vec![("LD_PRELOAD".to_owned(), "/x/evil.so".to_owned())];

        let (pinned, filtered) = pin_outer_launcher(&argv, env, Some(fixture.host_path()))
            .expect("host systemd-run resolves");

        assert_eq!(
            pinned[0],
            fixture
                .host
                .join("systemd-run")
                .to_string_lossy()
                .into_owned()
        );
        assert_eq!(
            pinned[1..].iter().map(String::as_str).collect::<Vec<_>>(),
            vec!["--user", "--pty", "sh"]
        );
        assert!(filtered.is_empty());
    }

    #[test]
    fn missing_host_bwrap_refuses_resolution() {
        let fixture = LauncherFixture::new(false);
        let argv = ["bwrap", "--", "sh"].map(str::to_owned).to_vec();
        let result = pin_outer_launcher(&argv, Vec::new(), Some(fixture.host_path()));

        assert!(matches!(result, Err(LauncherError::NotOnHostPath("bwrap"))));
    }

    #[test]
    fn relative_empty_and_dot_host_path_entries_cannot_select_cwd_launchers() {
        let fixture = LauncherFixture::new(true);
        let cwd = std::env::current_dir().expect("current directory");
        let target_components: Vec<_> = fixture.agent.components().collect();
        let cwd_components: Vec<_> = cwd.components().collect();
        let common = cwd_components
            .iter()
            .zip(&target_components)
            .take_while(|(left, right)| left == right)
            .count();
        let mut relative_agent = std::path::PathBuf::new();
        for _ in &cwd_components[common..] {
            relative_agent.push("..");
        }
        for component in &target_components[common..] {
            relative_agent.push(component.as_os_str());
        }
        assert!(relative_agent.join("bwrap").is_file());

        let host_path = std::env::join_paths([
            relative_agent.as_os_str(),
            OsStr::new(""),
            OsStr::new("."),
            fixture.host.as_os_str(),
        ])
        .expect("join hostile and host PATH entries");
        let result = host_launcher_path("bwrap", Some(&host_path));

        assert_eq!(
            result.expect("relative entries are skipped"),
            fixture
                .host
                .join("bwrap")
                .to_str()
                .expect("UTF-8 fixture path")
        );
    }

    #[test]
    fn other_programs_and_explicit_paths_keep_argv_and_env_unchanged() {
        let fixture = LauncherFixture::new(true);
        let env = vec![("LD_PRELOAD".to_owned(), "/x/evil.so".to_owned())];
        for argv in [
            ["podman", "exec", "x"].map(str::to_owned).to_vec(),
            ["/abs/bwrap", "x"].map(str::to_owned).to_vec(),
        ] {
            let (result_argv, result_env) =
                pin_outer_launcher(&argv, env.clone(), Some(fixture.host_path()))
                    .expect("unmanaged command remains unchanged");
            assert_eq!(result_argv, argv);
            assert_eq!(result_env, env);
        }
    }

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
        if !cfg!(target_os = "linux") {
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

    #[test]
    fn dropping_an_unreaped_owner_returns_without_the_grace_sleep() {
        if !cfg!(target_os = "linux") || crate::platform::pty_term_grace().is_zero() {
            return;
        }
        let before = own_children().len();
        let pty = portable_pty::native_pty_system();
        let pair = pty
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("open test PTY");
        let mut command = CommandBuilder::new("sh");
        command.args(["-c", "exec sleep 10"]);
        let child = pair.slave.spawn_command(command).expect("spawn test child");
        drop(pair.slave);
        drop(pair.master);
        let owner = PtyProcessOwner::new(child, None);
        let started = std::time::Instant::now();
        drop(owner);
        assert!(
            started.elapsed() < std::time::Duration::from_millis(150),
            "dropping an owner blocked for {:?}",
            started.elapsed()
        );

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while std::time::Instant::now() < deadline {
            let children = own_children();
            if children.len() <= before {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let after = own_children();
        assert!(
            after.len() <= before,
            "background PTY reaper left a child behind (before={before}, after={}): {after:?}",
            after.len()
        );
    }

    #[test]
    fn drain_detached_reapers_waits_for_inflight_reaps() {
        if !cfg!(target_os = "linux") || crate::platform::pty_term_grace().is_zero() {
            return;
        }
        let before = own_children().len();
        let pty = portable_pty::native_pty_system();
        let pair = pty
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("open test PTY");
        let mut command = CommandBuilder::new("sh");
        command.args(["-c", "exec sleep 10"]);
        let child = pair.slave.spawn_command(command).expect("spawn test child");
        drop(pair.slave);
        drop(pair.master);
        let owner = PtyProcessOwner::new(child, None);

        owner.terminate_and_reap_detached_with(Box::new(()));
        let drained = drain_detached_reapers(std::time::Duration::from_secs(3));
        if !drained {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
            while own_children().len() > before && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
        let after = own_children();
        assert!(
            after.len() <= before,
            "drained detached reapers left a child behind (before={before}, after={}): {after:?}",
            after.len()
        );
    }

    struct ReapDropProbe {
        owner: PtyProcessOwner,
        dropped: std::sync::mpsc::Sender<(Option<String>, bool)>,
    }

    impl Drop for ReapDropProbe {
        fn drop(&mut self) {
            let complete = self
                .owner
                .0
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .complete;
            self.dropped
                .send((std::thread::current().name().map(str::to_owned), complete))
                .unwrap();
        }
    }

    fn live_test_owner() -> PtyProcessOwner {
        let pty = portable_pty::native_pty_system();
        let pair = pty
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("open test PTY");
        let mut command = CommandBuilder::new("sh");
        command.args(["-c", "exec sleep 10"]);
        let child = pair.slave.spawn_command(command).expect("spawn test child");
        drop(pair.slave);
        drop(pair.master);
        PtyProcessOwner::new(child, None)
    }

    #[test]
    fn detached_reaper_drops_parcel_after_reap_on_named_thread() {
        if !cfg!(target_os = "linux") {
            return;
        }
        let owner = live_test_owner();
        let (dropped, received) = std::sync::mpsc::channel();
        owner.terminate_and_reap_detached_with(Box::new(ReapDropProbe {
            owner: owner.clone(),
            dropped,
        }));

        let (thread_name, complete) = received
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("reaper should release the parcel");
        assert_eq!(thread_name.as_deref(), Some("thegn-pty-reap"));
        assert!(complete, "the parcel must be dropped after the reap");
    }

    #[test]
    fn detached_reaper_spawn_failure_drops_parcel_after_inline_reap() {
        if !cfg!(target_os = "linux") {
            return;
        }
        let owner = live_test_owner();
        let (dropped, received) = std::sync::mpsc::channel();
        let parcel = Box::new(ReapDropProbe {
            owner: owner.clone(),
            dropped,
        });
        owner.terminate_and_reap_detached_with_spawner(parcel, |_worker| {
            Err(std::io::Error::other("injected thread spawn failure"))
        });

        let (thread_name, complete) = received
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("inline fallback should release the parcel");
        assert_eq!(thread_name.as_deref(), std::thread::current().name());
        assert!(complete, "the parcel must be dropped after the inline reap");
    }
}
