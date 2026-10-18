//! Pure loop-scheduling policy — the byte-budgeted PTY drain and the frame
//! pacing gate, isolated as data-in/data-out functions so the scheduling
//! behavior is exhaustively unit-tested in CI (the same treatment
//! `render_plan` gives the render decision).
//!
//! The drain policy replaces the old chunk-count budget (64 × 8KB = an
//! unbounded ~512KB of on-loop vt100 parsing per iteration, three scans per
//! byte) with a **byte + deadline** budget, split fairly across panes, and an
//! **input preemption** contract: a keystroke discovered mid-drain aborts the
//! drain immediately instead of waiting out the whole backlog.

use std::time::{Duration, Instant};
use thegn_core::log::parser::ParsedLog;
use thegn_svc::log::ingress::LogIngress;

/// Per-iteration PTY parse budget. `max_bytes` bounds the vt100 feed volume;
/// `deadline` is the wall-clock backstop for pathological escape-heavy input
/// where bytes/µs collapses. Checked between pane slices (never mid-buffer).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DrainBudget {
    pub max_bytes: usize,
    pub deadline: Duration,
}

/// The drain budget for this iteration. With input pending the budget shrinks
/// so the responding frame is never queued behind bulk parsing. Measured feed
/// cost on scroll-heavy output (alacritty parse + history pass + query scan)
/// is ~0.25µs/byte in release, so the deadline is the operative bound and
/// `max_bytes` is the backstop; the deadline is checked between
/// [`MAX_SLICE`]-capped slices, so one iteration overshoots by at most one
/// slice (~4ms).
pub fn drain_budget(input_pending: bool) -> DrainBudget {
    if input_pending {
        DrainBudget {
            max_bytes: 16 * 1024,
            deadline: Duration::from_millis(2),
        }
    } else {
        DrainBudget {
            max_bytes: 128 * 1024,
            deadline: Duration::from_millis(5),
        }
    }
}

/// Display rows retained for the Logs panel.
pub const LOG_RETAIN: usize = 1000;

/// Per-iteration log application limit. The deadline is a backstop for
/// pathological records; the record count stays the operative bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogBudget {
    pub max_records: usize,
    pub deadline: Duration,
}

/// Maximum records applied in one chunk, bounding deadline-check granularity.
pub const LOG_CHUNK: usize = 32;

/// Log application budget; input pending gets a smaller count and deadline.
pub fn log_budget(input_pending: bool) -> LogBudget {
    if input_pending {
        LogBudget {
            max_records: 32,
            deadline: Duration::from_millis(1),
        }
    } else {
        LogBudget {
            max_records: 256,
            deadline: Duration::from_millis(2),
        }
    }
}

/// Outcome of applying one bounded portion of the log ingress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogDrain {
    pub applied: usize,
    pub took: bool,
    pub dirty: bool,
    pub backlog: bool,
}

/// Drain bounded log chunks, checking the time backstop between chunks.
pub fn drain_logs(
    ingress: &LogIngress,
    rows: &mut Vec<ParsedLog>,
    first_seq: &mut u64,
    budget: LogBudget,
    visible: bool,
    mut now: impl FnMut() -> Instant,
) -> LogDrain {
    let started = now();
    let mut applied = 0;
    let mut took = false;
    let mut dirty = false;
    while applied < budget.max_records {
        let count = LOG_CHUNK.min(budget.max_records - applied);
        let slice = ingress.take(count);
        if slice.is_empty() {
            break;
        }
        took = true;
        let records = slice.len() - usize::from(slice.first().is_some_and(is_gap_marker));
        dirty |= apply_log_slice(rows, first_seq, slice, visible);
        applied += records;
        if applied >= budget.max_records
            || now().saturating_duration_since(started) >= budget.deadline
        {
            break;
        }
    }
    LogDrain {
        applied,
        took,
        dirty,
        backlog: ingress.has_pending(),
    }
}

/// Identifies the leading overload marker created by `thegn_svc::log::ingress::gap_marker`.
fn is_gap_marker(log: &ParsedLog) -> bool {
    log.timestamp.is_empty()
        && log.level == thegn_core::log::parser::LogLevel::Warn
        && log.target.is_none()
        && log.message.contains("log lines dropped under overload")
}

/// Whether a parsed log belongs to a target that must not dirty the frame.
pub fn is_render_noise(log: &ParsedLog) -> bool {
    log.target.as_deref().is_some_and(|target| {
        target == "thegn::frame"
            || target == "thegn::perf"
            || target.starts_with("thegn::frame::")
            || target.starts_with("thegn::perf::")
    })
}

