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
use std::sync::{Arc, Condvar, Mutex};

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
    Frame(Vec<u8>),
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
}

struct Inner {
    q: Mutex<Q>,
    cv: Condvar,
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
            }),
            cv: Condvar::new(),
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
    pub(crate) fn submit_frame(&self, bytes: Vec<u8>) -> bool {
        if self.sync {
            self.write_inline(&bytes);
            return true;
        }
        let mut q = self.inner.q.lock().unwrap_or_else(|e| e.into_inner());
        if q.frames_queued >= FRAME_QUEUE_DEPTH {
            return false;
        }
        q.frames_queued += 1;
        q.msgs.push_back(Msg::Frame(bytes));
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
            self.write_inline(&bytes);
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
    fn write_inline(&self, bytes: &[u8]) {
        let result = write_and_flush(bytes);
        let mut q = self.inner.q.lock().unwrap_or_else(|e| e.into_inner());
        apply_write_result(&mut q, result);
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
            let _ = h.join();
        }
    }
}

fn writer_main(inner: &Inner, waker: &TerminalWaker) {
    loop {
        let msg = {
            let mut q = inner.q.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if let Some(m) = q.msgs.pop_front() {
                    if matches!(m, Msg::Frame(_)) {
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
        let bytes = match &msg {
            Msg::Frame(b) | Msg::Oob(b) => b,
        };
        // After a Fatal, drop writes (the loop is tearing down); keep draining
        // so shutdown never deadlocks.
        let fatal = {
            let q = inner.q.lock().unwrap_or_else(|e| e.into_inner());
            matches!(q.status, WriterStatus::Fatal(_))
        };
        if fatal {
            continue;
        }
        let result = write_and_flush(bytes);
        let errored = result.is_err();
        {
            let mut q = inner.q.lock().unwrap_or_else(|e| e.into_inner());
            apply_write_result(&mut q, result);
        }
        if errored {
            // The loop acts on the status (full repaint / teardown) — wake it.
            let _ = waker.wake();
        }
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
        }
    }

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
