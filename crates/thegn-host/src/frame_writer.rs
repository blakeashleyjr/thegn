//! The terminal writer: a dedicated thread that owns every stdout write, so a
//! slow outer terminal (SSH, a throttled pty) can never block the event loop.
//!
//! Before this, the frame flush was a synchronous `write_all + flush` on the
//! loop thread — the outer terminal's drain rate backpressured input handling
//! and PTY parsing directly. Now the loop assembles a frame's bytes (wire
//! render + graphics + bell), submits them to a **bounded, 2-deep** queue, and
//! moves on; the writer thread does the blocking I/O.
//!
//! Ordering: ONE FIFO carries worker-managed frames and out-of-band writes
//! (OSC passthrough, kitty deletes, the muse marker), so those writes cannot
//! interleave mid-sequence. The termwiz debug renderer still writes directly
//! from the render loop and needs a separate migration to this FIFO.
//!
//! Backpressure/correctness rules:
//! - Frames are diffs against the loop's `front` surface: a frame, once
//!   accepted, is ALWAYS written (never latest-wins dropped) — dropping one
//!   would corrupt every later diff. The bound is enforced at submit time
//!   instead: with 2 frames in flight the loop defers composing (damage stays
//!   armed and coalesces), so the terminal's drain rate paces frame *rate*,
//!   never blocks the loop, and staleness is capped at ~2 frames.
//! - OOB admission is bounded to 128 messages and 4 MiB total, including the
//!   message currently being written. This admits one maximum-size kitty APC
//!   or preview image on an empty queue. `submit_oob` warns and drops on queue
//!   pressure; `try_submit_oob` returns refused bytes to retry-capable callers.
//!   Host clipboard copies use the typed clipboard queue and share its four-set
//!   count, with the same 4 MiB byte budget as OOB.
//! - A transient write error (EIO/EINTR/EAGAIN — see `frame_write`) sets a
//!   status the loop converts to a full-repaint retry, exactly like the old
//!   synchronous path; [`RETRY_MAX`](crate::frame_write::RETRY_MAX)
//!   consecutive failures escalate to Fatal (tear down).
//!
//! `THEGN_SYNC_WRITER=1` keeps FIFO writes on the owned worker but makes each
//! admission wait on the caller until completion or the bounded deadline. A
//! timeout sets Fatal and abandons queued bytes. `want_sync` also enables this
//! mode for the termwiz debug renderer, so its FIFO OOB and clipboard writes
//! can block the render loop for up to three seconds; its frames still write
//! outside this worker in `run.rs`.

use std::collections::VecDeque;
use std::io::Write as _;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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

/// A clipboard delivery that did not commit to the terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClipboardOutcome {
    /// The write failed and was dropped; some bytes may have reached the terminal.
    Uncertain,
    /// The set was abandoned before any write attempt.
    NotWritten,
}

/// Fixed, payload-free status text for a clipboard delivery outcome.
pub(crate) fn clipboard_outcome_status(outcome: ClipboardOutcome) -> &'static str {
    match outcome {
        ClipboardOutcome::Uncertain => {
            "Copy may not have reached the terminal clipboard (terminal write failed)"
        }
        ClipboardOutcome::NotWritten => {
            "Copy was not sent to the terminal clipboard (terminal writer stopped)"
        }
    }
}

enum Msg {
    /// A composed frame (wire bytes + trailing graphics/bell).
    Frame(Vec<u8>, Option<FrameStamp>, u64),
    /// Order-preserving passthrough (kitty deletes, muse marker…).
    Oob(Vec<u8>),
    Clipboard(Vec<u8>, u64),
}

struct Q {
    msgs: VecDeque<Msg>,
    /// Failed OOB/clipboard writes remain charged until replay or abandonment.
    deferred: VecDeque<DeferredWrite>,
    /// How many `Msg::Frame`s are queued (the 2-deep bound).
    frames_queued: usize,
    clipboard_queued: usize,
    clipboard_bytes: usize,
    clipboard_outcome: Option<(u64, ClipboardOutcome)>,
    clipboard_committed: u64,
    /// Includes queued and currently-writing OOB messages.
    oob_in_flight: usize,
    oob_bytes: usize,
    oob_retry_need: Option<usize>,
    status: WriterStatus,
    consec_errs: u32,
    parser_reset_pending: bool,
    shutdown: bool,
    worker_done: bool,
    submitted_seq: u64,
    completed_seq: u64,
    metrics: WriterMetrics,
}

struct DeferredWrite {
    msg: Msg,
    defer_after: u64,
}

struct Inner {
    q: Mutex<Q>,
    /// Termwiz paints frames directly, so its OOB writes have no worker frame
    /// boundary to wait for before being dropped after failure.
    worker_frames: bool,
    cv: Condvar,
    done_cv: Condvar,
    metrics_deferrals: AtomicU64,
    clipboard_retry: AtomicBool,
    oob_retry: AtomicBool,
    oob_pressure_logged: AtomicBool,
}

pub(crate) struct FrameWriter {
    inner: Arc<Inner>,
    /// The owned writer worker, including in sync mode.
    handle: Option<std::thread::JoinHandle<()>>,
    sync: bool,
}

/// Max frames in flight before `frame_slot_free` reports Busy.
const FRAME_QUEUE_DEPTH: usize = 2;
// CAN aborts a partial control sequence; ST closes an OSC/APC string. Both
// are harmless when the terminal parser is already in ground state.
const PARSER_RESET: &[u8] = b"\x18\x1b\\";
const OOB_QUEUE_COUNT: usize = 128;
pub(crate) const OOB_QUEUE_BYTES: usize = 4 * 1024 * 1024;
const WRITER_SHUTDOWN_BOUND: std::time::Duration = if cfg!(test) {
    std::time::Duration::from_millis(30)
} else {
    std::time::Duration::from_millis(500)
};
/// Maximum caller wait for one synchronous-mode write. Production uses three
/// seconds to allow a slow terminal to drain while keeping the UI recoverable.
const SYNC_WRITE_BOUND: std::time::Duration = if cfg!(test) {
    std::time::Duration::from_millis(30)
} else {
    std::time::Duration::from_secs(3)
};

impl FrameWriter {
    /// Spawn the owned writer thread.
    /// `waker` pulses the loop when the status changes so an error is acted
    /// on promptly even while idle.
    pub(crate) fn spawn(waker: TerminalWaker, sync: bool, worker_frames: bool) -> Self {
        Self::spawn_with(
            sync,
            worker_frames,
            move |reason| wake_loop(&waker, reason),
            write_and_flush,
        )
    }

