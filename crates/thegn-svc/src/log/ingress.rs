//! Bounded, drop-oldest ingress between the log tailer and host loop. The
//! producer never waits for the viewer; saturation evicts oldest records and
//! reports the gap before the retained tail.
//!
//! Limits across this path: `FileFollower` holds at most a 1 MiB partial raw
//! record and reads at most 64 KiB per poll; the provider clamps each parsed
//! input line to 8 KiB and pushes at most 64 records / 64 KiB estimated parsed
//! payload per batch. This queue retains at most 2,000 records / 1 MiB of
//! estimated parsed strings plus record overhead. The host retains 1,000
//! display rows. The parser estimate counts original, message, timestamp, and
//! worktree strings; queue and transient batches are separate bounds.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use thegn_core::log::parser::{LogLevel, ParsedLog};

/// Aggregate pending-record bound.
pub const MAX_PENDING_RECORDS: usize = 2000;
/// Aggregate pending payload estimate, including per-record allocation overhead.
pub const MAX_PENDING_BYTES: usize = 1024 * 1024;
/// Per-record cap applied before parsing a line from the follower.
pub const MAX_RECORD_BYTES: usize = 8 * 1024;
/// Maximum parsed records in a provider's transient push batch.
pub const MAX_BATCH_RECORDS: usize = 64;
/// Maximum estimated parsed payload bytes in a provider's transient push batch.
pub const MAX_BATCH_BYTES: usize = 64 * 1024;
const RECORD_OVERHEAD: usize = 128;

struct Entry {
    at: Instant,
    cost: usize,
    log: ParsedLog,
}

#[derive(Default)]
struct State {
    queue: VecDeque<Entry>,
    bytes: usize,
    gap: u64,
    dropped_total: u64,
}

/// Snapshot of bounded queue health for diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IngressStats {
    pub pending: usize,
    pub pending_bytes: usize,
    pub dropped_total: u64,
    pub oldest_age: Option<Duration>,
}

/// Cloneable producer/consumer handle for one log stream.
#[derive(Clone, Default)]
pub struct LogIngress {
    inner: Arc<Mutex<State>>,
}

pub(crate) fn parsed_cost(log: &ParsedLog) -> usize {
    RECORD_OVERHEAD
        + log.original.len()
        + log.message.len()
        + log.timestamp.len()
        + log.worktree.as_ref().map_or(0, String::len)
}

/// Truncate a raw line on a UTF-8 boundary and append an explicit marker.
pub fn clamp_record(line: &mut String) {
    if line.len() > MAX_RECORD_BYTES {
        let mut cut = MAX_RECORD_BYTES;
        while !line.is_char_boundary(cut) {
            cut -= 1;
        }
        line.truncate(cut);
        line.push_str(" [record truncated]");
    }
}

impl LogIngress {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Whether this is the only remaining handle (the host consumer is gone).
    pub fn is_orphaned(&self) -> bool {
        Arc::strong_count(&self.inner) == 1
    }

    /// Enqueue records, evicting oldest entries as needed. Returns true when
    /// the queue was empty before this call, so the producer should wake host.
    pub fn push(&self, batch: Vec<ParsedLog>) -> bool {
        let now = Instant::now();
        let mut state = self.lock();
        let was_empty = state.queue.is_empty();
        for log in batch {
            let cost = parsed_cost(&log);
            state.bytes = state.bytes.saturating_add(cost);
            state.queue.push_back(Entry { at: now, cost, log });
            while state.queue.len() > MAX_PENDING_RECORDS || state.bytes > MAX_PENDING_BYTES {
                let Some(old) = state.queue.pop_front() else {
                    break;
                };
                state.bytes -= old.cost;
                state.gap = state.gap.saturating_add(1);
                state.dropped_total = state.dropped_total.saturating_add(1);
            }
        }
        was_empty
    }

