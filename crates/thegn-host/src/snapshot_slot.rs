//! Latest-value storage for replaceable host-loop snapshots.
//!
//! Producers provide a generation and an already-measured byte size.  The
//! slot keeps at most one accepted value, rejects older generations, and
//! invokes its wake callback only when it changes from empty to full.  The
//! callback is deliberately generic so this module has no terminal or async
//! runtime dependency.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// The result of attempting to publish one snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PublishOutcome {
    /// The slot was empty and now contains the value.
    Stored,
    /// The value replaced an accepted value.
    Replaced,
    /// A newer generation is already stored, or the slot is closed.
    StaleRejected,
    /// The supplied byte count exceeds the slot's cap.
    TooLarge,
}

struct State<T> {
    value: Option<(u64, T)>,
    last_generation: Option<u64>,
    closed: bool,
    replacements: u64,
    peak_bytes: usize,
}

struct Inner<T> {
    state: Mutex<State<T>>,
    max_bytes: usize,
    wake_pending: AtomicBool,
    wake: Box<dyn Fn() + Send + Sync>,
}

/// A thread-safe, latest-value slot for replaceable snapshots.
pub(crate) struct SnapshotSlot<T> {
    inner: Arc<Inner<T>>,
}

impl<T> Clone for SnapshotSlot<T> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<T> SnapshotSlot<T> {
    /// Create a slot with a no-op wake callback.
    #[cfg(test)]
    fn new(max_bytes: usize) -> Self {
        Self::with_waker(max_bytes, || {})
    }

    /// Create a slot whose callback is invoked for each empty-to-full change.
    pub(crate) fn with_waker(max_bytes: usize, wake: impl Fn() + Send + Sync + 'static) -> Self {
        Self {
            inner: Arc::new(Inner {
                state: Mutex::new(State {
                    value: None,
                    last_generation: None,
                    closed: false,
                    replacements: 0,
                    peak_bytes: 0,
                }),
                max_bytes,
                wake_pending: AtomicBool::new(false),
                wake: Box::new(wake),
            }),
        }
    }

    /// Publish a value, retaining only the newest accepted value.
    pub(crate) fn publish(&self, generation: u64, value: T, bytes: usize) -> PublishOutcome {
        let (outcome, pulse) = {
            let mut state = self
                .inner
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if state.closed || bytes > self.inner.max_bytes {
                return if state.closed {
                    PublishOutcome::StaleRejected
                } else {
                    PublishOutcome::TooLarge
                };
            }
            if state
                .last_generation
                .is_some_and(|current_generation| generation < current_generation)
            {
                return PublishOutcome::StaleRejected;
            }

            let outcome = if state.value.is_some() {
                state.replacements = state.replacements.saturating_add(1);
                PublishOutcome::Replaced
            } else {
                PublishOutcome::Stored
            };
            state.value = Some((generation, value));
            state.last_generation = Some(generation);
            state.peak_bytes = state.peak_bytes.max(bytes);
            let pulse = matches!(outcome, PublishOutcome::Stored)
                && !self.inner.wake_pending.swap(true, Ordering::AcqRel);
            (outcome, pulse)
        };
        if pulse {
            (self.inner.wake)();
        }
        outcome
    }

    /// Take the current value.  A concurrent publish waits for this atomic
    /// slot operation and remains available to the next take.
    pub(crate) fn take(&self) -> Option<(u64, T)> {
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let value = state.value.take();
        if value.is_some() {
            self.inner.wake_pending.store(false, Ordering::Release);
        }
        value
    }

    /// Close the slot and discard its pending value.  Publishing while closed
    /// is deterministic (`StaleRejected`) until [`Self::reopen`] is called.
    #[cfg(test)]
    pub(crate) fn close(&self) {
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.value = None;
        state.last_generation = None;
        state.closed = true;
        self.inner.wake_pending.store(false, Ordering::Release);
    }

    /// Reopen a closed slot empty, resetting its pending-wake state.
    #[cfg(test)]
    pub(crate) fn reopen(&self) {
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.closed = false;
        state.value = None;
        state.last_generation = None;
        self.inner.wake_pending.store(false, Ordering::Release);
    }

    #[cfg(test)]
    pub(crate) fn replacements(&self) -> u64 {
        self.inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .replacements
    }

    #[cfg(test)]
    pub(crate) fn peak_bytes(&self) -> usize {
        self.inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .peak_bytes
    }
}

