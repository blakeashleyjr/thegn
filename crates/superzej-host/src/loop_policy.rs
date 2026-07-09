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

use std::time::Duration;

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