    /// Take at most `max_records` in source order, preceded by a gap marker.
    /// The marker is metadata and does not count against the record budget.
    pub fn take(&self, max_records: usize) -> Vec<ParsedLog> {
        let mut state = self.lock();
        let mut out = Vec::with_capacity(max_records.min(state.queue.len()) + 1);
        if state.gap > 0 {
            out.push(gap_marker(state.gap));
            state.gap = 0;
        }
        for _ in 0..max_records {
            let Some(entry) = state.queue.pop_front() else {
                break;
            };
            state.bytes -= entry.cost;
            out.push(entry.log);
        }
        out
    }

    /// True when records or an unreported gap remain.
    pub fn has_pending(&self) -> bool {
        let state = self.lock();
        !state.queue.is_empty() || state.gap > 0
    }

    pub fn stats(&self) -> IngressStats {
        let state = self.lock();
        IngressStats {
            pending: state.queue.len(),
            pending_bytes: state.bytes,
            dropped_total: state.dropped_total,
            oldest_age: state.queue.front().map(|entry| entry.at.elapsed()),
        }
    }
}

fn gap_marker(count: u64) -> ParsedLog {
    let message = format!("[{count} log lines dropped under overload]");
    ParsedLog {
        timestamp: String::new(),
        level: LogLevel::Warn,
        original: message.clone(),
        message,
        worktree: None,
        target: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(message: &str) -> ParsedLog {
        thegn_core::log::parser::parse_log(message)
    }

    #[test]
    fn saturation_reports_gap_and_retains_ordered_tail() {
        let queue = LogIngress::new();
        let count = MAX_PENDING_RECORDS + 50;
        for index in 0..count {
            queue.push(vec![rec(&format!("line-{index}"))]);
        }
        let stats = queue.stats();
        assert_eq!(stats.pending, MAX_PENDING_RECORDS);
        assert_eq!(stats.dropped_total, 50);
        let out = queue.take(usize::MAX);
        assert!(out[0].message.contains("50 log lines dropped"));
        assert_eq!(out[1].message, "line-50");
        assert_eq!(out.last().unwrap().message, format!("line-{}", count - 1));
        assert!(!queue.has_pending());
        queue.push(vec![rec("recovered")]);
        assert_eq!(queue.take(10)[0].message, "recovered");
    }

    #[test]
    fn byte_bound_is_enforced_for_a_saturated_queue() {
        let queue = LogIngress::new();
        let line = "x".repeat(MAX_RECORD_BYTES);
        for _ in 0..400 {
            queue.push(vec![rec(&line)]);
        }
        let stats = queue.stats();
        assert!(stats.pending_bytes <= MAX_PENDING_BYTES, "{stats:?}");
        assert!(stats.dropped_total > 0);
    }

    #[test]
    fn take_slices_work_and_preserves_the_remaining_order() {
        let queue = LogIngress::new();
        queue.push((0..10).map(|i| rec(&format!("line-{i}"))).collect());
        assert_eq!(queue.take(4).len(), 4);
        assert!(queue.has_pending());
        let rest = queue.take(100);
        assert_eq!(rest.len(), 6);
        assert_eq!(rest[0].message, "line-4");
    }

    #[test]
    fn push_reports_empty_transition_for_waking() {
        let queue = LogIngress::new();
        assert!(queue.push(vec![rec("first")]));
        assert!(!queue.push(vec![rec("second")]));
        queue.take(10);
        assert!(queue.push(vec![rec("third")]));
    }

    #[test]
    fn oversized_line_is_clamped_deterministically_on_utf8_boundary() {
        let mut line = "é".repeat(MAX_RECORD_BYTES);
        clamp_record(&mut line);
        assert!(line.ends_with(" [record truncated]"));
        assert!(line.len() <= MAX_RECORD_BYTES + " [record truncated]".len());
        let mut short = String::from("ok");
        clamp_record(&mut short);
        assert_eq!(short, "ok");
    }

    #[test]
    fn handle_reports_orphaning_after_consumer_drop() {
        let queue = LogIngress::new();
        let consumer = queue.clone();
        assert!(!queue.is_orphaned());
        drop(consumer);
        assert!(queue.is_orphaned());
    }
}
