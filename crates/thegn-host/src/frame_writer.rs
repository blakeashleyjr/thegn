//! The terminal writer: a dedicated thread that owns every stdout write, so a
//! slow outer terminal (SSH, a throttled pty) can never block the event loop.
//!
//! Before this, the frame flush was a synchronous `write_all + flush` on the
//! loop thread — the outer terminal's drain rate backpressured input handling
//! and PTY parsing directly. Now the loop assembles a frame's bytes (wire
//! render + graphics + bell), submits them to a **bounded, 2-deep** queue, and
//! moves on; the writer thread does the blocking I/O.
//!
//! Ordering: ONE FIFO carries both frames and out-of-band writes (OSC
//! passthrough, kitty deletes, the muse marker), and every stdout byte inside
//! the event loop goes through it — a single writer on a single fd, so frames
//! and passthrough can never interleave mid-sequence.
//!
//! Backpressure/correctness rules:
//! - Frames are diffs against the loop's `front` surface: a frame, once
//!   accepted, is ALWAYS written (never latest-wins dropped) — dropping one
//!   would corrupt every later diff. The bound is enforced at submit time
//!   instead: with 2 frames in flight the loop defers composing (damage stays
//!   armed and coalesces), so the terminal's drain rate paces frame *rate*,
//!   never blocks the loop, and staleness is capped at ~2 frames.
//! - OOB bytes are small and unbounded (they must not be dropped either).
//! - A transient write error (EIO/EINTR/EAGAIN — see `frame_write`) sets a
//!   status the loop converts to a full-repaint retry, exactly like the old
//!   synchronous path; [`RETRY_MAX`](crate::frame_write::RETRY_MAX)
//!   consecutive failures escalate to Fatal (tear down).
//!
//! `sync` mode (the termwiz debug renderer, or `THEGN_SYNC_WRITER=1` as the
//! A/B lever / field kill-switch) spawns no thread: submissions write directly
//! on the caller, preserving the old synchronous behavior through the same
//! call sites.

use std::collections::VecDeque;
use std::io::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, TryLockError};
use std::time::Instant;

use crate::perf_timing::{FrameStamp, WriterMetrics};

use termwiz::terminal::TerminalWaker;

/// What the loop should do about the writer's health, taken once per wake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WriterStatus {
    /// All writes delivered (or nothing new).
    Ok,
    /// A frame/oob write hit a transient tty condition: the terminal's actual
    /// content is unknown — force a full repaint and keep going.
    Transient,
    /// Persistent or unrecoverable failure: tear the compositor down.
    Fatal(String),
}

enum Msg {
    /// A composed frame (wire bytes + trailing graphics/bell).
    Frame(Vec<u8>, Option<FrameStamp>),
    /// Order-preserving passthrough (OSC 52, kitty deletes, muse marker…).
    Oob(Vec<u8>),
}

struct Q {
    msgs: VecDeque<Msg>,
    /// How many `Msg::Frame`s are queued (the 2-deep bound).
    frames_queued: usize,
    status: WriterStatus,
    consec_errs: u32,
    shutdown: bool,
    metrics: WriterMetrics,
}

struct Inner {
    q: Mutex<Q>,
    cv: Condvar,
    metrics_deferrals: AtomicU64,
}

pub(crate) struct FrameWriter {
    inner: Arc<Inner>,
    /// `None` in sync mode (no thread; writes happen on the caller).
    handle: Option<std::thread::JoinHandle<()>>,
    sync: bool,
}

/// Max frames in flight before `frame_slot_free` reports Busy.
const FRAME_QUEUE_DEPTH: usize = 2;

impl FrameWriter {
    /// Spawn the writer thread (or a sync-mode handle when `sync` is set).
    /// `waker` pulses the loop when the status changes so an error is acted
    /// on promptly even while idle.
    pub(crate) fn spawn(waker: TerminalWaker, sync: bool) -> Self {
        let inner = Arc::new(Inner {
            q: Mutex::new(Q {
                msgs: VecDeque::new(),
                frames_queued: 0,
                status: WriterStatus::Ok,
                consec_errs: 0,
                shutdown: false,
                metrics: WriterMetrics::default(),
            }),
            cv: Condvar::new(),
            metrics_deferrals: AtomicU64::new(0),
        });
        let handle = (!sync).then(|| {
            let inner = Arc::clone(&inner);
            std::thread::Builder::new()
                .name("thegn-writer".into())
                .spawn(move || writer_main(&inner, &waker))
                .expect("spawn writer thread")
        });
        FrameWriter {
            inner,
            handle,
            sync,
        }
    }

