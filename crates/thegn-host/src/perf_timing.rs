//! Measurement boundaries shared by the loop and terminal writer. These clocks
//! are passed explicitly so queueing, failure and early-continue behavior can be
//! verified without touching a terminal or sleeping.

use crate::perf::Histo;
use std::time::{Duration, Instant};

#[derive(Default)]
pub(crate) struct ActiveClock(Option<Instant>);

impl ActiveClock {
    /// Charge active work since the previous boundary, including dispatch at
    /// the end of a previous loop iteration. Calling twice cannot double count.
    pub(crate) fn checkpoint(&mut self, now: Instant) -> Duration {
        self.0
            .replace(now)
            .map_or(Duration::ZERO, |then| now.saturating_duration_since(then))
    }

    /// Restart after the *blocking* poll, excluding only that wait.
    pub(crate) fn resume(&mut self, now: Instant) {
        self.0 = Some(now);
    }

    pub(crate) fn pause(&mut self) {
        self.0 = None;
    }
}

/// Retain the earliest host observation in a coalesced input cohort. This is
/// not a hardware timestamp or a promise that the next frame shows its effect.
pub(crate) fn observe_input(pending: &mut Option<Instant>, now: Instant) {
    pending.get_or_insert(now);
}

#[derive(Clone, Copy)]
pub(crate) struct FrameStamp {
    pub queued_at: Instant,
    pub input_at: Option<Instant>,
}

impl FrameStamp {
    pub(crate) fn capture(input_at: Option<Instant>) -> Option<Self> {
        crate::perf::enabled().then(|| Self {
            queued_at: Instant::now(),
            input_at,
        })
    }
}

/// Fixed-size aggregates, recorded only after a frame's sink write returns.
/// OOB output does not enter this ledger. No completion queue or timer is used.
#[derive(Clone, Debug, Default)]
pub(crate) struct WriterMetrics {
    pub queue_us: Histo,
    pub write_us: Histo,
    pub completion_us: Histo,
    pub input_completion_us: Histo,
    pub failed_frames: u64,
    /// Successful collection includes samples retained across this many missed rollups.
    pub deferred_rollups: u64,
}

impl WriterMetrics {
    pub(crate) fn record(
        &mut self,
        stamp: FrameStamp,
        started: Instant,
        finished: Instant,
        success: bool,
    ) {
        if !success {
            self.failed_frames += 1;
            return;
        }
        self.queue_us.record_us(
            started
                .saturating_duration_since(stamp.queued_at)
                .as_micros() as u64,
        );
        self.write_us
            .record_us(finished.saturating_duration_since(started).as_micros() as u64);
        self.completion_us.record_us(
            finished
                .saturating_duration_since(stamp.queued_at)
                .as_micros() as u64,
        );
        if let Some(input) = stamp.input_at {
            self.input_completion_us
                .record_us(finished.saturating_duration_since(input).as_micros() as u64);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatch_and_early_continue_are_active_but_poll_wait_is_not() {
        let t = Instant::now();
        let mut clock = ActiveClock::default();
        assert_eq!(clock.checkpoint(t), Duration::ZERO);
        assert_eq!(
            clock.checkpoint(t + Duration::from_millis(3)),
            Duration::from_millis(3)
        );
        clock.resume(t + Duration::from_millis(100)); // blocked 97ms
        assert_eq!(
            clock.checkpoint(t + Duration::from_millis(125)),
            Duration::from_millis(25)
        );
        // An early continue/queued event resets nothing: its work survives.
        assert_eq!(
            clock.checkpoint(t + Duration::from_millis(140)),
            Duration::from_millis(15)
        );
        assert_eq!(
            clock.checkpoint(t + Duration::from_millis(140)),
            Duration::ZERO
        );
        clock.pause();
        assert_eq!(clock.checkpoint(t + Duration::from_secs(2)), Duration::ZERO);
    }

    #[test]
    fn preemption_and_later_dispatch_preserve_the_earliest_stamp() {
        let t = Instant::now();
        let mut pending = None;
        observe_input(&mut pending, t);
        observe_input(&mut pending, t + Duration::from_millis(200));
        assert_eq!(pending.take(), Some(t));
        observe_input(&mut pending, t + Duration::from_secs(1));
        assert_eq!(pending, Some(t + Duration::from_secs(1)));
    }

    #[test]
    fn rollup_boundary_charges_publication_and_queued_dispatch_to_next_interval() {
        let t = Instant::now();
        let mut clock = ActiveClock::default();
        clock.resume(t);
        // Both the rollup interval and active accounting reset at this instant.
        assert_eq!(
            clock.checkpoint(t + Duration::from_millis(10)),
            Duration::from_millis(10)
        );
        // Publishing the snapshot takes 4ms, then a queued event takes 8ms.
        // Neither operation performs a blocking poll, so no resume is allowed.
        assert_eq!(
            clock.checkpoint(t + Duration::from_millis(22)),
            Duration::from_millis(12)
        );
        // Timeout selection and queue inspection add 2ms before the real poll.
        assert_eq!(
            clock.checkpoint(t + Duration::from_millis(24)),
            Duration::from_millis(2)
        );
        clock.resume(t + Duration::from_millis(100));
        assert_eq!(
            clock.checkpoint(t + Duration::from_millis(105)),
            Duration::from_millis(5)
        );
    }

    #[test]
    fn writer_separates_queue_write_and_input_completion_and_excludes_failure() {
        let t = Instant::now();
        let stamp = FrameStamp {
            queued_at: t + Duration::from_millis(10),
            input_at: Some(t),
        };
        let mut metrics = WriterMetrics::default();
        metrics.record(
            stamp,
            t + Duration::from_millis(30),
            t + Duration::from_millis(70),
            true,
        );
        assert_eq!(metrics.queue_us.percentile_us(0.99), 16_384);
        assert_eq!(metrics.write_us.percentile_us(0.99), 32_768);
        assert_eq!(metrics.completion_us.percentile_us(0.99), 32_768);
        assert_eq!(metrics.input_completion_us.percentile_us(0.99), 65_536);
        metrics.record(stamp, t, t + Duration::from_secs(9), false);
        assert_eq!(metrics.failed_frames, 1);
        assert_eq!(metrics.completion_us.count(), 1);
        assert_eq!(metrics.input_completion_us.count(), 1);
        let drained = std::mem::take(&mut metrics);
        assert_eq!(drained.completion_us.count(), 1);
        assert_eq!(metrics.completion_us.count(), 0);
    }

    #[test]
    fn synchronous_frame_has_zero_queue_time_and_no_fabricated_input() {
        let t = Instant::now();
        let mut metrics = WriterMetrics::default();
        metrics.record(
            FrameStamp {
                queued_at: t,
                input_at: None,
            },
            t,
            t + Duration::from_millis(2),
            true,
        );
        assert_eq!(metrics.queue_us.count(), 1);
        assert_eq!(metrics.queue_us.percentile_us(0.99), 0);
        assert_eq!(metrics.input_completion_us.count(), 0);
    }
}
