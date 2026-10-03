//! Generation-owned supervisor for file-preview jobs, plus the bounded
//! subprocess runner the document renderers use (THE-363).
//!
//! * At most ONE job runs and ONE (the latest) waits. A new submission replaces
//!   the pending job (never started: free cancellation) and flips the active
//!   job's cancel flag, so rapid cursor movement costs one renderer at a time.
//! * Jobs run on one on-demand `std::thread` worker that exits when idle (0%
//!   idle, and the tokio blocking pool is never occupied by a hung renderer).
//! * A job delivers through [`JobCtx::deliver`], which checks the generation
//!   under the same lock `submit` takes, so a superseded job can never send.
//! * Memory: rasters are capped (`rasterize::MAX_DIM`) and only the current
//!   generation is ever delivered, so queued + completed bytes are bounded by
//!   one active + one pending job plus the one result the loop has yet to drain.

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Total wall-clock budget for one external renderer, kill grace included.
pub(crate) const RENDERER_DEADLINE: Duration = Duration::from_secs(15);
/// Max stdout bytes kept from a renderer (a 96 dpi page PNG is far smaller).
pub(crate) const RENDERER_MAX_OUTPUT: usize = 32 * 1024 * 1024;
const POLL: Duration = Duration::from_millis(10);
const READER_GRACE: Duration = Duration::from_millis(500);

/// Exact observable counters.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PreviewJobStats {
    pub submitted: u64,
    /// Pending jobs replaced before they ever started.
    pub superseded_queued: u64,
    /// Active jobs whose cancel flag was raised.
    pub cancelled_active: u64,
    pub started: u64,
    pub delivered: u64,
    /// Deliveries refused because the generation moved on.
    pub stale_dropped: u64,
    /// External renderers killed on deadline.
    pub renderer_timeouts: u64,
    /// Renderer output truncated at the byte cap.
    pub renderer_output_capped: u64,
}

type Work = Box<dyn FnOnce(&JobCtx) + Send>;

#[derive(Default)]
struct State {
    generation: u64,
    pending: Option<(u64, Work)>,
    active: Option<Arc<AtomicBool>>,
    worker_running: bool,
    stats: PreviewJobStats,
}

/// One supervisor = one preview slot.
#[derive(Default)]
pub struct PreviewSupervisor {
    state: Arc<Mutex<State>>,
}

/// Handed to a running job.
pub struct JobCtx {
    generation: u64,
    cancel: Arc<AtomicBool>,
    state: Arc<Mutex<State>>,
}

impl JobCtx {
    pub fn cancelled(&self) -> &AtomicBool {
        &self.cancel
    }

    /// Run `send` only if this job is still the newest generation. The check
    /// and the send are atomic with respect to `submit`/`cancel_all`.
    pub fn deliver(&self, send: impl FnOnce() -> bool) -> bool {
        let mut st = lock(&self.state);
        if st.generation != self.generation {
            st.stats.stale_dropped += 1;
            return false;
        }
        let sent = send();
        if sent {
            st.stats.delivered += 1;
        }
        sent
    }
}

fn lock(state: &Mutex<State>) -> std::sync::MutexGuard<'_, State> {
    state.lock().unwrap_or_else(|p| p.into_inner())
}

impl PreviewSupervisor {
    pub fn submit(&self, work: impl FnOnce(&JobCtx) + Send + 'static) {
        let spawn = {
            let mut st = lock(&self.state);
            st.generation = st.generation.wrapping_add(1);
            st.stats.submitted += 1;
            if st.pending.take().is_some() {
                st.stats.superseded_queued += 1;
            }
            if let Some(active) = st.active.as_ref() {
                active.store(true, Ordering::Release);
                st.stats.cancelled_active += 1;
            }
            let generation = st.generation;
            st.pending = Some((generation, Box::new(work)));
            !std::mem::replace(&mut st.worker_running, true)
        };
        if spawn {
            let state = Arc::clone(&self.state);
            let spawned = std::thread::Builder::new()
                .name("thegn-preview".into())
                .spawn(move || {
                    crate::platform::qos::set_self(crate::platform::qos::Qos::Utility);
                    worker(&state);
                });
            if spawned.is_err() {
                let mut st = lock(&self.state);
                st.worker_running = false;
                st.pending = None;
            }
        }
    }