    /// Resolve sync mode: the termwiz debug renderer writes through the
    /// BufferedTerminal (not us), and `THEGN_SYNC_WRITER=1` is the async
    /// writer's kill-switch/A-B lever.
    pub(crate) fn want_sync(use_termwiz_renderer: bool) -> bool {
        use_termwiz_renderer || std::env::var_os("THEGN_SYNC_WRITER").is_some_and(|v| v == "1")
    }

    /// True when a frame can be submitted without exceeding the bound. The
    /// loop checks this BEFORE running the wire renderer (whose SGR state
    /// must only advance for frames that are actually delivered).
    pub(crate) fn frame_slot_free(&self) -> bool {
        if self.sync {
            return true;
        }
        let q = self.inner.q.lock().unwrap_or_else(|e| e.into_inner());
        q.frames_queued < FRAME_QUEUE_DEPTH
    }

    /// Submit a composed frame. Returns false when the queue is full (the
    /// caller deferred composing, so this only races a concurrent drain —
    /// never drops). Sync mode writes inline.
    pub(crate) fn submit_frame(&self, bytes: Vec<u8>, stamp: Option<FrameStamp>) -> bool {
        if self.sync {
            self.write_inline(&bytes, stamp);
            return true;
        }
        let mut q = self.inner.q.lock().unwrap_or_else(|e| e.into_inner());
        if q.frames_queued >= FRAME_QUEUE_DEPTH {
            return false;
        }
        q.frames_queued += 1;
        q.msgs.push_back(Msg::Frame(bytes, stamp));
        drop(q);
        self.inner.cv.notify_one();
        true
    }

    /// Submit order-preserving passthrough bytes (never dropped, unbounded —
    /// these are small: OSC sequences, kitty deletes, BEL, markers).
    pub(crate) fn submit_oob(&self, bytes: Vec<u8>) {
        if bytes.is_empty() {
            return;
        }
        if self.sync {
            self.write_inline(&bytes, None);
            return;
        }
        let mut q = self.inner.q.lock().unwrap_or_else(|e| e.into_inner());
        q.msgs.push_back(Msg::Oob(bytes));
        drop(q);
        self.inner.cv.notify_one();
    }

    /// Take the current status (resetting Transient back to Ok; Fatal sticks).
    pub(crate) fn take_status(&self) -> WriterStatus {
        let mut q = self.inner.q.lock().unwrap_or_else(|e| e.into_inner());
        match q.status.clone() {
            WriterStatus::Transient => {
                q.status = WriterStatus::Ok;
                WriterStatus::Transient
            }
            other => other,
        }
    }

    /// Sync-mode write on the caller thread, with the same classification.
    fn write_inline(&self, bytes: &[u8], stamp: Option<FrameStamp>) {
        let started = stamp.map(|_| Instant::now());
        let result = write_and_flush(bytes);
        let finished = started.map(|_| Instant::now());
        let mut q = self.inner.q.lock().unwrap_or_else(|e| e.into_inner());
        if let (Some(stamp), Some(started), Some(finished)) = (stamp, started, finished) {
            q.metrics.record(stamp, started, finished, result.is_ok());
        }
        apply_write_result(&mut q, result);
    }

    /// Never wait behind a producer for diagnostics. The fixed-size ledger is
    /// retained if contended and included in a later existing rollup.
    pub(crate) fn take_metrics(&self) -> Option<WriterMetrics> {
        let mut q = match self.inner.q.try_lock() {
            Ok(q) => q,
            Err(TryLockError::Poisoned(e)) => e.into_inner(),
            Err(TryLockError::WouldBlock) => {
                self.inner.metrics_deferrals.fetch_add(1, Ordering::Relaxed);
                return None;
            }
        };
        let mut metrics = std::mem::take(&mut q.metrics);
        metrics.deferred_rollups = self.inner.metrics_deferrals.swap(0, Ordering::Relaxed);
        Some(metrics)
    }

    /// The termwiz debug renderer writes synchronously outside this writer.
    pub(crate) fn record_external_frame(
        &self,
        stamp: Option<FrameStamp>,
        started: Instant,
        finished: Instant,
        success: bool,
    ) {
        if let Some(stamp) = stamp {
            let mut q = self.inner.q.lock().unwrap_or_else(|e| e.into_inner());
            q.metrics.record(stamp, started, finished, success);
        }
    }
}