    fn spawn_with(
        sync: bool,
        worker_frames: bool,
        wake: impl Fn(&'static str) + Send + 'static,
        sink: impl FnMut(&[u8]) -> std::io::Result<()> + Send + 'static,
    ) -> Self {
        let inner = Arc::new(Inner {
            q: Mutex::new(Q {
                msgs: VecDeque::new(),
                deferred: VecDeque::new(),
                frames_queued: 0,
                clipboard_queued: 0,
                clipboard_bytes: 0,
                clipboard_outcome: None,
                clipboard_committed: 0,
                oob_in_flight: 0,
                oob_bytes: 0,
                oob_retry_need: None,
                status: WriterStatus::Ok,
                consec_errs: 0,
                parser_reset_pending: false,
                shutdown: false,
                worker_done: false,
                submitted_seq: 0,
                completed_seq: 0,
                metrics: WriterMetrics::default(),
            }),
            worker_frames,
            cv: Condvar::new(),
            done_cv: Condvar::new(),
            metrics_deferrals: AtomicU64::new(0),
            clipboard_retry: AtomicBool::new(false),
            oob_retry: AtomicBool::new(false),
            oob_pressure_logged: AtomicBool::new(false),
        });
        let worker_inner = Arc::clone(&inner);
        let handle = Some(
            std::thread::Builder::new()
                .name("thegn-writer".into())
                .spawn(move || writer_main(&worker_inner, wake, sink))
                .expect("spawn writer thread"),
        );
        FrameWriter {
            inner,
            handle,
            sync,
        }
    }

    /// Resolve sync mode: the termwiz debug renderer writes through the
    /// BufferedTerminal (not us), and `THEGN_SYNC_WRITER=1` makes worker-backed
    /// writes ordered and caller-blocking up to `SYNC_WRITE_BOUND`; timeout is
    /// Fatal and drops queued messages. Termwiz frames themselves remain on
    /// the render loop until the run-loop lane moves them.
    pub(crate) fn want_sync(use_termwiz_renderer: bool) -> bool {
        use_termwiz_renderer || std::env::var_os("THEGN_SYNC_WRITER").is_some_and(|v| v == "1")
    }

    /// True when a frame can be submitted without exceeding the bound. The
    /// loop checks this BEFORE running the wire renderer (whose SGR state
    /// must only advance for frames that are actually delivered).
    pub(crate) fn frame_slot_free(&self) -> bool {
        let q = self.inner.q.lock().unwrap_or_else(|e| e.into_inner());
        !q.shutdown
            && !matches!(q.status, WriterStatus::Fatal(_))
            && q.frames_queued < FRAME_QUEUE_DEPTH
    }

    /// Submit a composed frame. Returns false when the queue is full (the
    /// caller deferred composing, so this only races a concurrent drain —
    /// never drops). Sync mode waits for worker completion up to its deadline.
    pub(crate) fn submit_frame(&self, bytes: Vec<u8>, stamp: Option<FrameStamp>) -> bool {
        let mut q = self.inner.q.lock().unwrap_or_else(|e| e.into_inner());
        if q.shutdown
            || matches!(q.status, WriterStatus::Fatal(_))
            || q.frames_queued >= FRAME_QUEUE_DEPTH
        {
            return false;
        }
        q.submitted_seq = q.submitted_seq.saturating_add(1);
        let target = q.submitted_seq;
        q.frames_queued += 1;
        q.msgs.push_back(Msg::Frame(bytes, stamp, target));
        drop(q);
        self.inner.cv.notify_one();
        if self.sync {
            self.wait_for_sync_write(target);
        }
        true
    }

    /// Submit order-preserving passthrough bytes. Queue pressure drops this
    /// message with a warning; it never makes the compositor fatal.
    #[cfg(test)]
    pub(crate) fn submit_oob(&self, bytes: Vec<u8>) {
        if let Err(bytes) = self.admit_oob(bytes, false) {
            if self.inner.oob_pressure_logged.swap(true, Ordering::SeqCst) {
                tracing::debug!(
                    bytes = bytes.len(),
                    count_limit = OOB_QUEUE_COUNT,
                    byte_limit = OOB_QUEUE_BYTES,
                    "terminal OOB admission refused during existing pressure episode; dropping message"
                );
            } else {
                tracing::warn!(
                    bytes = bytes.len(),
                    count_limit = OOB_QUEUE_COUNT,
                    byte_limit = OOB_QUEUE_BYTES,
                    "terminal OOB admission refused; dropping message"
                );
            }
        }
    }

    /// Typed, lossless admission for producers able to retain and retry bytes.
    /// Refusals record the smallest needed message size so capacity wakes only
    /// when the current queue can admit at least one retained update.
    pub(crate) fn try_submit_oob(&self, bytes: Vec<u8>) -> Result<(), Vec<u8>> {
        self.admit_oob(bytes, true)
    }

    fn admit_oob(&self, bytes: Vec<u8>, retained: bool) -> Result<(), Vec<u8>> {
        if bytes.is_empty() {
            return Ok(());
        }
        let mut q = self.inner.q.lock().unwrap_or_else(|e| e.into_inner());
        if q.shutdown
            || matches!(q.status, WriterStatus::Fatal(_))
            || q.oob_in_flight >= OOB_QUEUE_COUNT
            || bytes.len() > OOB_QUEUE_BYTES.saturating_sub(q.oob_bytes)
        {
            if retained {
                self.inner.oob_retry.store(true, Ordering::SeqCst);
                q.oob_retry_need = Some(
                    q.oob_retry_need
                        .map_or(bytes.len(), |need| need.min(bytes.len())),
                );
            }
            return Err(bytes);
        }
        self.inner
            .oob_pressure_logged
            .store(false, Ordering::SeqCst);
        q.submitted_seq = q.submitted_seq.saturating_add(1);
        let target = q.submitted_seq;
        q.oob_in_flight += 1;
        q.oob_bytes += bytes.len();
        q.msgs.push_back(Msg::Oob(bytes));
        drop(q);
        self.inner.cv.notify_one();
        if self.sync {
            self.wait_for_sync_write(target);
        }
        Ok(())
    }

    /// Only validated pane clipboard sets enter this bounded queue. Four
    /// admitted sets, including in-flight and deferred writes, bound
    /// writer-owned clipboard data; refused ownership stays with the pane for
    /// latest-wins retry.
    pub(crate) fn try_submit_clipboard(
        &self,
        bytes: Vec<u8>,
        retry_wake: impl FnOnce(),
    ) -> Result<(), Vec<u8>> {
        if bytes.len() > crate::queries::clipboard::MAX_WIRE {
            return Err(bytes);
        }
        self.inner.clipboard_retry.store(true, Ordering::SeqCst);
        let mut q = match self.inner.q.try_lock() {
            Ok(q) => q,
            Err(TryLockError::Poisoned(e)) => e.into_inner(),
            Err(TryLockError::WouldBlock) => {
                // A non-writer lock holder with an empty queue cannot provide
                // a capacity wake. Pulse now so refused work never strands.
                retry_wake();
                return Err(bytes);
            }
        };
        let target = admit_clipboard(&mut q, bytes, 4 * crate::queries::clipboard::MAX_WIRE)?;
        drop(q);
        self.inner.cv.notify_one();
        if self.sync {
            self.wait_for_sync_write(target);
        }
        Ok(())
    }

    /// Admit thegn's own terminal clipboard copies. Host copies share the
    /// four-set clipboard count with pane sets and have a 4 MiB byte budget;
    /// pane sets retain their smaller `4 * MAX_WIRE` budget against these same
    /// counters, so a large host copy in flight makes later pane sets wait for
    /// capacity and use their existing latest-wins retry.
    pub(crate) fn try_submit_host_clipboard(&self, bytes: Vec<u8>) -> Result<(), Vec<u8>> {
        if bytes.is_empty() {
            return Ok(());
        }
        if bytes.len() > OOB_QUEUE_BYTES {
            return Err(bytes);
        }
        self.inner.clipboard_retry.store(true, Ordering::SeqCst);
        let mut q = self.inner.q.lock().unwrap_or_else(|e| e.into_inner());
        let target = admit_clipboard(&mut q, bytes, OOB_QUEUE_BYTES)?;
        drop(q);
        self.inner.cv.notify_one();
        if self.sync {
            self.wait_for_sync_write(target);
        }
        Ok(())
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

    /// Take the newest unresolved clipboard delivery outcome once.
    pub(crate) fn take_clipboard_outcome(&self) -> Option<ClipboardOutcome> {
        self.inner
            .q
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clipboard_outcome
            .take()
            .map(|(_, outcome)| outcome)
    }

    fn wait_for_sync_write(&self, target: u64) {
        let deadline = Instant::now() + SYNC_WRITE_BOUND;
        let mut q = self.inner.q.lock().unwrap_or_else(|e| e.into_inner());
        while q.completed_seq < target && !matches!(q.status, WriterStatus::Fatal(_)) {
            let now = Instant::now();
            if now >= deadline {
                q.status = WriterStatus::Fatal("synchronous terminal write deadline".into());
                abandon_queued_messages(&mut q);
                drop(q);
                self.inner.done_cv.notify_all();
                return;
            }
            let (next, _) = self
                .inner
                .done_cv
                .wait_timeout(q, deadline.saturating_duration_since(now))
                .unwrap_or_else(|e| e.into_inner());
            q = next;
        }
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

fn admit_clipboard(q: &mut Q, bytes: Vec<u8>, byte_budget: usize) -> Result<u64, Vec<u8>> {
    if q.shutdown
        || matches!(q.status, WriterStatus::Fatal(_))
        || q.clipboard_queued >= 4
        || q.clipboard_bytes + bytes.len() > byte_budget
    {
        return Err(bytes);
    }
    q.clipboard_queued += 1;
    q.clipboard_bytes += bytes.len();
    q.submitted_seq = q.submitted_seq.saturating_add(1);
    let target = q.submitted_seq;
    q.msgs.push_back(Msg::Clipboard(bytes, target));
    Ok(target)
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
            let deadline = std::time::Instant::now() + WRITER_SHUTDOWN_BOUND;
            let mut q = self.inner.q.lock().unwrap_or_else(|e| e.into_inner());
            while !q.worker_done {
                let now = std::time::Instant::now();
                if now >= deadline {
                    tracing::warn!(
                        "terminal writer exceeded shutdown bound; detaching degraded worker"
                    );
                    abandon_queued_messages(&mut q);
                    drop(h);
                    return;
                }
                let (next, timeout) = self
                    .inner
                    .cv
                    .wait_timeout(q, deadline.saturating_duration_since(now))
                    .unwrap_or_else(|e| e.into_inner());
                q = next;
                if timeout.timed_out() && !q.worker_done {
                    tracing::warn!(
                        "terminal writer exceeded shutdown bound; detaching degraded worker"
                    );
                    abandon_queued_messages(&mut q);
                    drop(h);
                    return;
                }
            }
            drop(q);
            if h.join().is_err() {
                tracing::warn!("terminal writer thread panicked before shutdown");
            }
        }
    }
}

fn abandon_queued_messages(q: &mut Q) {
    if !matches!(q.status, WriterStatus::Fatal(_)) {
        q.status = WriterStatus::Fatal("terminal writer shutdown deadline".into());
    }
    let queued_clipboard_ids: Vec<u64> = q
        .msgs
        .iter()
        .filter_map(|msg| match msg {
            Msg::Clipboard(_, id) => Some(*id),
            _ => None,
        })
        .collect();
    for id in queued_clipboard_ids {
        note_clipboard(q, id, Some(ClipboardOutcome::NotWritten));
    }
    let deferred_clipboard_ids: Vec<u64> = q
        .deferred
        .iter()
        .filter_map(|deferred| match &deferred.msg {
            Msg::Clipboard(_, id) => Some(*id),
            _ => None,
        })
        .collect();
    for id in deferred_clipboard_ids {
        note_clipboard(q, id, Some(ClipboardOutcome::Uncertain));
    }
    let queued_oob_count = q
        .msgs
        .iter()
        .filter(|msg| matches!(msg, Msg::Oob(_)))
        .count();
    let queued_oob_bytes = q.msgs.iter().fold(0usize, |total, msg| match msg {
        Msg::Oob(bytes) => total.saturating_add(bytes.len()),
        Msg::Frame(..) | Msg::Clipboard(..) => total,
    });
    let deferred_oob_count = q
        .deferred
        .iter()
        .filter(|deferred| matches!(&deferred.msg, Msg::Oob(_)))
        .count();
    let deferred_oob_bytes =
        q.deferred
            .iter()
            .fold(0usize, |total, deferred| match &deferred.msg {
                Msg::Oob(bytes) => total.saturating_add(bytes.len()),
                Msg::Frame(..) | Msg::Clipboard(..) => total,
            });
    q.msgs.clear();
    q.deferred.clear();
    q.frames_queued = 0;
    // Keep accounting for an in-flight write; write_message releases it when
    // that write returns. Everything still in msgs has been abandoned.
    q.oob_in_flight = q
        .oob_in_flight
        .saturating_sub(queued_oob_count + deferred_oob_count);
    q.oob_bytes = q
        .oob_bytes
        .saturating_sub(queued_oob_bytes.saturating_add(deferred_oob_bytes));
    q.clipboard_queued = 0;
    q.clipboard_bytes = 0;
}

fn abandon_deferred_writes(q: &mut Q) {
    let clipboard_ids: Vec<u64> = q
        .deferred
        .iter()
        .filter_map(|deferred| match &deferred.msg {
            Msg::Clipboard(_, id) => Some(*id),
            _ => None,
        })
        .collect();
    for id in clipboard_ids {
        note_clipboard(q, id, Some(ClipboardOutcome::Uncertain));
    }
    let (oob_count, oob_bytes, clipboard_count, clipboard_bytes) = q.deferred.iter().fold(
        (0usize, 0usize, 0usize, 0usize),
        |(oob_count, oob_bytes, clipboard_count, clipboard_bytes), deferred| match &deferred.msg {
            Msg::Oob(bytes) => (
                oob_count + 1,
                oob_bytes.saturating_add(bytes.len()),
                clipboard_count,
                clipboard_bytes,
            ),
            Msg::Clipboard(bytes, _) => (
                oob_count,
                oob_bytes,
                clipboard_count + 1,
                clipboard_bytes.saturating_add(bytes.len()),
            ),
            Msg::Frame(..) => (oob_count, oob_bytes, clipboard_count, clipboard_bytes),
        },
    );
    q.deferred.clear();
    q.oob_in_flight = q.oob_in_flight.saturating_sub(oob_count);
    q.oob_bytes = q.oob_bytes.saturating_sub(oob_bytes);
    q.clipboard_queued = q.clipboard_queued.saturating_sub(clipboard_count);
    q.clipboard_bytes = q.clipboard_bytes.saturating_sub(clipboard_bytes);
}

fn pop_message(q: &mut Q) -> Option<Msg> {
    let message = q.msgs.pop_front()?;
    if matches!(message, Msg::Frame(..)) {
        q.frames_queued = q.frames_queued.saturating_sub(1);
    }
    Some(message)
}

fn note_clipboard(q: &mut Q, id: u64, outcome: Option<ClipboardOutcome>) {
    match outcome {
        None => {
            q.clipboard_committed = q.clipboard_committed.max(id);
            if q.clipboard_outcome
                .is_some_and(|(pending_id, _)| pending_id <= id)
            {
                q.clipboard_outcome = None;
            }
        }
        Some(outcome) if id > q.clipboard_committed => {
            let replace = match q.clipboard_outcome {
                Some((pending_id, _)) if pending_id > id => false,
                Some((pending_id, ClipboardOutcome::Uncertain)) if pending_id == id => false,
                Some((pending_id, _)) if pending_id == id => true,
                _ => true,
            };
            if replace {
                q.clipboard_outcome = Some((id, outcome));
            }
        }
        Some(_) => {}
    }
}

fn writer_main(
    inner: &Inner,
    wake: impl Fn(&'static str),
    mut sink: impl FnMut(&[u8]) -> std::io::Result<()>,
) {
    loop {
        let msg = {
            let mut q = inner.q.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if let Some(m) = pop_message(&mut q) {
                    break Some(m);
                }
                if q.shutdown {
                    break None;
                }
                q = inner.cv.wait(q).unwrap_or_else(|e| e.into_inner());
            }
        };
        let Some(msg) = msg else { break };
        let (errored, released_oob, released_clipboard) = write_message(inner, msg, &mut sink);
        if released_clipboard && inner.clipboard_retry.swap(false, Ordering::SeqCst) {
            wake("clipboard capacity released");
        }
        if errored || oob_capacity_wake_requested(inner, released_oob) {
            wake("writer state changed");
        }
    }
    let mut q = inner.q.lock().unwrap_or_else(|e| e.into_inner());
    abandon_deferred_writes(&mut q);
    q.worker_done = true;
    inner.cv.notify_all();
    inner.done_cv.notify_all();
}

fn wake_loop(waker: &TerminalWaker, reason: &'static str) {
    if let Err(error) = waker.wake() {
        tracing::trace!(?error, reason, "terminal writer wake failed");
    }
}

fn oob_capacity_wake_requested(inner: &Inner, released_oob: bool) -> bool {
    if !released_oob {
        return false;
    }
    let mut q = inner.q.lock().unwrap_or_else(|e| e.into_inner());
    if !inner.oob_retry.load(Ordering::SeqCst)
        || q.oob_in_flight >= OOB_QUEUE_COUNT
        || q.oob_retry_need
            .is_none_or(|need| need > OOB_QUEUE_BYTES.saturating_sub(q.oob_bytes))
    {
        return false;
    }
    inner.oob_retry.store(false, Ordering::SeqCst);
    q.oob_retry_need = None;
    true
}

/// One message, with the sink injected for isolated boundary tests. Queue locks
/// are never held during sink I/O; only fixed-size status/metric updates follow.
fn write_message(
    inner: &Inner,
    msg: Msg,
    mut write: impl FnMut(&[u8]) -> std::io::Result<()>,
) -> (bool, bool, bool) {
    let bytes = match &msg {
        Msg::Frame(b, ..) | Msg::Oob(b) | Msg::Clipboard(b, _) => b,
    };
    // After a Fatal, drop writes (the loop is tearing down); keep draining
    // so shutdown never deadlocks.
    let fatal = {
        let mut q = inner.q.lock().unwrap_or_else(|e| e.into_inner());
        let fatal = matches!(q.status, WriterStatus::Fatal(_));
        let released = if fatal {
            if let Msg::Clipboard(_, id) = &msg {
                note_clipboard(&mut q, *id, Some(ClipboardOutcome::NotWritten));
            }
            release_message_capacity(&mut q, &msg);
            matches!(&msg, Msg::Oob(_))
        } else {
            false
        };
        (fatal, released)
    };
    if fatal.0 {
        let mut q = inner.q.lock().unwrap_or_else(|e| e.into_inner());
        q.completed_seq = q.completed_seq.saturating_add(1);
        drop(q);
        inner.done_cv.notify_all();
        return (false, fatal.1, matches!(&msg, Msg::Clipboard(..)));
    }
    let stamp = match &msg {
        Msg::Frame(_, stamp, _) => *stamp,
        Msg::Oob(_) | Msg::Clipboard(..) => None,
    };
    let started = stamp.map(|_| Instant::now());
    let reset_pending = inner
        .q
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .parser_reset_pending;
    let result = if reset_pending {
        match write(PARSER_RESET) {
            Ok(()) => {
                let mut q = inner.q.lock().unwrap_or_else(|e| e.into_inner());
                q.parser_reset_pending = false;
                let fatal = matches!(q.status, WriterStatus::Fatal(_));
                drop(q);
                if fatal { Ok(()) } else { write(bytes) }
            }
            Err(error) => Err(error),
        }
    } else {
        write(bytes)
    };
    let finished = started.map(|_| Instant::now());
    let errored = result.is_err();
    let mut released_oob = false;
    let mut released_clipboard = false;
    let mut replay = VecDeque::new();
    let successful_frame_seq = match &msg {
        Msg::Frame(_, _, seq) if result.is_ok() => Some(*seq),
        _ => None,
    };
    {
        let mut q = inner.q.lock().unwrap_or_else(|e| e.into_inner());
        if result.is_err()
            && inner.worker_frames
            && matches!(&msg, Msg::Oob(_) | Msg::Clipboard(..))
        {
            let defer_after = q.submitted_seq;
            q.deferred.push_back(DeferredWrite { msg, defer_after });
        } else {
            released_oob = matches!(&msg, Msg::Oob(_));
            released_clipboard = matches!(&msg, Msg::Clipboard(..));
            if let Msg::Clipboard(_, id) = &msg {
                let outcome = result.is_err().then_some(ClipboardOutcome::Uncertain);
                note_clipboard(&mut q, *id, outcome);
            }
            release_message_capacity(&mut q, &msg);
        }
        if let (Some(stamp), Some(started), Some(finished)) = (stamp, started, finished) {
            q.metrics.record(stamp, started, finished, !errored);
        }
        let deferred_ready = successful_frame_seq.is_some_and(|frame_seq| {
            q.deferred
                .front()
                .is_some_and(|deferred| deferred.defer_after < frame_seq)
        });
        // Keep replay failures consecutive across their successful resync
        // frames, so repeated delivery failure still reaches Fatal.
        if !(result.is_ok() && deferred_ready) {
            apply_write_result(&mut q, result);
        }
        if matches!(q.status, WriterStatus::Fatal(_)) {
            abandon_queued_messages(&mut q);
        }
        if let Some(frame_seq) = successful_frame_seq {
            while q
                .deferred
                .front()
                .is_some_and(|deferred| deferred.defer_after < frame_seq)
            {
                if let Some(deferred) = q.deferred.pop_front() {
                    replay.push_back(deferred.msg);
                }
            }
        }
        q.completed_seq = q.completed_seq.saturating_add(1);
    }
    inner.done_cv.notify_all();
    let mut replay_errored = false;
    while let Some(replay_msg) = replay.pop_front() {
        let (fatal, abandoned_oob, abandoned_clipboard) = {
            let mut q = inner.q.lock().unwrap_or_else(|e| e.into_inner());
            if matches!(q.status, WriterStatus::Fatal(_)) {
                let mut abandoned_oob = matches!(&replay_msg, Msg::Oob(_));
                let mut abandoned_clipboard = matches!(&replay_msg, Msg::Clipboard(..));
                if let Msg::Clipboard(_, id) = &replay_msg {
                    note_clipboard(&mut q, *id, Some(ClipboardOutcome::Uncertain));
                }
                release_message_capacity(&mut q, &replay_msg);
                while let Some(remaining) = replay.pop_front() {
                    abandoned_oob |= matches!(&remaining, Msg::Oob(_));
                    abandoned_clipboard |= matches!(&remaining, Msg::Clipboard(..));
                    if let Msg::Clipboard(_, id) = &remaining {
                        note_clipboard(&mut q, *id, Some(ClipboardOutcome::Uncertain));
                    }
                    release_message_capacity(&mut q, &remaining);
                }
                (true, abandoned_oob, abandoned_clipboard)
            } else {
                (false, false, false)
            }
        };
        if fatal {
            released_oob |= abandoned_oob;
            released_clipboard |= abandoned_clipboard;
            // The remaining local replay messages were abandoned with the
            // current one; release their reservations before dropping them.
            // (The loop above drained them while holding the accounting lock.)
            break;
        }
        let replay_bytes = match &replay_msg {
            Msg::Oob(bytes) | Msg::Clipboard(bytes, _) => bytes,
            Msg::Frame(..) => unreachable!("only OOB and clipboard messages are deferred"),
        };
        let replay_result = write(replay_bytes);
        let replay_failed = replay_result.is_err();
        let mut q = inner.q.lock().unwrap_or_else(|e| e.into_inner());
        if replay_failed {
            let defer_after = q.submitted_seq;
            let mut retry = VecDeque::new();
            retry.push_back(replay_msg);
            retry.append(&mut replay);
            while let Some(remaining) = retry.pop_back() {
                q.deferred.push_front(DeferredWrite {
                    msg: remaining,
                    defer_after,
                });
            }
            replay_errored = true;
        } else {
            released_oob |= matches!(&replay_msg, Msg::Oob(_));
            released_clipboard |= matches!(&replay_msg, Msg::Clipboard(..));
            if let Msg::Clipboard(_, id) = &replay_msg {
                note_clipboard(&mut q, *id, None);
            }
            release_message_capacity(&mut q, &replay_msg);
        }
        apply_write_result(&mut q, replay_result);
        if matches!(q.status, WriterStatus::Fatal(_)) {
            abandon_queued_messages(&mut q);
        }
        drop(q);
        if replay_failed {
            break;
        }
    }
    if released_oob || replay_errored {
        inner.done_cv.notify_all();
    }
    (errored || replay_errored, released_oob, released_clipboard)
}

fn release_oob_capacity(q: &mut Q, msg: &Msg) {
    if let Msg::Oob(bytes) = msg {
        q.oob_in_flight = q.oob_in_flight.saturating_sub(1);
        q.oob_bytes = q.oob_bytes.saturating_sub(bytes.len());
    }
}

fn release_message_capacity(q: &mut Q, msg: &Msg) {
    release_oob_capacity(q, msg);
    if let Msg::Clipboard(bytes, _) = msg {
        q.clipboard_queued = q.clipboard_queued.saturating_sub(1);
        q.clipboard_bytes = q.clipboard_bytes.saturating_sub(bytes.len());
    }
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
            q.parser_reset_pending = true;
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
            deferred: VecDeque::new(),
            frames_queued: 0,
            clipboard_queued: 0,
            clipboard_bytes: 0,
            clipboard_outcome: None,
            clipboard_committed: 0,
            oob_in_flight: 0,
            oob_bytes: 0,
            oob_retry_need: None,
            status: WriterStatus::Ok,
            consec_errs: 0,
            parser_reset_pending: false,
            shutdown: false,
            worker_done: false,
            submitted_seq: 0,
            completed_seq: 0,
            metrics: WriterMetrics::default(),
        }
    }