/// Apply one bounded log slice to display storage, retaining the newest rows.
/// Returns whether visible Logs received a non-render-noise row to repaint.
pub fn apply_log_slice(
    rows: &mut Vec<ParsedLog>,
    first_seq: &mut u64,
    slice: Vec<ParsedLog>,
    visible: bool,
) -> bool {
    if slice.is_empty() {
        return false;
    }
    let dirty = visible && slice.iter().any(|log| !is_render_noise(log));
    rows.extend(slice);
    if rows.len() > LOG_RETAIN {
        let overflow = rows.len() - LOG_RETAIN;
        rows.drain(0..overflow);
        *first_seq += overflow as u64;
    }
    dirty
}

/// Stash high-water: stop receiving from the PTY channel once this many bytes
/// are stashed unparsed. The bounded(256) channel then fills, reader threads
/// block on `blocking_send`, and ultimately the child blocks writing its PTY —
/// the same end-to-end backpressure a plain terminal applies to `cat bigfile`.
/// Never drop bytes: a dropped chunk can split an escape sequence (corrupting
/// emulator state) and silently lose scrollback.
pub const BACKLOG_HIGH_WATER: usize = 1024 * 1024;

/// Hard cap on one pane's parse slice, so the deadline check between slices
/// has real granularity (~16KB ≈ 4ms at the measured 0.25µs/byte). Without
/// this, a single flooding pane's slice was the whole byte budget — one
/// uninterruptible 256KB ≈ 65ms feed before the deadline was even consulted.
pub const MAX_SLICE: usize = 16 * 1024;

/// One pane's parse slice this round: an even split of the remaining budget
/// across the panes that still have backlog, with an 8KB floor so a pane
/// always makes visible progress (fairness: one flooding pane can't starve
/// the others' updates, because each gets its slice before the flooder gets
/// a second one) and the [`MAX_SLICE`] ceiling for deadline granularity.
pub fn pane_slice(remaining_bytes: usize, panes_with_backlog: usize) -> usize {
    const FLOOR: usize = 8 * 1024;
    if panes_with_backlog == 0 {
        return 0;
    }
    (remaining_bytes / panes_with_backlog).clamp(FLOOR, MAX_SLICE)
}

/// What the renderer should do about pacing this iteration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameGate {
    /// Compose and flush now.
    RenderNow,
    /// Pane-only damage inside the pacing window: skip the frame, keep the
    /// damage accumulated, and arm the poll timeout with `remaining` so the
    /// trailing frame always flushes within the window (the idle-never-polls
    /// invariant holds — Defer only occurs with damage pending).
    Defer { remaining: Duration },
}

/// The pacing window for pane-only frames under sustained output. One frame
/// per window is imperceptible for streaming text but halves-or-better the
/// compose+diff+flush volume, freeing the loop for parsing and input.
pub const PANE_FRAME_WINDOW: Duration = Duration::from_millis(8);