impl Drop for FrameWriter {
    fn drop(&mut self) {
        // Drain-then-join: queued frames (incl. the last one on screen) land
        // before the caller restores the terminal, so the alt-screen exit
        // sequence is last-out.
        {
            let mut q = self.inner.q.lock().unwrap_or_else(|e| e.into_inner());
            q.shutdown = true;
        }
        self.inner.cv.notify_one();
        if let Some(h) = self.handle.take() {
            let _ = h.join(); // best-effort: thread join: a panicked helper loses its output, not the caller
        }
    }
}

fn writer_main(inner: &Inner, waker: &TerminalWaker) {
    loop {
        let msg = {
            let mut q = inner.q.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if let Some(m) = q.msgs.pop_front() {
                    if matches!(m, Msg::Frame(..)) {
                        q.frames_queued = q.frames_queued.saturating_sub(1);
                    }
                    break Some(m);
                }
                if q.shutdown {
                    break None;
                }
                q = inner.cv.wait(q).unwrap_or_else(|e| e.into_inner());
            }
        };
        let Some(msg) = msg else { break };
        let errored = write_message(inner, msg, write_and_flush);
        if errored {
            // The loop acts on the status (full repaint / teardown) — wake it.
            let _ = waker.wake(); // best-effort: waker pulse: an input nudge must never fail the calling path
        }
    }
}

/// One message, with the sink injected for isolated boundary tests. Queue locks
/// are never held during sink I/O; only fixed-size status/metric updates follow.
fn write_message(
    inner: &Inner,
    msg: Msg,
    write: impl FnOnce(&[u8]) -> std::io::Result<()>,
) -> bool {
    let bytes = match &msg {
        Msg::Frame(b, _) | Msg::Oob(b) => b,
    };
    // After a Fatal, drop writes (the loop is tearing down); keep draining
    // so shutdown never deadlocks.
    let fatal = {
        let q = inner.q.lock().unwrap_or_else(|e| e.into_inner());
        matches!(q.status, WriterStatus::Fatal(_))
    };
    if fatal {
        return false;
    }
    let stamp = match &msg {
        Msg::Frame(_, stamp) => *stamp,
        Msg::Oob(_) => None,
    };
    let started = stamp.map(|_| Instant::now());
    let result = write(bytes);
    let finished = started.map(|_| Instant::now());
    let errored = result.is_err();
    {
        let mut q = inner.q.lock().unwrap_or_else(|e| e.into_inner());
        if let (Some(stamp), Some(started), Some(finished)) = (stamp, started, finished) {
            q.metrics.record(stamp, started, finished, !errored);
        }
        apply_write_result(&mut q, result);
    }
    errored
}

fn write_and_flush(bytes: &[u8]) -> std::io::Result<()> {
    let mut out = std::io::stdout();
    out.write_all(bytes)?;
    out.flush()
}