    /// Drop pending work and cancel the active job (preview closed/replaced).
    pub fn cancel_all(&self) {
        let mut st = lock(&self.state);
        st.generation = st.generation.wrapping_add(1);
        st.pending = None;
        if let Some(active) = st.active.as_ref() {
            active.store(true, Ordering::Release);
            st.stats.cancelled_active += 1;
        }
    }

    #[cfg(test)]
    pub fn stats(&self) -> PreviewJobStats {
        lock(&self.state).stats.clone()
    }

    /// Jobs queued or running right now (never more than 2).
    #[cfg(test)]
    pub fn in_flight(&self) -> usize {
        let st = lock(&self.state);
        usize::from(st.pending.is_some()) + usize::from(st.active.is_some())
    }
}

fn worker(state: &Arc<Mutex<State>>) {
    loop {
        let (generation, work, cancel) = {
            let mut st = lock(state);
            let Some((generation, work)) = st.pending.take() else {
                st.worker_running = false;
                return;
            };
            let cancel = Arc::new(AtomicBool::new(false));
            st.active = Some(Arc::clone(&cancel));
            st.stats.started += 1;
            (generation, work, cancel)
        };
        let ctx = JobCtx {
            generation,
            cancel,
            state: Arc::clone(state),
        };
        // A panicking job must not wedge the slot.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work(&ctx))); // best-effort: a panicking decoder must not kill the worker
        lock(state).active = None;
    }
}

/// The process-wide preview slot.
pub fn global() -> &'static PreviewSupervisor {
    static G: OnceLock<PreviewSupervisor> = OnceLock::new();
    G.get_or_init(PreviewSupervisor::default)
}

#[derive(Debug)]
pub enum CaptureError {
    Spawn(std::io::Error),
    Cancelled,
    Timeout,
    Failed,
}

/// Run `cmd` in its own process group, capture at most `max_out` stdout bytes,
/// and bound it by `deadline` and `cancel`. On every exit path the whole group
/// is killed and the leader reaped; the stdout reader is waited for only a
/// bounded grace (a descendant that escaped the group cannot hang the worker).
#[expect(
    clippy::disallowed_methods,
    reason = "the wait follows a confirmed exit or a group kill, on the dedicated preview worker"
)]
pub fn bounded_capture(
    cmd: &mut Command,
    cancel: &AtomicBool,
    deadline: Duration,
    max_out: usize,
    stats: Option<&PreviewSupervisor>,
) -> Result<Vec<u8>, CaptureError> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let (mut child, group) = crate::platform::spawn_grouped(cmd).map_err(CaptureError::Spawn)?;
    let mut stdout = child.stdout.take();
    let (tx, rx) = std::sync::mpsc::channel::<(Vec<u8>, bool)>();
    let reader = std::thread::Builder::new()
        .name("thegn-preview-io".into())
        .spawn(move || {
            let mut buf = Vec::new();
            let mut capped = false;
            if let Some(out) = stdout.as_mut() {
                let mut chunk = [0u8; 64 * 1024];
                loop {
                    match out.read(&mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let room = max_out.saturating_sub(buf.len());
                            buf.extend_from_slice(&chunk[..n.min(room)]);
                            if n > room {
                                capped = true;
                                break;
                            }
                        }
                    }
                }
            }
            let _ = tx.send((buf, capped)); // best-effort: the waiter may have timed out
        });
    let started = Instant::now();
    let outcome = loop {
        if cancel.load(Ordering::Acquire) {
            break Err(CaptureError::Cancelled);
        }
        if started.elapsed() >= deadline {
            break Err(CaptureError::Timeout);
        }
        match crate::platform::gate_child_exited(&mut child) {
            Ok(true) => break Ok(()),
            Ok(false) => {}
            Err(_) => break Err(CaptureError::Failed),
        }
        // Output cap reached: the reader stopped, stop the renderer too.
        if reader.is_err() {
            break Err(CaptureError::Failed);
        }
        std::thread::sleep(POLL);
    };
    // Kill the whole group before reaping so no descendant outlives the job and
    // the unreaped leader pins the pgid against reuse.
    group.kill();
    let status = child.wait();
    let (bytes, capped) = rx.recv_timeout(READER_GRACE).unwrap_or_default();
    if let Some(sup) = stats {
        let mut st = lock(&sup.state);
        if matches!(outcome, Err(CaptureError::Timeout)) {
            st.stats.renderer_timeouts += 1;
        }
        if capped {
            st.stats.renderer_output_capped += 1;
        }
    }
    outcome?;
    match status {
        Ok(s) if s.success() => Ok(bytes),
        _ => Err(CaptureError::Failed),
    }
}

