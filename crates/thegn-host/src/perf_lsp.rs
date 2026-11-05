//! Surfaces the LSP diagnostics bus queue statistics in the `thegn::perf` rollup.

use thegn_svc::lsp::{BacklogAge, DiagnosticsReceiver, QueueStats};

/// The tracing-shaped view of one observation. `observed == false` means the bus could not be
/// read (poisoned mutex): every other field is zero and MUST NOT be read as an empty queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LspQueueView {
    pub observed: bool,
    pub admitted: u64,
    pub replaced: u64,
    pub evicted: u64,
    pub rejected_capacity: u64,
    pub stale_rejected: u64,
    pub purged_on_retire: u64,
    pub delivered: u64,
    pub pending_documents: u64,
    pub pending_bytes: u64,
    pub ready_roots: u64,
    pub active_streams: u64,
    /// "unobserved" | "empty" | "oldest"; "unobserved" also when `observed` is false.
    pub backlog: &'static str,
    /// Oldest pending age in milliseconds, 0 unless `backlog == "oldest"`.
    pub oldest_ms: u64,
}

/// Projects the optional bus snapshot into bounded tracing fields.
pub(crate) fn view(stats: Option<QueueStats>) -> LspQueueView {
    let Some(stats) = stats else {
        return LspQueueView {
            observed: false,
            admitted: 0,
            replaced: 0,
            evicted: 0,
            rejected_capacity: 0,
            stale_rejected: 0,
            purged_on_retire: 0,
            delivered: 0,
            pending_documents: 0,
            pending_bytes: 0,
            ready_roots: 0,
            active_streams: 0,
            backlog: "unobserved",
            oldest_ms: 0,
        };
    };

    let (backlog, oldest_ms) = match stats.backlog_age {
        BacklogAge::Unobserved => ("unobserved", 0),
        BacklogAge::Empty => ("empty", 0),
        BacklogAge::Oldest(age) => ("oldest", u64::try_from(age.as_millis()).unwrap_or(u64::MAX)),
    };
    let to_u64 = |value: usize| u64::try_from(value).unwrap_or(u64::MAX);
    LspQueueView {
        observed: true,
        admitted: stats.counters.admitted,
        replaced: stats.counters.replaced,
        evicted: stats.counters.evicted,
        rejected_capacity: stats.counters.rejected_capacity,
        stale_rejected: stats.counters.stale_rejected,
        purged_on_retire: stats.counters.purged_on_retire,
        delivered: stats.counters.delivered,
        pending_documents: to_u64(stats.pending_documents),
        pending_bytes: to_u64(stats.pending_bytes),
        ready_roots: to_u64(stats.ready_roots),
        active_streams: to_u64(stats.active_streams),
        backlog,
        oldest_ms,
    }
}