/// Fold one write's outcome into the shared status: success resets the
/// consecutive-failure counter; a transient error becomes `Transient` until
/// [`crate::frame_write::RETRY_MAX`] in a row escalate to `Fatal`; a
/// non-transient error is `Fatal` immediately. Fatal is sticky.
fn apply_write_result(q: &mut Q, result: std::io::Result<()>) {
    match result {
        Ok(()) => {
            q.consec_errs = 0;
            // Leave a pending Transient for the loop to observe (it must
            // still full-repaint the frame that failed); Ok only means THIS
            // write landed.
        }
        Err(e) => {
            if matches!(q.status, WriterStatus::Fatal(_)) {
                return;
            }
            let transient = crate::frame_write::is_transient_io(&e);
            q.consec_errs += 1;
            if !transient || q.consec_errs > crate::frame_write::RETRY_MAX {
                q.status = WriterStatus::Fatal(format!(
                    "terminal write failed ({e}) after {} attempt(s)",
                    q.consec_errs
                ));
            } else {
                q.status = WriterStatus::Transient;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q() -> Q {
        Q {
            msgs: VecDeque::new(),
            frames_queued: 0,
            status: WriterStatus::Ok,
            consec_errs: 0,
            shutdown: false,
            metrics: WriterMetrics::default(),
        }
    }

    fn test_writer() -> FrameWriter {
        FrameWriter {
            inner: Arc::new(Inner {
                q: Mutex::new(q()),
                cv: Condvar::new(),
                metrics_deferrals: AtomicU64::new(0),
            }),
            handle: None,
            sync: false,
        }
    }

    #[test]
    fn completion_is_recorded_after_sink_returns_and_oob_is_excluded() {
        let writer = test_writer();
        let stamp = FrameStamp {
            queued_at: Instant::now(),
            input_at: Some(Instant::now()),
        };
        assert!(!write_message(
            &writer.inner,
            Msg::Frame(vec![1], Some(stamp)),
            |bytes| {
                assert_eq!(bytes, [1]);
                // A blocked sink must not hold the UI's queue/metrics mutex.
                assert_eq!(writer.take_metrics().unwrap().completion_us.count(), 0);
                Ok(())
            }
        ));
        assert_eq!(writer.take_metrics().unwrap().completion_us.count(), 1);
        assert!(!write_message(&writer.inner, Msg::Oob(vec![2]), |_| Ok(())));
        let metrics = writer.take_metrics().unwrap();
        assert_eq!(metrics.completion_us.count(), 0);
        assert_eq!(metrics.failed_frames, 0);
    }

    #[test]
    fn failed_frame_never_becomes_a_successful_completion() {
        let writer = test_writer();
        let stamp = FrameStamp {
            queued_at: Instant::now(),
            input_at: None,
        };
        assert!(write_message(
            &writer.inner,
            Msg::Frame(vec![], Some(stamp)),
            |_| { Err(std::io::ErrorKind::Interrupted.into()) }
        ));
        let metrics = writer.take_metrics().unwrap();
        assert_eq!(metrics.failed_frames, 1);
        assert_eq!(metrics.completion_us.count(), 0);
        assert_eq!(metrics.input_completion_us.count(), 0);
    }

    #[test]
    fn metrics_drain_never_waits_on_queue_contention_and_keeps_samples() {
        let writer = test_writer();
        let mut guard = writer.inner.q.lock().unwrap();
        guard.metrics.failed_frames = 7;
        assert!(writer.take_metrics().is_none());
        drop(guard);
        let metrics = writer.take_metrics().unwrap();
        assert_eq!(metrics.failed_frames, 7);
        assert_eq!(metrics.deferred_rollups, 1);
        assert_eq!(writer.take_metrics().unwrap().failed_frames, 0);
    }

    #[test]
    fn timing_metadata_keeps_the_existing_frame_queue_bound() {
        let writer = test_writer();
        assert!(writer.submit_frame(vec![1], None));
        assert!(writer.submit_frame(vec![2], None));
        assert!(!writer.frame_slot_free());
        assert!(!writer.submit_frame(vec![3], None));
        assert_eq!(writer.inner.q.lock().unwrap().msgs.len(), FRAME_QUEUE_DEPTH);
    }

    #[cfg(unix)] // EIO-as-transient is unix errno classification
    #[test]
    fn transient_errors_escalate_to_fatal_after_retry_max() {
        let mut state = q();
        for i in 1..=crate::frame_write::RETRY_MAX {
            apply_write_result(
                &mut state,
                Err(std::io::Error::from_raw_os_error(libc::EIO)),
            );
            assert_eq!(
                state.status,
                WriterStatus::Transient,
                "attempt {i} should still be transient"
            );
        }
        apply_write_result(
            &mut state,
            Err(std::io::Error::from_raw_os_error(libc::EIO)),
        );
        assert!(matches!(state.status, WriterStatus::Fatal(_)));
    }

    #[cfg(unix)] // EIO-as-transient is unix errno classification
    #[test]
    fn success_resets_the_consecutive_counter() {
        let mut state = q();
        apply_write_result(
            &mut state,
            Err(std::io::Error::from_raw_os_error(libc::EIO)),
        );
        assert_eq!(state.consec_errs, 1);
        apply_write_result(&mut state, Ok(()));
        assert_eq!(state.consec_errs, 0);
        // The pending Transient stays for the loop to observe.
        assert_eq!(state.status, WriterStatus::Transient);
    }

    #[test]
    fn non_transient_error_is_immediately_fatal_and_sticky() {
        let mut state = q();
        apply_write_result(
            &mut state,
            Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe)),
        );
        assert!(matches!(state.status, WriterStatus::Fatal(_)));
        // Sticky: later successes/errors don't downgrade it.
        apply_write_result(&mut state, Ok(()));
        assert!(matches!(state.status, WriterStatus::Fatal(_)));
    }
}