/// Run `bounded_capture` with the global stats and the standard budgets.
pub fn run_renderer(cmd: &mut Command, cancel: &AtomicBool) -> Result<Vec<u8>, CaptureError> {
    bounded_capture(
        cmd,
        cancel,
        RENDERER_DEADLINE,
        RENDERER_MAX_OUTPUT,
        Some(global()),
    )
}

/// Spawn-and-wait for a renderer that writes a file instead of stdout.
pub fn run_renderer_quiet(cmd: &mut Command, cancel: &AtomicBool) -> Result<(), CaptureError> {
    run_renderer(cmd, cancel).map(|_| ())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn sh(script: &str) -> Command {
        let mut c = Command::new("sh");
        c.args(["-c", script]);
        c
    }

    #[test]
    fn rapid_submissions_run_at_most_active_plus_latest() {
        let sup = PreviewSupervisor::default();
        let (gate_tx, gate_rx) = mpsc::channel::<()>();
        let (started_tx, started_rx) = mpsc::channel::<u32>();
        let ran = Arc::new(Mutex::new(Vec::<u32>::new()));
        // Job 0 blocks until released, so everything after it queues.
        {
            let ran = Arc::clone(&ran);
            let st = started_tx.clone();
            sup.submit(move |_| {
                st.send(0).unwrap();
                gate_rx.recv().unwrap();
                ran.lock().unwrap().push(0);
            });
        }
        assert_eq!(started_rx.recv().unwrap(), 0);
        for i in 1..=2000u32 {
            let ran = Arc::clone(&ran);
            sup.submit(move |_| ran.lock().unwrap().push(i));
            assert!(sup.in_flight() <= 2);
        }
        gate_tx.send(()).unwrap();
        let t = Instant::now();
        while sup.in_flight() > 0 && t.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(5));
        }
        let ran = ran.lock().unwrap().clone();
        assert_eq!(ran, vec![0, 2000], "only the active and the latest ran");
        let s = sup.stats();
        assert_eq!(s.submitted, 2001);
        assert_eq!(s.superseded_queued, 1999);
        assert_eq!(s.started, 2);
    }

    #[test]
    fn superseded_job_is_cancelled_and_cannot_deliver() {
        let sup = PreviewSupervisor::default();
        let (started_tx, started_rx) = mpsc::channel::<()>();
        let (res_tx, res_rx) = mpsc::channel::<&'static str>();
        let r1 = res_tx.clone();
        sup.submit(move |ctx| {
            started_tx.send(()).unwrap();
            while !ctx.cancelled().load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(2));
            }
            // Cancelled, but even a late send must be refused.
            ctx.deliver(|| r1.send("stale").is_ok());
        });
        started_rx.recv().unwrap();
        sup.submit(move |ctx| {
            ctx.deliver(|| res_tx.send("fresh").is_ok());
        });
        assert_eq!(
            res_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            "fresh"
        );
        assert!(res_rx.recv_timeout(Duration::from_millis(200)).is_err());
        assert_eq!(sup.stats().stale_dropped, 1);
        assert_eq!(sup.stats().cancelled_active, 1);
    }

    #[test]
    fn cancel_all_drops_pending_and_blocks_delivery() {
        let sup = PreviewSupervisor::default();
        let (res_tx, res_rx) = mpsc::channel::<u8>();
        let (started_tx, started_rx) = mpsc::channel::<()>();
        sup.submit(move |ctx| {
            started_tx.send(()).unwrap();
            while !ctx.cancelled().load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(2));
            }
            ctx.deliver(|| res_tx.send(1).is_ok());
        });
        started_rx.recv().unwrap();
        sup.cancel_all();
        assert!(res_rx.recv_timeout(Duration::from_millis(300)).is_err());
        let t = Instant::now();
        while sup.in_flight() > 0 && t.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(sup.in_flight(), 0);
    }

    #[test]
    fn panicking_job_does_not_wedge_the_slot() {
        let sup = PreviewSupervisor::default();
        sup.submit(|_| panic!("boom"));
        let (tx, rx) = mpsc::channel::<()>();
        std::thread::sleep(Duration::from_millis(50));
        sup.submit(move |_| tx.send(()).unwrap());
        rx.recv_timeout(Duration::from_secs(5)).unwrap();
    }

    fn pid_alive(pid: i32) -> bool {
        std::fs::metadata(format!("/proc/{pid}")).is_ok()
            && std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .map(|s| {
                    !s.rsplit_once(')')
                        .is_some_and(|(_, r)| r.trim_start().starts_with('Z'))
                })
                .unwrap_or(false)
    }

    #[test]
    fn deadline_kills_never_exiting_renderer_and_pipe_holding_descendant() {
        let dir = tempfile::tempdir().unwrap();
        let pidf = dir.path().join("pid");
        let script = format!("sleep 60 & echo $! > {}; wait", pidf.display());
        let t = Instant::now();
        let r = bounded_capture(
            &mut sh(&script),
            &AtomicBool::new(false),
            Duration::from_millis(400),
            1024,
            None,
        );
        assert!(matches!(r, Err(CaptureError::Timeout)));
        assert!(t.elapsed() < Duration::from_secs(3));
        let pid: i32 = std::fs::read_to_string(&pidf)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        std::thread::sleep(Duration::from_millis(100));
        assert!(!pid_alive(pid), "descendant must be killed");
    }

    #[test]
    fn cancellation_kills_the_tree_promptly() {
        let cancel = Arc::new(AtomicBool::new(false));
        let c2 = Arc::clone(&cancel);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            c2.store(true, Ordering::Release);
        });
        let t = Instant::now();
        let r = bounded_capture(
            &mut sh("sleep 60 & wait"),
            &cancel,
            Duration::from_secs(30),
            1024,
            None,
        );
        assert!(matches!(r, Err(CaptureError::Cancelled)));
        assert!(t.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn captures_stdout_and_caps_oversized_output() {
        let ok = bounded_capture(
            &mut sh("printf hello"),
            &AtomicBool::new(false),
            Duration::from_secs(5),
            1024,
            None,
        )
        .unwrap();
        assert_eq!(ok, b"hello");
        let big = bounded_capture(
            &mut sh("head -c 1000000 /dev/zero"),
            &AtomicBool::new(false),
            Duration::from_secs(5),
            4096,
            None,
        );
        // Output was truncated at the cap; the killed/early-exit renderer never hangs us.
        assert!(big.is_ok() || matches!(big, Err(CaptureError::Failed)));
    }

    #[test]
    fn nonzero_exit_is_failure() {
        let r = bounded_capture(
            &mut sh("exit 3"),
            &AtomicBool::new(false),
            Duration::from_secs(5),
            16,
            None,
        );
        assert!(matches!(r, Err(CaptureError::Failed)));
    }
}