#[cfg(test)]
mod tests {
    use super::{PublishOutcome, SnapshotSlot};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn slot<T>() -> SnapshotSlot<T> {
        SnapshotSlot::new(64)
    }

    #[test]
    fn slow_consumer_retains_exactly_one_value() {
        let slot = slot();
        assert_eq!(slot.publish(1, "first", 5), PublishOutcome::Stored);
        assert_eq!(slot.publish(2, "latest", 6), PublishOutcome::Replaced);
        assert_eq!(slot.take(), Some((2, "latest")));
        assert_eq!(slot.take(), None);
    }

    #[test]
    fn daemon_status_drain_uses_latest_value() {
        let slot = SnapshotSlot::new(64 * 1024);
        let mut first = crate::chrome::DaemonStatus::default();
        first.hostname = "old-host".to_owned();
        let mut latest = crate::chrome::DaemonStatus::default();
        latest.hostname = "new-host".to_owned();
        assert_eq!(slot.publish(0, first, 128), PublishOutcome::Stored);
        assert_eq!(slot.publish(0, latest, 128), PublishOutcome::Replaced);
        let (_, status) = slot.take().expect("daemon status snapshot");
        assert_eq!(status.hostname, "new-host");
    }

    #[test]
    fn stale_generation_is_rejected() {
        let slot = slot();
        assert_eq!(slot.publish(9, "new", 3), PublishOutcome::Stored);
        assert_eq!(slot.publish(8, "old", 3), PublishOutcome::StaleRejected);
        assert_eq!(slot.take(), Some((9, "new")));
        assert_eq!(
            slot.publish(8, "late-old", 8),
            PublishOutcome::StaleRejected
        );
        assert_eq!(slot.take(), None);
    }

    #[test]
    fn replacement_during_take_preserves_newer_value() {
        let slot = slot();
        assert_eq!(slot.publish(1, "old", 3), PublishOutcome::Stored);
        assert_eq!(slot.take(), Some((1, "old")));
        assert_eq!(slot.publish(2, "new", 3), PublishOutcome::Stored);
        assert_eq!(slot.take(), Some((2, "new")));
    }

    #[test]
    fn close_then_reopen_is_deterministic() {
        let slot = slot();
        assert_eq!(slot.publish(1, "before", 6), PublishOutcome::Stored);
        slot.close();
        assert_eq!(slot.take(), None);
        assert_eq!(slot.publish(2, "closed", 6), PublishOutcome::StaleRejected);
        slot.reopen();
        assert_eq!(slot.publish(3, "after", 5), PublishOutcome::Stored);
        assert_eq!(slot.take(), Some((3, "after")));
    }

    #[test]
    fn byte_cap_refuses_with_typed_outcome() {
        let slot = slot();
        assert_eq!(slot.publish(1, "kept", 4), PublishOutcome::Stored);
        assert_eq!(slot.publish(2, "rejected", 65), PublishOutcome::TooLarge);
        assert_eq!(slot.take(), Some((1, "kept")));
    }

    #[test]
    fn wake_pulses_once_per_empty_to_full_transition() {
        let wakes = Arc::new(AtomicUsize::new(0));
        let callback_wakes = Arc::clone(&wakes);
        let slot = SnapshotSlot::with_waker(64, move || {
            callback_wakes.fetch_add(1, Ordering::Relaxed);
        });
        assert_eq!(slot.publish(1, 1, 1), PublishOutcome::Stored);
        assert_eq!(slot.publish(2, 2, 1), PublishOutcome::Replaced);
        assert_eq!(wakes.load(Ordering::Relaxed), 1);
        assert_eq!(slot.take(), Some((2, 2)));
        assert_eq!(slot.publish(3, 3, 1), PublishOutcome::Stored);
        assert_eq!(wakes.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn many_publishes_keep_one_slot_and_bounded_peak_bytes() {
        let slot = slot();
        for generation in 0..1_000 {
            assert!(matches!(
                slot.publish(generation, vec![0_u8; 32], 32),
                PublishOutcome::Stored | PublishOutcome::Replaced
            ));
        }
        assert_eq!(slot.replacements(), 999);
        assert_eq!(slot.peak_bytes(), 32);
        assert_eq!(
            slot.take()
                .map(|(generation, value)| (generation, value.len())),
            Some((999, 32))
        );
        assert_eq!(slot.take(), None);
    }
}
