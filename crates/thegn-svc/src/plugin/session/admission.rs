//! Bounded, ordered admission. No caller owns or writes an OS pipe.

use std::collections::VecDeque;
use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use thegn_core::plugin_api::{PluginCallback, RpcMessage, RpcResponse};
use tokio::sync::Notify;
use tokio::time::Instant;

use super::{CloseReason, MAX_QUEUED_BYTES, MAX_QUEUED_FRAMES, SessionFailure, SessionOutcome};
use crate::plugin::proc::MAX_LINE_BYTES;

pub(super) type CloseHook = Box<dyn FnOnce(SessionFailure) + Send>;

pub(super) struct State {
    pub queue: VecDeque<Vec<u8>>,
    pub bytes: usize,
    pub closing: Option<(CloseReason, Instant)>,
    pub hooks: Vec<CloseHook>,
    pub response: Option<Arc<dyn Fn(RpcResponse) -> bool + Send + Sync>>,
}

pub(crate) struct Shared {
    pub(super) state: Mutex<State>,
    pub wake: Notify,
    pub outcome: tokio::sync::watch::Sender<Option<SessionOutcome>>,
}

impl Shared {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State {
                queue: VecDeque::new(),
                bytes: 0,
                closing: None,
                hooks: Vec::new(),
                response: None,
            }),
            wake: Notify::new(),
            outcome: tokio::sync::watch::channel(None).0,
        })
    }

    pub fn close(&self, reason: CloseReason, deadline: Instant) {
        let hooks = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((old_reason, old_deadline)) = &mut state.closing {
                *old_deadline = (*old_deadline).min(deadline);
                if reason != CloseReason::WriterClosed {
                    *old_reason = reason;
                }
                if matches!(
                    reason,
                    CloseReason::ProcessExit
                        | CloseReason::StdoutClosed
                        | CloseReason::WriterClosed
                ) {
                    Vec::new()
                } else {
                    std::mem::take(&mut state.hooks)
                }
            } else {
                state.closing = Some((reason, deadline));
                if matches!(reason, CloseReason::ProcessExit | CloseReason::StdoutClosed) {
                    Vec::new()
                } else {
                    std::mem::take(&mut state.hooks)
                }
            }
        };
        // Never call arbitrary code while holding the admission mutex. Pending
        // insertion checks this latch under its own table lock, so it either
        // precedes this drain or observes Closed.
        for hook in hooks {
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                hook(SessionFailure::Closed)
            }))
            .is_err()
            {
                tracing::error!(target: "thegn::plugin", "terminal subscriber panicked");
            }
        }
        self.wake.notify_one();
    }

    pub fn finish_close(&self) {
        let hooks = std::mem::take(&mut self.state.lock().unwrap_or_else(|e| e.into_inner()).hooks);
        for hook in hooks {
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                hook(SessionFailure::Closed)
            }))
            .is_err()
            {
                tracing::error!(target: "thegn::plugin", "terminal subscriber panicked");
            }
        }
    }

    pub fn route_response(&self, response: &RpcResponse) -> bool {
        let route = self
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .response
            .clone();
        route.is_some_and(|route| route(response.clone()))
    }

    pub fn pop(&self) -> Option<Vec<u8>> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let frame = state.queue.pop_front()?;
        state.bytes -= frame.len();
        Some(frame)
    }

    pub fn closing(&self) -> Option<(CloseReason, Instant)> {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).closing
    }

    pub fn discard(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.queue.clear();
        state.bytes = 0;
    }
}

/// Cloneable admission handle. Success means admitted in FIFO order, not that
/// the child has consumed the bytes. A closed transport rejects all new work.
#[derive(Clone)]
pub struct SessionWriter(pub(crate) Arc<Shared>);

impl SessionWriter {
    fn preflight(&self, bytes: usize) -> Result<usize, SessionFailure> {
        let state = self
            .0
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if state.closing.is_some() {
            return Err(SessionFailure::Closed);
        }
        if state.queue.len() >= MAX_QUEUED_FRAMES
            || bytes > MAX_QUEUED_BYTES.saturating_sub(state.bytes)
        {
            return Err(SessionFailure::Full);
        }
        Ok(MAX_QUEUED_BYTES - state.bytes)
    }

    fn admit(&self, frame: Vec<u8>) -> Result<(), SessionFailure> {
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.closing.is_some() {
            return Err(SessionFailure::Closed);
        }
        if state.queue.len() >= MAX_QUEUED_FRAMES
            || frame.len() > MAX_QUEUED_BYTES.saturating_sub(state.bytes)
        {
            return Err(SessionFailure::Full);
        }
        state.bytes += frame.len();
        state.queue.push_back(frame);
        drop(state);
        self.0.wake.notify_one();
        Ok(())
    }

