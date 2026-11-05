//! Surfaces the log ingress queue counters in the `thegn::perf` rollup.

use thegn_svc::log::ingress::IngressStats;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LogIngressView {
    pub pending: u64,
    pub pending_bytes: u64,
    pub pushed: u64,
    pub taken: u64,
    pub dropped: u64,
    pub gap_markers: u64,
    /// False when the queue is empty: no age exists, which is not the same as an age of 0.
    pub oldest_known: bool,
    pub oldest_ms: u64,
}

/// Projects an ingress snapshot into bounded tracing fields.
pub(crate) fn view(stats: IngressStats) -> LogIngressView {
    let to_u64 = |value: usize| u64::try_from(value).unwrap_or(u64::MAX);
    let (oldest_known, oldest_ms) = match stats.oldest_age {
        Some(age) => (true, u64::try_from(age.as_millis()).unwrap_or(u64::MAX)),
        None => (false, 0),
    };

    LogIngressView {
        pending: to_u64(stats.pending),
        pending_bytes: to_u64(stats.pending_bytes),
        pushed: stats.pushed_total,
        taken: stats.taken_total,
        dropped: stats.dropped_total,
        gap_markers: stats.gap_markers_total,
        oldest_known,
        oldest_ms,
    }
}

/// Emits one log ingress observation in the existing perf rollup stream.
pub(crate) fn emit(stats: IngressStats) {
    let v = view(stats);
    tracing::info!(
        target: "thegn::perf",
        log_ingress_pending = v.pending,
        log_ingress_pending_bytes = v.pending_bytes,
        log_ingress_pushed = v.pushed,
        log_ingress_taken = v.taken,
        log_ingress_dropped = v.dropped,
        log_ingress_gap_markers = v.gap_markers,
        log_ingress_oldest_known = v.oldest_known,
        log_ingress_oldest_ms = v.oldest_ms,
        "log ingress"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn view_maps_every_field() {
        let stats = IngressStats {
            pending: 7,
            pending_bytes: 8192,
            dropped_total: 5,
            pushed_total: 12,
            taken_total: 4,
            gap_markers_total: 3,
            oldest_age: Some(Duration::from_millis(1500)),
        };

        assert_eq!(
            view(stats),
            LogIngressView {
                pending: 7,
                pending_bytes: 8192,
                pushed: 12,
                taken: 4,
                dropped: 5,
                gap_markers: 3,
                oldest_known: true,
                oldest_ms: 1500,
            }
        );
    }

    #[test]
    fn view_of_empty_queue_reports_age_unknown() {
        let stats = IngressStats {
            pending: 0,
            pending_bytes: 0,
            dropped_total: 0,
            pushed_total: 0,
            taken_total: 0,
            gap_markers_total: 0,
            oldest_age: None,
        };
        let v = view(stats);

        assert!(!v.oldest_known);
        assert_eq!(v.oldest_ms, 0);
    }

    #[test]
    fn view_of_oldest_reports_milliseconds() {
        let stats = IngressStats {
            pending: 1,
            pending_bytes: 128,
            dropped_total: 0,
            pushed_total: 1,
            taken_total: 0,
            gap_markers_total: 0,
            oldest_age: Some(Duration::from_millis(1500)),
        };
        let v = view(stats);
        assert!(v.oldest_known);
        assert_eq!(v.oldest_ms, 1500);

        let sub_millisecond = IngressStats {
            oldest_age: Some(Duration::from_micros(900)),
            ..stats
        };
        let v = view(sub_millisecond);
        assert!(v.oldest_known);
        assert_eq!(v.oldest_ms, 0);
    }

    #[test]
    fn view_saturates_oversized_gauges() {
        let stats = IngressStats {
            pending: usize::MAX,
            pending_bytes: usize::MAX,
            dropped_total: 0,
            pushed_total: 0,
            taken_total: 0,
            gap_markers_total: 0,
            oldest_age: None,
        };
        let v = view(stats);

        assert_eq!(v.pending, u64::try_from(usize::MAX).unwrap_or(u64::MAX));
        assert_eq!(
            v.pending_bytes,
            u64::try_from(usize::MAX).unwrap_or(u64::MAX)
        );
    }

    #[test]
    fn emit_does_not_panic_without_a_subscriber() {
        emit(IngressStats {
            pending: 0,
            pending_bytes: 0,
            dropped_total: 0,
            pushed_total: 0,
            taken_total: 0,
            gap_markers_total: 0,
            oldest_age: None,
        });
    }
}