/// Emits one cumulative LSP queue observation in the existing perf rollup stream.
pub(crate) fn emit(rx: &DiagnosticsReceiver) {
    let v = view(rx.queue_stats());
    tracing::info!(
        target: "thegn::perf",
        lsp_queue_observed = v.observed,
        lsp_queue_admitted = v.admitted,
        lsp_queue_replaced = v.replaced,
        lsp_queue_evicted = v.evicted,
        lsp_queue_rejected_capacity = v.rejected_capacity,
        lsp_queue_stale_rejected = v.stale_rejected,
        lsp_queue_purged_on_retire = v.purged_on_retire,
        lsp_queue_delivered = v.delivered,
        lsp_queue_pending_documents = v.pending_documents,
        lsp_queue_pending_bytes = v.pending_bytes,
        lsp_queue_ready_roots = v.ready_roots,
        lsp_queue_active_streams = v.active_streams,
        lsp_queue_backlog = v.backlog,
        lsp_queue_oldest_ms = v.oldest_ms,
        "lsp queue"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use thegn_svc::lsp::QueueCounters;

    #[test]
    fn view_of_none_is_unobserved_not_empty() {
        let v = view(None);

        assert!(!v.observed);
        assert_eq!(v.backlog, "unobserved");
        assert_eq!(v.oldest_ms, 0);
    }

    #[test]
    fn view_maps_counters_and_gauges_exactly() {
        let stats = QueueStats {
            counters: QueueCounters {
                admitted: 7,
                replaced: 6,
                evicted: 5,
                rejected_capacity: 4,
                stale_rejected: 3,
                purged_on_retire: 2,
                delivered: 1,
            },
            pending_documents: 9,
            pending_bytes: 8192,
            ready_roots: 3,
            active_streams: 2,
            backlog_age: BacklogAge::Empty,
        };
        let v = view(Some(stats));

        assert!(v.observed);
        assert_eq!(v.admitted, 7);
        assert_eq!(v.replaced, 6);
        assert_eq!(v.evicted, 5);
        assert_eq!(v.rejected_capacity, 4);
        assert_eq!(v.stale_rejected, 3);
        assert_eq!(v.purged_on_retire, 2);
        assert_eq!(v.delivered, 1);
        assert_eq!(v.pending_documents, 9);
        assert_eq!(v.pending_bytes, 8192);
        assert_eq!(v.ready_roots, 3);
        assert_eq!(v.active_streams, 2);
        assert_eq!(v.backlog, "empty");
        assert_eq!(v.oldest_ms, 0);
    }

    #[test]
    fn view_of_oldest_reports_milliseconds() {
        let stats = QueueStats {
            backlog_age: BacklogAge::Oldest(Duration::from_millis(1500)),
            ..QueueStats {
                counters: QueueCounters::default(),
                pending_documents: 0,
                pending_bytes: 0,
                ready_roots: 0,
                active_streams: 0,
                backlog_age: BacklogAge::Empty,
            }
        };
        let v = view(Some(stats));
        assert_eq!(v.backlog, "oldest");
        assert_eq!(v.oldest_ms, 1500);

        let sub_millisecond = QueueStats {
            backlog_age: BacklogAge::Oldest(Duration::from_micros(900)),
            ..stats
        };
        let v = view(Some(sub_millisecond));
        assert_eq!(v.backlog, "oldest");
        assert_eq!(v.oldest_ms, 0);
    }

    #[test]
    fn view_of_unobserved_age_keeps_gauges() {
        let stats = QueueStats {
            pending_documents: 4,
            backlog_age: BacklogAge::Unobserved,
            ..QueueStats {
                counters: QueueCounters::default(),
                pending_documents: 0,
                pending_bytes: 0,
                ready_roots: 0,
                active_streams: 0,
                backlog_age: BacklogAge::Empty,
            }
        };
        let v = view(Some(stats));

        assert_eq!(v.backlog, "unobserved");
        assert!(v.observed);
        assert_eq!(v.pending_documents, 4);
    }

    #[test]
    fn view_saturates_oversized_gauges() {
        let stats = QueueStats {
            pending_bytes: usize::MAX,
            ..QueueStats {
                counters: QueueCounters::default(),
                pending_documents: 0,
                pending_bytes: 0,
                ready_roots: 0,
                active_streams: 0,
                backlog_age: BacklogAge::Empty,
            }
        };

        assert_eq!(
            view(Some(stats)).pending_bytes,
            u64::try_from(usize::MAX).unwrap_or(u64::MAX)
        );
    }

    #[test]
    fn emit_reads_a_live_bus() {
        let (_tx, rx) = thegn_svc::lsp::diagnostics_channel();
        rx.set_timing(true);

        emit(&rx);

        let v = view(rx.queue_stats());
        assert!(v.observed);
        assert_eq!(v.backlog, "empty");
        assert_eq!(v.admitted, 0);
        assert_eq!(v.replaced, 0);
        assert_eq!(v.evicted, 0);
        assert_eq!(v.rejected_capacity, 0);
        assert_eq!(v.stale_rejected, 0);
        assert_eq!(v.purged_on_retire, 0);
        assert_eq!(v.delivered, 0);
    }
}