/// Decide whether to render now or defer.
///
/// Rules, in precedence order:
/// 1. A dispatched input is awaiting its frame (`input_awaiting_frame`, the
///    loop's `input_at` stamp) ⇒ **render now** — input responsiveness is
///    absolute, and this also bounds rule 2 under a key flood (dispatch and
///    render alternate, so rendering can never starve).
/// 2. An interactive event is queued but not yet dispatched (`input_queued`,
///    found by the drain's preemption poll) ⇒ **defer with zero remaining**:
///    skip this compose, let the loop bottom dispatch it immediately, and let
///    the NEXT frame carry both the pane damage and the input's effect — one
///    frame instead of a stale one plus a real one (and the `input_us` stamp
///    is never consumed by a frame that predates its dispatch).
/// 3. Chrome / switch / geometry / bars / sidebar damage ⇒ **render now**
///    (interaction feedback is never paced).
/// 4. Pure pane output (streaming) renders at most once per
///    [`PANE_FRAME_WINDOW`]; inside the window it defers with the remainder,
///    which the loop arms as its poll timeout — the trailing-frame guarantee.
pub fn frame_gate(
    input_awaiting_frame: bool,
    input_queued: bool,
    pane_only_damage: bool,
    since_last_flush: Duration,
    window: Duration,
) -> FrameGate {
    if input_awaiting_frame {
        return FrameGate::RenderNow;
    }
    if input_queued {
        return FrameGate::Defer {
            remaining: Duration::ZERO,
        };
    }
    if !pane_only_damage {
        return FrameGate::RenderNow;
    }
    if since_last_flush >= window {
        FrameGate::RenderNow
    } else {
        FrameGate::Defer {
            remaining: window - since_last_flush,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::time::Instant;
    use thegn_core::log::parser::LogLevel;
    use thegn_svc::log::ingress::MAX_PENDING_RECORDS;

    fn record(message: &str) -> ParsedLog {
        ParsedLog {
            timestamp: String::new(),
            level: LogLevel::Info,
            original: message.to_string(),
            message: message.to_string(),
            worktree: None,
            target: Some("test".to_string()),
        }
    }

    fn noise() -> ParsedLog {
        ParsedLog {
            timestamp: String::new(),
            level: LogLevel::Debug,
            original: "frame noise".to_string(),
            message: "frame noise".to_string(),
            worktree: None,
            target: Some("thegn::frame".to_string()),
        }
    }

    #[test]
    fn log_application_budget_is_bounded_and_yields_to_input() {
        let hot = log_budget(true);
        let quiet = log_budget(false);
        assert!(hot.max_records < quiet.max_records);
        assert!(hot.deadline <= quiet.deadline);
        assert!(quiet.max_records <= LOG_RETAIN);
        assert!(hot.max_records > 0);
        assert!(LOG_CHUNK <= hot.max_records);
    }

    #[test]
    fn drain_stops_at_the_time_budget() {
        let ingress = LogIngress::new();
        ingress.push(
            (0..300)
                .map(|index| record(&format!("record-{index}")))
                .collect(),
        );
        let clock = Cell::new(Instant::now());
        let result = drain_logs(
            &ingress,
            &mut Vec::new(),
            &mut 0,
            log_budget(false),
            true,
            || {
                let at = clock.get();
                clock.set(at + Duration::from_micros(1500));
                at
            },
        );
        assert_eq!(result.applied, 2 * LOG_CHUNK);
        assert!(result.backlog);
    }

    #[test]
    fn drain_stops_at_the_record_budget_with_a_frozen_clock() {
        let ingress = LogIngress::new();
        ingress.push(
            (0..300)
                .map(|index| record(&format!("record-{index}")))
                .collect(),
        );
        let fixed = Instant::now();
        let quiet = drain_logs(
            &ingress,
            &mut Vec::new(),
            &mut 0,
            log_budget(false),
            true,
            || fixed,
        );
        assert_eq!(quiet.applied, 256);
        assert!(quiet.backlog);

        let hot_ingress = LogIngress::new();
        hot_ingress.push(
            (0..300)
                .map(|index| record(&format!("record-{index}")))
                .collect(),
        );
        let hot = drain_logs(
            &hot_ingress,
            &mut Vec::new(),
            &mut 0,
            log_budget(true),
            true,
            || fixed,
        );
        assert_eq!(hot.applied, 32);
        assert!(hot.backlog);
    }

    #[test]
    fn continuous_producer_with_input_and_visibility_transitions() {
        let ingress = LogIngress::new();
        let mut rows = Vec::new();
        let mut first_seq = 0;
        let mut next_record = 0;
        let mut total_applied = 0;
        let iterations = 12;

        for iteration in 0..iterations {
            let mut batch = Vec::new();
            if iteration % 3 == 0 {
                batch.push(noise());
            }
            for _ in 0..64 {
                batch.push(record(&format!("record-{next_record}")));
                next_record += 1;
            }
            ingress.push(batch);

            let input_pending = iteration % 2 == 0;
            let visible = (iterations / 3..2 * iterations / 3).contains(&iteration);
            let fixed = Instant::now();
            let result = drain_logs(
                &ingress,
                &mut rows,
                &mut first_seq,
                log_budget(input_pending),
                visible,
                || fixed,
            );
            total_applied += result.applied;
            assert!(result.applied <= log_budget(input_pending).max_records);
            if !visible {
                assert!(!result.dirty);
            } else if result.applied > 0 {
                assert!(result.dirty);
            }
            assert_eq!(
                crate::idle_poll::poll_timeout(None, false, false, result.backlog),
                result.backlog.then_some(crate::idle_poll::BATCH_POLL)
            );
        }

        let pending = ingress.stats().pending;
        let max_drain_iterations = pending.div_ceil(LOG_CHUNK) + 1;
        let mut trailing_iterations = 0;
        while ingress.has_pending() && trailing_iterations < max_drain_iterations {
            let fixed = Instant::now();
            let result = drain_logs(
                &ingress,
                &mut rows,
                &mut first_seq,
                log_budget(true),
                false,
                || fixed,
            );
            total_applied += result.applied;
            trailing_iterations += 1;
        }
        assert!(!ingress.has_pending());
        assert!(trailing_iterations <= max_drain_iterations);
        assert_eq!(
            rows.last().map(|log| log.message.as_str()),
            Some("record-767")
        );
        assert!(rows.windows(2).all(|pair| {
            let left = pair[0].message.strip_prefix("record-");
            let right = pair[1].message.strip_prefix("record-");
            match (left, right) {
                (Some(left), Some(right)) => {
                    left.parse::<usize>().unwrap() < right.parse::<usize>().unwrap()
                }
                _ => true,
            }
        }));
        assert!(rows.len() <= LOG_RETAIN);
        assert_eq!(first_seq + rows.len() as u64, total_applied as u64);
        assert_eq!(
            crate::idle_poll::poll_timeout(None, false, false, false),
            None
        );

        let fixed = Instant::now();
        let empty = drain_logs(
            &ingress,
            &mut rows,
            &mut first_seq,
            log_budget(false),
            false,
            || fixed,
        );
        assert_eq!(empty.applied, 0);
        assert!(!empty.took);
        assert!(!empty.dirty);
        assert!(!empty.backlog);
    }

    #[test]
    fn overload_gap_marker_is_applied_in_order_and_not_counted() {
        let ingress = LogIngress::new();
        ingress.push(
            (0..MAX_PENDING_RECORDS + 25)
                .map(|index| record(&format!("record-{index}")))
                .collect(),
        );
        let mut rows = Vec::new();
        let fixed = Instant::now();
        let result = drain_logs(&ingress, &mut rows, &mut 0, log_budget(false), true, || {
            fixed
        });
        assert_eq!(result.applied, 256);
        assert!(result.took);
        assert_eq!(rows.len(), 257);
        assert!(rows[0].message.contains("25 log lines dropped"));
        assert_eq!(rows[1].message, "record-25");
    }

    #[test]
    fn log_slice_visibility_controls_dirty_result() {
        let slice = vec![thegn_core::log::parser::parse_log("one")];
        let mut hidden = Vec::new();
        let mut first_seq = 0;
        assert!(!apply_log_slice(&mut hidden, &mut first_seq, slice, false));
        assert_eq!(hidden.len(), 1);

        let mut visible = Vec::new();
        assert!(apply_log_slice(
            &mut visible,
            &mut first_seq,
            vec![thegn_core::log::parser::parse_log("one")],
            true,
        ));
    }

    #[test]
    fn log_slice_retains_newest_rows_and_gap_position() {
        let mut slice: Vec<_> = (0..LOG_RETAIN + 2)
            .map(|index| thegn_core::log::parser::parse_log(&format!("line-{index}")))
            .collect();
        // Ingress creates synthetic gap metadata directly; parsing it as a
        // source log would interpret its first words as timestamp and level.
        let gap = "[3 log lines dropped under overload]".to_string();
        slice[500] = ParsedLog {
            timestamp: String::new(),
            level: thegn_core::log::parser::LogLevel::Warn,
            original: gap.clone(),
            message: gap,
            worktree: None,
            target: None,
        };
        let mut rows = Vec::new();
        let mut first_seq = 0;

        assert!(apply_log_slice(&mut rows, &mut first_seq, slice, true));
        assert_eq!(rows.len(), LOG_RETAIN);
        assert_eq!(first_seq, 2);
        assert_eq!(rows[0].message, "line-2");
        assert_eq!(rows[497].message, "line-499");
        assert!(rows[498].message.contains("3 log lines dropped"));
        assert_eq!(rows[499].message, "line-501");
        assert_eq!(rows[LOG_RETAIN - 1].message, "line-1001");
    }

    #[test]
    fn noise_only_slice_is_not_dirty() {
        let noise = || {
            thegn_core::log::parser::parse_log(
                "2026-06-05T12:00:00  DEBUG thegn::frame  full frame chosen",
            )
        };
        let mut rows = Vec::new();
        let mut first_seq = 0;
        assert!(!apply_log_slice(
            &mut rows,
            &mut first_seq,
            vec![noise(), noise()],
            true,
        ));
        assert_eq!(rows.len(), 2);
        assert!(apply_log_slice(
            &mut rows,
            &mut first_seq,
            vec![thegn_core::log::parser::parse_log(
                "2026-06-05T12:00:01  INFO  thegn::db  ready",
            )],
            true,
        ));
    }

    #[test]
    fn ring_drop_advances_first_seq() {
        let slice = (0..LOG_RETAIN + 5)
            .map(|index| thegn_core::log::parser::parse_log(&format!("line-{index}")))
            .collect();
        let mut rows = Vec::new();
        let mut first_seq = 0;
        assert!(apply_log_slice(&mut rows, &mut first_seq, slice, true));
        assert_eq!(first_seq, 5);
        assert_eq!(rows.len(), LOG_RETAIN);
    }

    #[test]
    fn input_pending_shrinks_the_budget() {
        let quiet = drain_budget(false);
        let hot = drain_budget(true);
        assert!(hot.max_bytes < quiet.max_bytes);
        assert!(hot.deadline < quiet.deadline);
        // The input budget stays small enough that its parse cost can't push
        // the responding frame past the 16ms frame budget.
        assert!(hot.max_bytes <= 32 * 1024);
    }

    #[test]
    fn quiet_budget_is_bounded() {
        // The whole point vs the old 64×8KB chunk budget: a hard byte ceiling
        // (~0.25µs/byte measured ⇒ ≤ ~32ms even if the deadline never fired)
        // plus a deadline that actually bites between MAX_SLICE slices.
        let b = drain_budget(false);
        assert!(b.max_bytes <= 128 * 1024);
        assert!(b.deadline <= Duration::from_millis(5));
    }

    #[test]
    fn pane_slice_splits_evenly_with_floor_and_cap() {
        // Even split, but never past the deadline-granularity cap.
        assert_eq!(pane_slice(256 * 1024, 4), MAX_SLICE);
        assert_eq!(pane_slice(24 * 1024, 2), 12 * 1024);
        // Tiny remainder across many panes ⇒ the 8KB floor guarantees progress.
        assert_eq!(pane_slice(4 * 1024, 10), 8 * 1024);
        // One flooding pane can't monopolize an uninterruptible mega-slice.
        assert_eq!(pane_slice(128 * 1024, 1), MAX_SLICE);
        // No backlog ⇒ nothing to hand out.
        assert_eq!(pane_slice(256 * 1024, 0), 0);
    }

    #[test]
    fn fairness_small_pane_parses_alongside_a_flood() {
        // Pane A has 10MB queued, pane B has 8KB: with an even split both get
        // a slice ≥ B's entire backlog, so B's bytes land this iteration.
        let slice = pane_slice(drain_budget(false).max_bytes, 2);
        assert!(
            slice >= 8 * 1024,
            "per-pane slice {slice} must cover a small pane's backlog"
        );
    }

    #[test]
    fn dispatched_input_frame_always_renders_now() {
        for pane_only in [true, false] {
            assert_eq!(
                frame_gate(true, false, pane_only, Duration::ZERO, PANE_FRAME_WINDOW),
                FrameGate::RenderNow,
                "the frame answering a dispatched input is never paced"
            );
        }
    }

    #[test]
    fn queued_undispatched_input_defers_so_the_frame_carries_its_effect() {
        // A keystroke found mid-drain (queued, not yet dispatched) must not be
        // answered with a stale pre-dispatch frame: defer with zero remaining,
        // dispatch, and render once.
        assert_eq!(
            frame_gate(false, true, true, Duration::ZERO, PANE_FRAME_WINDOW),
            FrameGate::Defer {
                remaining: Duration::ZERO
            }
        );
        // ...but a dispatched input awaiting its frame outranks a newly-queued
        // one, so a key flood alternates dispatch/render and can never starve
        // rendering.
        assert_eq!(
            frame_gate(true, true, true, Duration::ZERO, PANE_FRAME_WINDOW),
            FrameGate::RenderNow
        );
    }

    #[test]
    fn chrome_or_switch_damage_renders_now() {
        // Anything that isn't pure pane output is interaction feedback.
        assert_eq!(
            frame_gate(false, false, false, Duration::ZERO, PANE_FRAME_WINDOW),
            FrameGate::RenderNow
        );
    }

    #[test]
    fn pane_only_damage_is_paced_with_a_trailing_flush() {
        // Inside the window: defer, with the exact remainder for the poll
        // timeout (the trailing-frame guarantee).
        let g = frame_gate(
            false,
            false,
            true,
            Duration::from_millis(3),
            PANE_FRAME_WINDOW,
        );
        assert_eq!(
            g,
            FrameGate::Defer {
                remaining: Duration::from_millis(5)
            }
        );
        // At/after the window boundary: render.
        assert_eq!(
            frame_gate(false, false, true, PANE_FRAME_WINDOW, PANE_FRAME_WINDOW),
            FrameGate::RenderNow
        );
        assert_eq!(
            frame_gate(
                false,
                false,
                true,
                Duration::from_millis(20),
                PANE_FRAME_WINDOW
            ),
            FrameGate::RenderNow
        );
    }
}