    fn test_writer() -> FrameWriter {
        test_writer_with_worker_frames(true)
    }

    fn test_writer_with_worker_frames(worker_frames: bool) -> FrameWriter {
        FrameWriter {
            inner: Arc::new(Inner {
                q: Mutex::new(q()),
                worker_frames,
                cv: Condvar::new(),
                done_cv: Condvar::new(),
                metrics_deferrals: AtomicU64::new(0),
                clipboard_retry: AtomicBool::new(false),
                oob_retry: AtomicBool::new(false),
                oob_pressure_logged: AtomicBool::new(false),
            }),
            handle: None,
            sync: false,
        }
    }

    fn gated_sync_writer(
        gate: std::sync::mpsc::Receiver<()>,
        log: Arc<Mutex<Vec<Vec<u8>>>>,
        finished: Option<std::sync::mpsc::Sender<()>>,
    ) -> FrameWriter {
        FrameWriter::spawn_with(
            true,
            true,
            |_| {},
            move |bytes| {
                gate.recv().unwrap();
                log.lock().unwrap().push(bytes.to_vec());
                if let Some(finished) = &finished {
                    finished.send(()).unwrap();
                }
                Ok(())
            },
        )
    }

    #[test]
    fn sync_submit_returns_within_bound_on_a_never_returning_sink() {
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let log = Arc::new(Mutex::new(Vec::new()));
        let writer = gated_sync_writer(release_rx, Arc::clone(&log), None);
        let started = Instant::now();
        assert!(writer.submit_frame(vec![1], None));
        assert!(started.elapsed() < SYNC_WRITE_BOUND + std::time::Duration::from_millis(150));
        assert!(matches!(writer.take_status(), WriterStatus::Fatal(_)));
        assert_eq!(writer.try_submit_oob(vec![2]), Err(vec![2]));
        release_tx.send(()).unwrap();
        drop(writer);
        assert_eq!(*log.lock().unwrap(), vec![vec![1]]);
    }