    pub fn is_closed(&self) -> bool {
        self.0.closing().is_some()
    }

    /// A finite number of terminal listeners, invoked once outside all session
    /// locks. Provider bridges use weak ownership in their listener.
    pub(crate) fn bind_provider(
        &self,
        hook: CloseHook,
        response: Arc<dyn Fn(RpcResponse) -> bool + Send + Sync>,
    ) -> Result<(), SessionFailure> {
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.closing.is_some() {
            return Err(SessionFailure::Closed);
        }
        if state.response.is_some() || state.hooks.len() >= MAX_QUEUED_FRAMES {
            return Err(SessionFailure::Full);
        }
        state.hooks.push(hook);
        state.response = Some(response);
        Ok(())
    }

    pub fn notify(
        &self,
        callback: PluginCallback,
        params: serde_json::Value,
    ) -> Result<(), SessionFailure> {
        self.send_json(&RpcMessage::notification(callback, params))
    }

    pub fn send_raw(&self, line: &str) -> Result<(), SessionFailure> {
        if line.len() >= MAX_LINE_BYTES {
            return Err(SessionFailure::TooLarge);
        }
        // Refused traffic should not repeatedly scan/copy a maximum-sized
        // frame. Admission rechecks after validation to cover producer races.
        self.preflight(line.len() + 1)?;
        if line
            .as_bytes()
            .iter()
            .any(|byte| matches!(byte, b'\n' | b'\r'))
        {
            return Err(SessionFailure::TooLarge);
        }
        let mut frame = Vec::with_capacity(line.len() + 1);
        frame.extend_from_slice(line.as_bytes());
        frame.push(b'\n');
        self.admit(frame)
    }

    pub(crate) fn send_json(&self, value: &impl serde::Serialize) -> Result<(), SessionFailure> {
        let remaining = self.preflight(1)?;
        let limit = (remaining - 1).min(MAX_LINE_BYTES - 1);
        if limit == 0 {
            return Err(SessionFailure::Full);
        }
        let mut frame = BoundedFrame(Vec::new(), limit);
        serde_json::to_writer(&mut frame, value).map_err(|_| {
            if limit < MAX_LINE_BYTES - 1 {
                SessionFailure::Full
            } else {
                SessionFailure::TooLarge
            }
        })?;
        self.admit(frame.finish())
    }

    pub fn respond(&self, response: &RpcResponse) -> Result<(), SessionFailure> {
        self.send_json(response)
    }

    /// Close admission immediately, drain already admitted frames against the
    /// existing deadline, then send EOF. Kill/Drop can preempt this drain.
    pub fn close(&self) {
        self.0.close(
            CloseReason::WriterClosed,
            Instant::now() + super::CLOSE_BUDGET,
        );
    }
}

struct BoundedFrame(Vec<u8>, usize);

impl BoundedFrame {
    fn finish(mut self) -> Vec<u8> {
        // Serde may finish at a non-power-of-two capacity. An ordinary push
        // could double that allocation beyond the frame/remaining-byte cap.
        if self.0.len() == self.0.capacity() {
            self.0.reserve_exact(1);
        }
        self.0.push(b'\n');
        self.0
    }
}

