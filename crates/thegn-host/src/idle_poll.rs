//! The loop's poll-timeout decision, as a pure function.
//!
//! The 0%-idle contract (CLAUDE.md): when there is no work in hand the loop
//! blocks on `poll_input(None)` — no tick, no timeout — and wakes only on a
//! terminal event or a `TerminalWaker` pulse. The one sanctioned exception is
//! *busy-time batching*: while work is already queued (a dirty frame, pending
//! input, an exhausted frame budget) the loop polls with a short timeout so a
//! burst of input coalesces before the next flush; and a gate-deferred frame
//! arms its exact remainder so the trailing flush is guaranteed.
//!
//! `render_plan::plan` locks the *render* decision with unit tests; this
//! module locks the *poll* decision the same way, and `just lint` asserts the
//! loop has exactly one timed `poll_input` site, which consumes it.

use std::time::{Duration, Instant};

/// The busy-time batching window.
pub(crate) const BATCH_POLL: Duration = Duration::from_millis(8);

/// `defer` — a gate-deferred frame's remaining wait (wins outright, it is the
/// trailing-flush guarantee). Otherwise [`BATCH_POLL`] iff any work is in
/// hand, else `None`: block until woken.
pub(crate) fn poll_timeout(
    defer: Option<Duration>,
    dirty: bool,
    pending_input: bool,
    budget_exhausted: bool,
) -> Option<Duration> {
    if let Some(remaining) = defer {
        return Some(remaining);
    }
    if dirty || pending_input || budget_exhausted {
        Some(BATCH_POLL)
    } else {
        None
    }
}

/// Fold an absolute app-request deadline into the poll timeout. A missing
/// deadline preserves a true idle block.
pub(crate) fn fold_deadline(
    timeout: Option<Duration>,
    deadline: Option<Instant>,
    now: Instant,
) -> Option<Duration> {
    match (timeout, deadline) {
        (None, None) => None,
        (Some(timeout), None) => Some(timeout),
        (None, Some(deadline)) => Some(deadline.saturating_duration_since(now)),
        (Some(timeout), Some(deadline)) => {
            Some(timeout.min(deadline.saturating_duration_since(now)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_never_polls() {
        assert_eq!(poll_timeout(None, false, false, false), None);
    }

    #[test]
    fn busy_batches_8ms() {
        assert_eq!(poll_timeout(None, true, false, false), Some(BATCH_POLL));
        assert_eq!(poll_timeout(None, false, true, false), Some(BATCH_POLL));
        assert_eq!(poll_timeout(None, false, false, true), Some(BATCH_POLL));
        assert_eq!(BATCH_POLL, Duration::from_millis(8));
    }

    #[test]
    fn log_backlog_keeps_an_otherwise_idle_loop_working() {
        assert_eq!(poll_timeout(None, false, false, true), Some(BATCH_POLL));
    }

    #[test]
    fn fold_none_keeps_idle_none() {
        assert_eq!(fold_deadline(None, None, Instant::now()), None);
    }

    #[test]
    fn fold_takes_the_earlier() {
        let now = Instant::now();
        assert_eq!(
            fold_deadline(
                Some(Duration::from_secs(1)),
                Some(now + Duration::from_millis(200)),
                now
            ),
            Some(Duration::from_millis(200))
        );
        assert_eq!(
            fold_deadline(
                Some(Duration::from_millis(200)),
                Some(now + Duration::from_secs(1)),
                now
            ),
            Some(Duration::from_millis(200))
        );
    }

    #[test]
    fn past_deadline_is_zero() {
        let now = Instant::now();
        assert_eq!(
            fold_deadline(None, Some(now - Duration::from_millis(1)), now),
            Some(Duration::ZERO)
        );
    }

    #[test]
    fn deferred_remainder_wins_over_batch() {
        let d = Duration::from_millis(3);
        assert_eq!(poll_timeout(Some(d), true, true, true), Some(d));
        assert_eq!(poll_timeout(Some(d), false, false, false), Some(d));
    }
}