    #[test]
    fn sync_mode_preserves_frame_oob_clipboard_order_and_returns_after_completion() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let writer = FrameWriter::spawn_with(true, true, |_| {}, {
            let log = Arc::clone(&log);
            move |bytes| {
                log.lock().unwrap().push(bytes.to_vec());
                Ok(())
            }
        });
        assert!(writer.submit_frame(vec![1], None));
        assert_eq!(log.lock().unwrap().len(), 1);
        assert_eq!(writer.try_submit_oob(vec![2]), Ok(()));
        assert_eq!(log.lock().unwrap().len(), 2);
        let retry_deadline = Instant::now() + std::time::Duration::from_millis(250);
        loop {
            match writer.try_submit_clipboard(vec![3], || {}) {
                Ok(()) => break,
                Err(bytes) => {
                    assert_eq!(bytes, vec![3]);
                    assert!(
                        Instant::now() < retry_deadline,
                        "clipboard admission did not retry"
                    );
                    std::thread::yield_now();
                }
            }
        }
        assert_eq!(*log.lock().unwrap(), vec![vec![1], vec![2], vec![3]]);
        drop(writer);
    }

    #[test]
    fn drop_during_blocked_sync_write_is_bounded_and_emits_no_queued_bytes() {
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (finished_tx, finished_rx) = std::sync::mpsc::channel();
        let log = Arc::new(Mutex::new(Vec::new()));
        let sink_log = Arc::clone(&log);
        let writer = Arc::new(FrameWriter::spawn_with(
            true,
            true,
            |_| {},
            move |bytes| {
                started_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                sink_log.lock().unwrap().push(bytes.to_vec());
                finished_tx.send(()).unwrap();
                Ok(())
            },
        ));
        let first_writer = Arc::clone(&writer);
        let first = std::thread::spawn(move || first_writer.submit_frame(vec![1], None));
        started_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        {
            let mut q = writer.inner.q.lock().unwrap();
            assert_eq!(q.submitted_seq, 1);
            q.submitted_seq = q.submitted_seq.saturating_add(1);
            q.oob_in_flight += 1;
            q.oob_bytes += 1;
            q.msgs.push_back(Msg::Oob(vec![2]));
        }
        assert!(first.join().unwrap());
        let started = Instant::now();
        drop(writer);
        assert!(started.elapsed() < WRITER_SHUTDOWN_BOUND + std::time::Duration::from_millis(150));
        release_tx.send(()).unwrap();
        finished_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        assert_eq!(*log.lock().unwrap(), vec![vec![1]]);
    }

    #[test]
    fn clipboard_admission_is_bounded_fifo_and_capacity_is_returned_after_write() {
        let writer = test_writer();
        writer.submit_oob(vec![0]);
        for byte in 1..=4 {
            assert!(writer.try_submit_clipboard(vec![byte], || panic!()).is_ok());
        }
        assert_eq!(
            writer.try_submit_clipboard(vec![5], || panic!()),
            Err(vec![5])
        );
        let mut q = writer.inner.q.lock().unwrap();
        assert_eq!(q.clipboard_queued, 4);
        assert_eq!(q.clipboard_bytes, 4);
        assert!(matches!(pop_message(&mut q), Some(Msg::Oob(ref b)) if b == &[0]));
        for byte in 1..=4 {
            assert!(matches!(pop_message(&mut q), Some(Msg::Clipboard(ref b, _) ) if b == &[byte]));
        }
        assert_eq!(q.clipboard_queued, 4);
        assert_eq!(q.clipboard_bytes, 4);
        drop(q);
        assert!(writer.try_submit_clipboard(vec![5], || panic!()).is_err());
        for byte in 1..=4 {
            assert!(
                !write_message(
                    &writer.inner,
                    Msg::Clipboard(vec![byte], u64::from(byte)),
                    |_| Ok(())
                )
                .0
            );
        }
        assert!(writer.try_submit_clipboard(vec![5], || panic!()).is_ok());
        assert!(writer.inner.clipboard_retry.load(Ordering::SeqCst));
        assert!(
            writer
                .try_submit_clipboard(
                    vec![0; crate::queries::clipboard::MAX_WIRE + 1],
                    || panic!()
                )
                .is_err()
        );
    }

    #[test]
    fn dropped_failed_clipboard_write_reports_uncertain_once() {
        let writer = test_writer_with_worker_frames(false);
        writer
            .try_submit_clipboard(vec![b'C'], || panic!())
            .unwrap();
        let clipboard = pop_message(&mut writer.inner.q.lock().unwrap()).unwrap();
        assert!(
            write_message(&writer.inner, clipboard, |_| {
                Err(std::io::ErrorKind::Interrupted.into())
            })
            .0
        );
        assert_eq!(
            writer.take_clipboard_outcome(),
            Some(ClipboardOutcome::Uncertain)
        );
        assert_eq!(writer.take_clipboard_outcome(), None);
    }

    #[test]
    fn queued_clipboard_abandoned_before_write_reports_not_written() {
        let writer = test_writer();
        writer
            .try_submit_clipboard(vec![b'C'], || panic!())
            .unwrap();
        abandon_queued_messages(&mut writer.inner.q.lock().unwrap());
        assert_eq!(
            writer.take_clipboard_outcome(),
            Some(ClipboardOutcome::NotWritten)
        );
    }

    #[test]
    fn refused_clipboard_admission_has_no_outcome() {
        let writer = test_writer();
        for byte in 0..4 {
            assert!(writer.try_submit_clipboard(vec![byte], || panic!()).is_ok());
        }
        assert_eq!(
            writer.try_submit_clipboard(vec![5], || panic!()),
            Err(vec![5])
        );
        assert_eq!(writer.take_clipboard_outcome(), None);
    }

    #[test]
    fn fatal_clipboard_drop_before_write_reports_not_written() {
        let writer = test_writer();
        writer
            .try_submit_clipboard(vec![b'C'], || panic!())
            .unwrap();
        let clipboard = pop_message(&mut writer.inner.q.lock().unwrap()).unwrap();
        writer.inner.q.lock().unwrap().status =
            WriterStatus::Fatal("test terminal failure".to_string());
        assert!(!write_message(&writer.inner, clipboard, |_| panic!()).0);
        assert_eq!(
            writer.take_clipboard_outcome(),
            Some(ClipboardOutcome::NotWritten)
        );
    }

    #[test]
    fn failed_then_replayed_clipboard_commits_without_an_outcome() {
        let writer = test_writer();
        writer
            .try_submit_clipboard(vec![b'C'], || panic!())
            .unwrap();
        let clipboard = pop_message(&mut writer.inner.q.lock().unwrap()).unwrap();
        assert!(
            write_message(&writer.inner, clipboard, |_| {
                Err(std::io::ErrorKind::Interrupted.into())
            })
            .0
        );
        assert_eq!(writer.take_clipboard_outcome(), None);
        assert!(writer.submit_frame(vec![b'F'], None));
        let frame = pop_message(&mut writer.inner.q.lock().unwrap()).unwrap();
        assert!(!write_message(&writer.inner, frame, |_| Ok(())).0);
        assert_eq!(writer.take_clipboard_outcome(), None);
    }

    #[test]
    fn deferred_clipboard_abandoned_on_worker_exit_reports_uncertain() {
        let writer = test_writer();
        writer
            .try_submit_clipboard(vec![b'C'], || panic!())
            .unwrap();
        let clipboard = pop_message(&mut writer.inner.q.lock().unwrap()).unwrap();
        assert!(
            write_message(&writer.inner, clipboard, |_| {
                Err(std::io::ErrorKind::Interrupted.into())
            })
            .0
        );
        abandon_deferred_writes(&mut writer.inner.q.lock().unwrap());
        assert_eq!(
            writer.take_clipboard_outcome(),
            Some(ClipboardOutcome::Uncertain)
        );
    }

    #[test]
    fn frame_oob_and_bell_failures_never_produce_a_clipboard_outcome() {
        let writer = test_writer_with_worker_frames(false);
        assert!(writer.submit_frame(b"frame bell".to_vec(), None));
        let frame = pop_message(&mut writer.inner.q.lock().unwrap()).unwrap();
        assert!(
            write_message(&writer.inner, frame, |_| {
                Err(std::io::ErrorKind::Interrupted.into())
            })
            .0
        );
        assert_eq!(writer.take_clipboard_outcome(), None);

        assert!(writer.try_submit_oob(b"graphics".to_vec()).is_ok());
        let oob = pop_message(&mut writer.inner.q.lock().unwrap()).unwrap();
        assert!(
            write_message(&writer.inner, oob, |_| {
                Err(std::io::ErrorKind::Interrupted.into())
            })
            .0
        );
        assert_eq!(writer.take_clipboard_outcome(), None);
    }

    #[test]
    fn newer_committed_clipboard_supersedes_an_older_outcome() {
        let writer = test_writer_with_worker_frames(false);
        writer
            .try_submit_clipboard(vec![b'A'], || panic!())
            .unwrap();
        let first = pop_message(&mut writer.inner.q.lock().unwrap()).unwrap();
        let first_id = match &first {
            Msg::Clipboard(_, id) => *id,
            _ => panic!("expected clipboard message"),
        };
        assert!(
            write_message(&writer.inner, first, |_| {
                Err(std::io::ErrorKind::Interrupted.into())
            })
            .0
        );
        assert!(writer.try_submit_clipboard(vec![b'B'], || panic!()).is_ok());
        let second = pop_message(&mut writer.inner.q.lock().unwrap()).unwrap();
        assert!(!write_message(&writer.inner, second, |_| Ok(())).0);
        assert_eq!(writer.take_clipboard_outcome(), None);
        note_clipboard(
            &mut writer.inner.q.lock().unwrap(),
            first_id,
            Some(ClipboardOutcome::Uncertain),
        );
        assert_eq!(writer.take_clipboard_outcome(), None);
    }

    #[test]
    fn clipboard_outcome_status_is_payload_free() {
        let writer = test_writer_with_worker_frames(false);
        writer
            .try_submit_clipboard(b"SECRET-PAYLOAD".to_vec(), || panic!())
            .unwrap();
        let clipboard = pop_message(&mut writer.inner.q.lock().unwrap()).unwrap();
        assert!(
            write_message(&writer.inner, clipboard, |_| {
                Err(std::io::ErrorKind::Interrupted.into())
            })
            .0
        );
        let outcome = writer.take_clipboard_outcome().unwrap();
        let status = clipboard_outcome_status(outcome);
        assert!(!status.is_empty());
        assert!(status.contains("clipboard"));
        assert!(!status.contains("SECRET-PAYLOAD"));
    }

    #[test]
    fn clipboard_lock_contention_with_empty_queue_pulses_retry_and_retains_bytes() {
        let writer = test_writer();
        let held = writer.inner.q.lock().unwrap();
        let mut wakes = 0;
        assert_eq!(
            writer.try_submit_clipboard(vec![7], || wakes += 1),
            Err(vec![7])
        );
        assert_eq!(wakes, 1);
        assert!(held.msgs.is_empty());
        drop(held);
        assert!(writer.try_submit_clipboard(vec![7], || wakes += 1).is_ok());
        assert_eq!(wakes, 1);
        let msg = pop_message(&mut writer.inner.q.lock().unwrap()).unwrap();
        assert!(
            !write_message(&writer.inner, msg, |b| {
                assert_eq!(b, &[7]);
                Ok(())
            })
            .0
        );
        assert_eq!(writer.take_metrics().unwrap().completion_us.count(), 0);
    }

    #[test]
    fn completion_is_recorded_after_sink_returns_and_oob_is_excluded() {
        let writer = test_writer();
        let stamp = FrameStamp {
            queued_at: Instant::now(),
            input_at: Some(Instant::now()),
        };
        assert!(
            !write_message(
                &writer.inner,
                Msg::Frame(vec![1], Some(stamp), 1),
                |bytes| {
                    assert_eq!(bytes, [1]);
                    // A blocked sink must not hold the UI's queue/metrics mutex.
                    assert_eq!(writer.take_metrics().unwrap().completion_us.count(), 0);
                    Ok(())
                }
            )
            .0
        );
        assert_eq!(writer.take_metrics().unwrap().completion_us.count(), 1);
        let (errored, released_oob, _) =
            write_message(&writer.inner, Msg::Oob(vec![2]), |_| Ok(()));
        assert!(!errored);
        assert!(released_oob, "completed OOB write must release capacity");
        assert!(
            !oob_capacity_wake_requested(&writer.inner, released_oob),
            "capacity release without a refused submission must not wake the loop"
        );
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
        assert!(
            write_message(&writer.inner, Msg::Frame(vec![], Some(stamp), 1), |_| {
                Err(std::io::ErrorKind::Interrupted.into())
            })
            .0
        );
        let metrics = writer.take_metrics().unwrap();
        assert_eq!(metrics.failed_frames, 1);
        assert_eq!(metrics.completion_us.count(), 0);
        assert_eq!(metrics.input_completion_us.count(), 0);
    }

    #[test]
    fn next_message_starts_with_parser_reset_after_a_failed_write() {
        let writer = test_writer();
        apply_write_result(
            &mut writer.inner.q.lock().unwrap(),
            Err(std::io::ErrorKind::Interrupted.into()),
        );
        assert!(writer.inner.q.lock().unwrap().parser_reset_pending);

        let mut calls = Vec::new();
        let (errored, _, _) = write_message(&writer.inner, Msg::Frame(vec![7], None, 1), |bytes| {
            calls.push(bytes.to_vec());
            Ok(())
        });
        assert!(!errored);
        assert_eq!(calls, vec![PARSER_RESET.to_vec(), vec![7]]);
        assert!(!writer.inner.q.lock().unwrap().parser_reset_pending);
        let (errored, _, _) = write_message(&writer.inner, Msg::Frame(vec![8], None, 2), |bytes| {
            calls.push(bytes.to_vec());
            Ok(())
        });
        assert!(!errored);
        assert_eq!(calls, vec![PARSER_RESET.to_vec(), vec![7], vec![8]]);
    }

    #[test]
    fn initial_success_does_not_emit_parser_reset() {
        let writer = test_writer();
        let mut calls = Vec::new();
        let (errored, _, _) = write_message(&writer.inner, Msg::Frame(vec![8], None, 1), |bytes| {
            calls.push(bytes.to_vec());
            Ok(())
        });
        assert!(!errored);
        assert_eq!(calls, vec![vec![8]]);
    }

    #[test]
    fn failed_parser_reset_keeps_flag_and_does_not_write_message() {
        let writer = test_writer();
        apply_write_result(
            &mut writer.inner.q.lock().unwrap(),
            Err(std::io::ErrorKind::Interrupted.into()),
        );
        let mut calls = Vec::new();
        let (errored, _, _) = write_message(&writer.inner, Msg::Frame(vec![9], None, 1), |bytes| {
            calls.push(bytes.to_vec());
            Err(std::io::ErrorKind::Interrupted.into())
        });
        assert!(errored);
        assert_eq!(calls, vec![PARSER_RESET.to_vec()]);
        assert!(writer.inner.q.lock().unwrap().parser_reset_pending);
    }

    #[test]
    fn failed_oob_is_replayed_after_the_first_later_successful_frame() {
        let writer = test_writer();
        assert!(writer.try_submit_oob(vec![b'A']).is_ok());
        let failed = pop_message(&mut writer.inner.q.lock().unwrap()).unwrap();
        let mut calls = Vec::new();
        let (errored, released, _) = write_message(&writer.inner, failed, |bytes| {
            calls.push(bytes.to_vec());
            Err(std::io::ErrorKind::Interrupted.into())
        });
        assert!(errored);
        assert!(!released);
        {
            let q = writer.inner.q.lock().unwrap();
            assert_eq!(q.oob_in_flight, 1);
            assert_eq!(q.oob_bytes, 1);
            assert_eq!(q.deferred.len(), 1);
        }
        assert!(writer.submit_frame(vec![b'F'], None));
        let frame = pop_message(&mut writer.inner.q.lock().unwrap()).unwrap();
        let (errored, _, _) = write_message(&writer.inner, frame, |bytes| {
            calls.push(bytes.to_vec());
            Ok(())
        });
        assert!(!errored);
        assert_eq!(
            calls,
            vec![vec![b'A'], PARSER_RESET.to_vec(), vec![b'F'], vec![b'A']]
        );
        let q = writer.inner.q.lock().unwrap();
        assert_eq!(q.oob_in_flight, 0);
        assert_eq!(q.oob_bytes, 0);
        assert_eq!(q.completed_seq, 2, "replay is not a submitted completion");
        assert!(q.deferred.is_empty());
    }

    #[test]
    fn deferred_oob_waits_past_pre_failure_frames_and_later_oob() {
        let writer = test_writer();
        assert!(writer.submit_frame(vec![b'P'], None));
        assert!(writer.try_submit_oob(vec![b'A']).is_ok());
        let mut q = writer.inner.q.lock().unwrap();
        let prior_frame = pop_message(&mut q).unwrap();
        let failed_oob = pop_message(&mut q).unwrap();
        drop(q);

        assert!(
            write_message(&writer.inner, failed_oob, |_| {
                Err(std::io::ErrorKind::Interrupted.into())
            })
            .0
        );
        let mut calls = Vec::new();
        assert!(
            !write_message(&writer.inner, prior_frame, |bytes| {
                calls.push(bytes.to_vec());
                Ok(())
            })
            .0
        );
        assert!(writer.try_submit_oob(vec![b'B']).is_ok());
        let later_oob = pop_message(&mut writer.inner.q.lock().unwrap()).unwrap();
        assert!(
            !write_message(&writer.inner, later_oob, |bytes| {
                calls.push(bytes.to_vec());
                Ok(())
            })
            .0
        );
        assert_eq!(calls, vec![PARSER_RESET.to_vec(), vec![b'P'], vec![b'B']]);
        assert_eq!(writer.inner.q.lock().unwrap().deferred.len(), 1);

        assert!(writer.submit_frame(vec![b'F'], None));
        let frame = pop_message(&mut writer.inner.q.lock().unwrap()).unwrap();
        assert!(
            !write_message(&writer.inner, frame, |bytes| {
                calls.push(bytes.to_vec());
                Ok(())
            })
            .0
        );
        assert_eq!(calls.last(), Some(&vec![b'A']));
    }

    #[test]
    fn failed_clipboard_write_keeps_its_admission_until_frame_replay() {
        let writer = test_writer();
        writer
            .try_submit_clipboard(vec![b'C'], || panic!())
            .unwrap();
        let clipboard = pop_message(&mut writer.inner.q.lock().unwrap()).unwrap();
        assert!(
            write_message(&writer.inner, clipboard, |_| {
                Err(std::io::ErrorKind::Interrupted.into())
            })
            .0
        );
        {
            let q = writer.inner.q.lock().unwrap();
            assert_eq!(q.clipboard_queued, 1);
            assert_eq!(q.clipboard_bytes, 1);
        }
        assert!(writer.submit_frame(vec![b'F'], None));
        let frame = pop_message(&mut writer.inner.q.lock().unwrap()).unwrap();
        let mut calls = Vec::new();
        assert!(
            !write_message(&writer.inner, frame, |bytes| {
                calls.push(bytes.to_vec());
                Ok(())
            })
            .0
        );
        assert_eq!(calls, vec![PARSER_RESET.to_vec(), vec![b'F'], vec![b'C']]);
        let q = writer.inner.q.lock().unwrap();
        assert_eq!(q.clipboard_queued, 0);
        assert_eq!(q.clipboard_bytes, 0);
    }

    #[test]
    fn repeated_deferred_replay_failure_becomes_fatal_and_abandon_releases_capacity() {
        let writer = test_writer();
        assert!(writer.try_submit_oob(vec![b'A']).is_ok());
        let oob = pop_message(&mut writer.inner.q.lock().unwrap()).unwrap();
        assert!(
            write_message(&writer.inner, oob, |_| {
                Err(std::io::ErrorKind::Interrupted.into())
            })
            .0
        );
        writer
            .try_submit_clipboard(vec![b'C'], || panic!())
            .unwrap();
        let clipboard = pop_message(&mut writer.inner.q.lock().unwrap()).unwrap();
        assert!(
            write_message(&writer.inner, clipboard, |_| {
                Err(std::io::ErrorKind::Interrupted.into())
            })
            .0
        );

        for _ in 0..=crate::frame_write::RETRY_MAX {
            assert!(writer.submit_frame(vec![b'F'], None));
            let frame = pop_message(&mut writer.inner.q.lock().unwrap()).unwrap();
            let (errored, _, _) = write_message(&writer.inner, frame, |bytes| {
                if bytes == *b"A" || bytes == *b"C" {
                    Err(std::io::ErrorKind::Interrupted.into())
                } else {
                    Ok(())
                }
            });
            assert!(errored);
            if matches!(writer.take_status(), WriterStatus::Fatal(_)) {
                break;
            }
        }
        assert!(matches!(writer.take_status(), WriterStatus::Fatal(_)));
        let mut q = writer.inner.q.lock().unwrap();
        abandon_queued_messages(&mut q);
        assert!(q.deferred.is_empty());
        assert_eq!(q.oob_in_flight, 0);
        assert_eq!(q.oob_bytes, 0);
        assert_eq!(q.clipboard_queued, 0);
        assert_eq!(q.clipboard_bytes, 0);
    }

    #[test]
    fn termwiz_parity_drops_failed_oob_without_a_worker_frame() {
        let writer = test_writer_with_worker_frames(false);
        assert!(writer.try_submit_oob(vec![b'A']).is_ok());
        let oob = pop_message(&mut writer.inner.q.lock().unwrap()).unwrap();
        assert!(
            write_message(&writer.inner, oob, |_| {
                Err(std::io::ErrorKind::Interrupted.into())
            })
            .0
        );
        let q = writer.inner.q.lock().unwrap();
        assert!(q.deferred.is_empty());
        assert_eq!(q.oob_in_flight, 0);
        assert_eq!(q.oob_bytes, 0);
    }

    #[test]
    fn normal_shutdown_releases_deferred_write_reservations() {
        let writer = FrameWriter::spawn_with(
            false,
            true,
            |_| {},
            |_| Err(std::io::ErrorKind::Interrupted.into()),
        );
        let inner = Arc::clone(&writer.inner);
        assert!(writer.submit_frame(vec![b'F'], None));
        assert!(writer.try_submit_oob(vec![b'A']).is_ok());
        {
            let mut q = inner.q.lock().unwrap();
            let deadline = Instant::now() + std::time::Duration::from_secs(1);
            while q.deferred.is_empty() {
                let now = Instant::now();
                assert!(now < deadline, "writer did not defer failed OOB");
                let (next, _) = inner
                    .done_cv
                    .wait_timeout(q, deadline.saturating_duration_since(now))
                    .unwrap();
                q = next;
            }
            assert_eq!(q.oob_in_flight, 1);
            assert_eq!(q.oob_bytes, 1);
        }
        drop(writer);
        let q = inner.q.lock().unwrap();
        assert!(q.worker_done);
        assert!(q.deferred.is_empty());
        assert_eq!(q.oob_in_flight, 0);
        assert_eq!(q.oob_bytes, 0);
        assert_eq!(q.clipboard_queued, 0);
        assert_eq!(q.clipboard_bytes, 0);
    }

    #[test]
    fn fatal_during_parser_reset_suppresses_the_payload_write() {
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let log = Arc::new(Mutex::new(Vec::new()));
        let sink_log = Arc::clone(&log);
        let writer = Arc::new(FrameWriter::spawn_with(
            true,
            true,
            |_| {},
            move |bytes| {
                if bytes == PARSER_RESET {
                    started_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                }
                sink_log.lock().unwrap().push(bytes.to_vec());
                Ok(())
            },
        ));
        writer.inner.q.lock().unwrap().parser_reset_pending = true;
        let submitting = Arc::clone(&writer);
        let submit = std::thread::spawn(move || submitting.submit_frame(vec![b'P'], None));
        started_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        assert!(submit.join().unwrap());
        assert!(matches!(writer.take_status(), WriterStatus::Fatal(_)));
        release_tx.send(()).unwrap();
        drop(writer);
        assert_eq!(*log.lock().unwrap(), vec![PARSER_RESET.to_vec()]);
    }

    #[test]
    fn fatal_during_first_replay_abandons_remaining_oob_and_releases_capacity() {
        let writer = test_writer();
        for byte in *b"AB" {
            assert!(writer.try_submit_oob(vec![byte]).is_ok());
            let oob = pop_message(&mut writer.inner.q.lock().unwrap()).unwrap();
            assert!(
                write_message(&writer.inner, oob, |_| {
                    Err(std::io::ErrorKind::Interrupted.into())
                })
                .0
            );
        }
        assert!(writer.submit_frame(vec![b'F'], None));
        let frame = pop_message(&mut writer.inner.q.lock().unwrap()).unwrap();
        let mut calls = Vec::new();
        let (errored, released_oob, _) = write_message(&writer.inner, frame, |bytes| {
            calls.push(bytes.to_vec());
            if bytes == *b"A" {
                writer.inner.q.lock().unwrap().status = WriterStatus::Fatal("sink deadline".into());
            }
            Ok(())
        });
        assert!(!errored);
        assert!(released_oob);
        assert_eq!(calls, vec![PARSER_RESET.to_vec(), vec![b'F'], vec![b'A']]);
        let q = writer.inner.q.lock().unwrap();
        assert!(matches!(q.status, WriterStatus::Fatal(_)));
        assert_eq!(q.oob_in_flight, 0);
        assert_eq!(q.oob_bytes, 0);
        assert!(q.deferred.is_empty());
    }

    #[test]
    fn fatal_writer_does_not_emit_parser_reset_or_message() {
        let writer = test_writer();
        {
            let mut q = writer.inner.q.lock().unwrap();
            q.status = WriterStatus::Fatal("test fatal".into());
            q.parser_reset_pending = true;
        }
        let mut calls = 0;
        let (errored, _, _) = write_message(&writer.inner, Msg::Frame(vec![1], None, 1), |_| {
            calls += 1;
            Ok(())
        });
        assert!(!errored);
        assert_eq!(calls, 0);
        assert!(writer.inner.q.lock().unwrap().parser_reset_pending);
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

    #[test]
    fn oob_admission_accepts_four_mib_and_refusal_is_not_fatal() {
        let writer = test_writer();
        assert!(writer.try_submit_oob(vec![0; OOB_QUEUE_BYTES]).is_ok());
        assert_eq!(writer.inner.q.lock().unwrap().oob_bytes, 4 * 1024 * 1024);

        let byte_bounded = test_writer();
        assert!(
            byte_bounded
                .try_submit_oob(vec![0; 3 * 1024 * 1024])
                .is_ok()
        );
        assert_eq!(
            byte_bounded
                .try_submit_oob(vec![1; 2 * 1024 * 1024])
                .unwrap_err()
                .len(),
            2 * 1024 * 1024
        );

        let refusing = test_writer();
        for _ in 0..OOB_QUEUE_COUNT {
            assert!(refusing.try_submit_oob(vec![1]).is_ok());
        }
        refusing.submit_oob(vec![2]);
        assert_eq!(refusing.take_status(), WriterStatus::Ok);
        let refused = refusing.try_submit_oob(vec![3]).unwrap_err();
        assert_eq!(refused, vec![3]);
        assert!(refusing.inner.oob_retry.load(Ordering::SeqCst));

        let release_writer = test_writer();
        assert!(release_writer.try_submit_oob(vec![9]).is_ok());
        let active = pop_message(&mut release_writer.inner.q.lock().unwrap()).unwrap();
        let (errored, released, _) = write_message(&release_writer.inner, active, |_| Ok(()));
        assert!(!errored);
        assert!(released, "completed OOB write must release capacity");
        assert!(!oob_capacity_wake_requested(
            &release_writer.inner,
            released
        ));
        assert!(release_writer.try_submit_oob(vec![4]).is_ok());
    }

    #[test]
    fn oob_pressure_warns_once_and_refusal_wakes_on_capacity_release() {
        let writer = test_writer();
        for _ in 0..OOB_QUEUE_COUNT {
            assert!(writer.try_submit_oob(vec![1]).is_ok());
        }
        assert_eq!(writer.try_submit_oob(vec![2]), Err(vec![2]));
        assert!(writer.inner.oob_retry.load(Ordering::SeqCst));

        writer.submit_oob(vec![3]);
        assert!(writer.inner.oob_pressure_logged.load(Ordering::SeqCst));
        writer.submit_oob(vec![4]);
        assert!(writer.inner.oob_pressure_logged.load(Ordering::SeqCst));

        let active = pop_message(&mut writer.inner.q.lock().unwrap()).unwrap();
        let (errored, released, _) = write_message(&writer.inner, active, |_| Ok(()));
        assert!(!errored);
        assert!(released);
        assert!(oob_capacity_wake_requested(&writer.inner, released));
        assert!(!writer.inner.oob_retry.load(Ordering::SeqCst));

        assert!(writer.try_submit_oob(vec![5]).is_ok());
        assert!(!writer.inner.oob_pressure_logged.load(Ordering::SeqCst));
    }

    #[test]
    fn refused_clipboard_copy_retries_after_oob_capacity_wake() {
        let writer = test_writer();
        for _ in 0..OOB_QUEUE_COUNT {
            assert!(writer.try_submit_oob(vec![1]).is_ok());
        }

        let mut clipboard = crate::preview_retry::PendingClipboardOob::default();
        let osc52 = crate::copymode::osc52("hi");
        clipboard.offer(osc52.clone(), |bytes| writer.try_submit_oob(bytes));
        assert!(clipboard.is_pending());
        assert!(writer.inner.oob_retry.load(Ordering::SeqCst));

        let active = pop_message(&mut writer.inner.q.lock().unwrap()).unwrap();
        let (errored, released, _) = write_message(&writer.inner, active, |_| Ok(()));
        assert!(!errored);
        assert!(released);
        assert!(oob_capacity_wake_requested(&writer.inner, released));

        clipboard.retry(|bytes| writer.try_submit_oob(bytes));
        let q = writer.inner.q.lock().unwrap();
        assert!(matches!(q.msgs.back(), Some(Msg::Oob(bytes)) if bytes == &osc52));
        assert!(!clipboard.is_pending());
    }

    #[test]
    fn drop_deadline_discards_queued_messages_while_sink_is_blocked() {
        let base = test_writer();
        let inner = Arc::clone(&base.inner);
        drop(base);
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let written = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
        {
            let mut q = inner.q.lock().unwrap();
            q.msgs.push_back(Msg::Oob(vec![2]));
            q.msgs.push_back(Msg::Frame(vec![3], None, 2));
            q.oob_in_flight = 1;
            q.oob_bytes = 1;
            q.frames_queued = 1;
        }
        let written_by_worker = Arc::clone(&written);
        let inner_for_worker = Arc::clone(&inner);
        let handle = std::thread::spawn(move || {
            let (errored, _, _) =
                write_message(&inner_for_worker, Msg::Frame(vec![1], None, 1), |bytes| {
                    started_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    written_by_worker.lock().unwrap().push(bytes.to_vec());
                    Ok(())
                });
            assert!(!errored);
            let mut q = inner_for_worker.q.lock().unwrap();
            while let Some(msg) = pop_message(&mut q) {
                drop(q);
                let (errored, _, _) = write_message(&inner_for_worker, msg, |bytes| {
                    written_by_worker.lock().unwrap().push(bytes.to_vec());
                    Ok(())
                });
                assert!(!errored);
                q = inner_for_worker.q.lock().unwrap();
            }
            q.worker_done = true;
            inner_for_worker.cv.notify_all();
            done_tx.send(()).unwrap();
        });
        let writer = FrameWriter {
            inner: Arc::clone(&inner),
            handle: Some(handle),
            sync: false,
        };
        started_rx.recv().unwrap();
        let started_at = Instant::now();
        drop(writer);
        assert!(
            started_at.elapsed() < WRITER_SHUTDOWN_BOUND + std::time::Duration::from_millis(100)
        );
        {
            let q = inner.q.lock().unwrap();
            assert!(matches!(q.status, WriterStatus::Fatal(_)));
            assert!(q.msgs.is_empty());
            assert_eq!(q.frames_queued, 0);
            assert_eq!(q.oob_in_flight, 0);
            assert_eq!(q.oob_bytes, 0);
        }
        release_tx.send(()).unwrap();
        // The only bytes that may land after the deadline are from the one
        // write already in flight; the queued frame must be discarded.
        done_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        assert_eq!(*written.lock().unwrap(), vec![vec![1]]);
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

    #[test]
    fn small_oob_release_does_not_wake_a_refused_near_limit_message() {
        let writer = test_writer();
        assert!(writer.try_submit_oob(vec![1]).is_ok());
        assert!(writer.try_submit_oob(vec![2; 1024 * 1024]).is_ok());
        let refused = writer.try_submit_oob(vec![3; 3_500 * 1024]);
        assert!(refused.is_err());
        assert!(writer.inner.oob_retry.load(Ordering::SeqCst));

        let small = pop_message(&mut writer.inner.q.lock().unwrap()).unwrap();
        let (errored, released, _) = write_message(&writer.inner, small, |_| Ok(()));
        assert!(!errored);
        assert!(released);
        assert!(!oob_capacity_wake_requested(&writer.inner, released));
        assert!(writer.inner.oob_retry.load(Ordering::SeqCst));

        let large = pop_message(&mut writer.inner.q.lock().unwrap()).unwrap();
        let (errored, released, _) = write_message(&writer.inner, large, |_| Ok(()));
        assert!(!errored);
        assert!(released);
        assert!(oob_capacity_wake_requested(&writer.inner, released));
        assert!(!writer.inner.oob_retry.load(Ordering::SeqCst));
        assert!(writer.try_submit_oob(vec![3; 3_500 * 1024]).is_ok());
    }

    #[test]
    fn smallest_refused_need_wakes_first() {
        let writer = test_writer();
        assert!(writer.try_submit_oob(vec![1; 300 * 1024]).is_ok());
        assert!(writer.try_submit_oob(vec![2; 3_600 * 1024]).is_ok());
        assert!(writer.try_submit_oob(vec![3; 3_500 * 1024]).is_err());
        assert!(writer.try_submit_oob(vec![4; 200 * 1024]).is_err());

        let small = pop_message(&mut writer.inner.q.lock().unwrap()).unwrap();
        let (errored, released, _) = write_message(&writer.inner, small, |_| Ok(()));
        assert!(!errored);
        assert!(released);
        assert!(oob_capacity_wake_requested(&writer.inner, released));
        assert!(writer.try_submit_oob(vec![4; 200 * 1024]).is_ok());
    }

    #[test]
    fn dropped_lossy_refusal_does_not_dilute_retained_wake_need() {
        let writer = test_writer();
        assert!(writer.try_submit_oob(vec![1; 300 * 1024]).is_ok());
        assert!(writer.try_submit_oob(vec![2; 3_600 * 1024]).is_ok());
        assert!(writer.try_submit_oob(vec![3; 3_500 * 1024]).is_err());
        assert_eq!(
            writer.inner.q.lock().unwrap().oob_retry_need,
            Some(3_500 * 1024)
        );

        writer.submit_oob(vec![4]);
        assert_eq!(
            writer.inner.q.lock().unwrap().oob_retry_need,
            Some(3_500 * 1024)
        );

        let small = pop_message(&mut writer.inner.q.lock().unwrap()).unwrap();
        let (errored, released, _) = write_message(&writer.inner, small, |_| Ok(()));
        assert!(!errored);
        assert!(released);
        assert!(!oob_capacity_wake_requested(&writer.inner, released));
        assert!(writer.inner.oob_retry.load(Ordering::SeqCst));
    }

    #[test]
    fn host_clipboard_copy_is_a_typed_clipboard_message() {
        let writer = test_writer();
        let bytes = crate::copymode::osc52("hi");
        assert!(writer.try_submit_host_clipboard(bytes.clone()).is_ok());
        let mut q = writer.inner.q.lock().unwrap();
        assert!(matches!(pop_message(&mut q), Some(Msg::Clipboard(queued, _)) if queued == bytes));
        assert_eq!(q.oob_in_flight, 0);
        assert_eq!(q.clipboard_queued, 1);
    }

    #[test]
    fn failed_host_copy_reports_uncertain_without_payload() {
        let writer = test_writer_with_worker_frames(false);
        let payload = b"SECRET-PAYLOAD".to_vec();
        assert!(writer.try_submit_host_clipboard(payload.clone()).is_ok());
        let clipboard = pop_message(&mut writer.inner.q.lock().unwrap()).unwrap();
        assert!(
            write_message(&writer.inner, clipboard, |_| {
                Err(std::io::ErrorKind::Interrupted.into())
            })
            .0
        );
        assert_eq!(
            writer.take_clipboard_outcome(),
            Some(ClipboardOutcome::Uncertain)
        );
        assert!(
            !clipboard_outcome_status(ClipboardOutcome::Uncertain)
                .as_bytes()
                .windows(payload.len())
                .any(|window| window == payload.as_slice())
        );
    }

    #[test]
    fn abandoned_host_copy_reports_not_written() {
        let writer = test_writer();
        assert!(writer.try_submit_host_clipboard(vec![b'C']).is_ok());
        abandon_queued_messages(&mut writer.inner.q.lock().unwrap());
        assert_eq!(
            writer.take_clipboard_outcome(),
            Some(ClipboardOutcome::NotWritten)
        );
    }

    #[test]
    fn newer_committed_host_copy_supersedes_an_older_failure() {
        let writer = test_writer_with_worker_frames(false);
        assert!(writer.try_submit_host_clipboard(vec![b'A']).is_ok());
        assert!(writer.try_submit_host_clipboard(vec![b'B']).is_ok());
        let older = pop_message(&mut writer.inner.q.lock().unwrap()).unwrap();
        assert!(
            write_message(&writer.inner, older, |_| {
                Err(std::io::ErrorKind::Interrupted.into())
            })
            .0
        );
        let newer = pop_message(&mut writer.inner.q.lock().unwrap()).unwrap();
        assert!(!write_message(&writer.inner, newer, |_| Ok(())).0);
        assert_eq!(writer.take_clipboard_outcome(), None);
    }

    #[test]
    fn refused_host_copy_arms_the_clipboard_wake_and_is_admitted_after_release() {
        let writer = test_writer();
        for byte in 0..4 {
            assert!(writer.try_submit_host_clipboard(vec![byte]).is_ok());
        }
        let refused = vec![5];
        assert_eq!(
            writer.try_submit_host_clipboard(refused.clone()),
            Err(refused)
        );
        assert!(writer.inner.clipboard_retry.load(Ordering::SeqCst));

        let active = pop_message(&mut writer.inner.q.lock().unwrap()).unwrap();
        let (errored, _, released_clipboard) = write_message(&writer.inner, active, |_| Ok(()));
        assert!(!errored);
        assert!(released_clipboard);
        assert!(writer.try_submit_host_clipboard(vec![5]).is_ok());
    }

    #[test]
    fn host_copy_budget_admits_a_copy_too_large_for_pane_sets() {
        let writer = test_writer();
        let large_copy = vec![b'x'; 1024 * 1024];
        assert!(writer.try_submit_host_clipboard(large_copy.clone()).is_ok());
        assert!(writer.try_submit_clipboard(large_copy, || {}).is_err());
        assert!(
            writer
                .try_submit_host_clipboard(vec![b'x'; OOB_QUEUE_BYTES + 1])
                .is_err()
        );
    }
}