impl Write for BoundedFrame {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.1.saturating_sub(self.0.len()) {
            return Err(io::Error::other("resident frame exceeds byte limit"));
        }
        // Vec's geometric growth must not exceed the explicit allocation cap.
        let needed = self.0.len() + bytes.len();
        if needed > self.0.capacity() {
            let capacity = needed
                .max(self.0.capacity().saturating_mul(2))
                .min(self.1 + 1);
            self.0.reserve_exact(capacity - self.0.len());
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resident_admission_is_fifo_count_and_byte_bounded() {
        let shared = Shared::new();
        let writer = SessionWriter(shared.clone());
        for n in 0..MAX_QUEUED_FRAMES {
            writer.send_raw(&n.to_string()).unwrap();
        }
        assert_eq!(writer.send_raw("overflow"), Err(SessionFailure::Full));
        for n in 0..MAX_QUEUED_FRAMES {
            assert_eq!(shared.pop().unwrap(), format!("{n}\n").as_bytes());
        }
        let large = "x".repeat(MAX_LINE_BYTES - 1);
        writer.send_raw(&large).unwrap();
        writer.send_raw(&large).unwrap();
        assert_eq!(writer.send_raw("x"), Err(SessionFailure::Full));
        assert_eq!(shared.state.lock().unwrap().bytes, MAX_QUEUED_BYTES);
        writer.close();
        assert_eq!(writer.send_raw("late"), Err(SessionFailure::Closed));
    }

    #[test]
    fn resident_nearly_full_queue_caps_json_work_before_serialization() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct CountSerialize<'a>(&'a AtomicUsize);
        impl serde::Serialize for CountSerialize<'_> {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                self.0.fetch_add(1, Ordering::Relaxed);
                serializer.serialize_str("must not be visited")
            }
        }
        let shared = Shared::new();
        let writer = SessionWriter(shared.clone());
        writer.send_raw(&"x".repeat(MAX_LINE_BYTES - 1)).unwrap();
        writer.send_raw(&"x".repeat(MAX_LINE_BYTES - 2)).unwrap();
        assert_eq!(shared.state.lock().unwrap().bytes, MAX_QUEUED_BYTES - 1);
        let calls = AtomicUsize::new(0);
        assert_eq!(
            writer.send_json(&CountSerialize(&calls)),
            Err(SessionFailure::Full)
        );
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        // Capacity refusal precedes even invalid-line scanning.
        assert_eq!(writer.send_raw("\n"), Err(SessionFailure::Full));
        let mut small = BoundedFrame(Vec::new(), 127);
        assert!(serde_json::to_writer(&mut small, &"x".repeat(MAX_LINE_BYTES)).is_err());
        assert!(small.0.len() <= 127 && small.0.capacity() <= 128);
    }

    #[test]
    fn resident_final_newline_does_not_double_large_json_allocation() {
        let value = serde_json::json!({"id":1_000_000,"method":"provider.call",
            "params":{"a":"x".repeat(300_000),"b":"y".repeat(300_044)}});
        let mut frame = BoundedFrame(Vec::new(), MAX_LINE_BYTES - 1);
        serde_json::to_writer(&mut frame, &value).unwrap();
        assert_eq!(frame.0.len(), 600_108);
        assert_eq!(
            frame.0.capacity(),
            frame.0.len(),
            "fixture must hit delimiter growth boundary"
        );
        let wire = frame.finish();
        assert_eq!(wire.len(), 600_109);
        assert!(wire.capacity() <= MAX_LINE_BYTES);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&wire).unwrap(),
            value
        );
        let shared = Shared::new();
        SessionWriter(shared.clone()).send_json(&value).unwrap();
        assert!(shared.pop().unwrap().capacity() <= MAX_LINE_BYTES);
    }

    #[test]
    fn resident_serialization_bounds_before_growth_and_preserves_json() {
        let shared = Shared::new();
        let writer = SessionWriter(shared.clone());
        writer
            .send_json(&serde_json::json!({"line":"a\nb"}))
            .unwrap();
        assert_eq!(shared.pop().unwrap(), b"{\"line\":\"a\\nb\"}\n");
        assert_eq!(
            writer.send_json(&"x".repeat(MAX_LINE_BYTES)),
            Err(SessionFailure::TooLarge)
        );
        assert_eq!(writer.send_raw("a\nb"), Err(SessionFailure::TooLarge));
        let mut frame = BoundedFrame(Vec::new(), MAX_LINE_BYTES - 1);
        for _ in 0..MAX_LINE_BYTES - 1 {
            frame.write_all(b"x").unwrap();
        }
        assert!(frame.write_all(b"x").is_err());
        assert!(frame.0.capacity() <= MAX_LINE_BYTES);
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn resident_natural_close_escalation_drains_pending_hooks_once() {
        let shared = Shared::new();
        let writer = SessionWriter(shared.clone());
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        writer
            .bind_provider(
                Box::new(move |_| {
                    observed.fetch_add(1, Ordering::Relaxed);
                }),
                Arc::new(|_| false),
            )
            .unwrap();
        let deadline = Instant::now() + super::super::CLOSE_BUDGET;
        shared.close(CloseReason::ProcessExit, deadline);
        assert!(writer.is_closed());
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        shared.close(CloseReason::Shutdown, deadline);
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        shared.finish_close();
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn resident_subscriber_panic_cannot_abort_closure_or_other_subscribers() {
        let shared = Shared::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        {
            let mut state = shared.state.lock().unwrap();
            state.hooks.push(Box::new(|_| panic!("fixture subscriber")));
            state.hooks.push(Box::new(move |_| {
                observed.fetch_add(1, Ordering::Relaxed);
            }));
        }
        shared.close(CloseReason::Requested, Instant::now());
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert!(SessionWriter(shared).is_closed());
    }
}
