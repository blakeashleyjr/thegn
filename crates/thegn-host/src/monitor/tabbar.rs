//! Tab-bar arithmetic for the system-monitor modal: which digit jumps where,
//! and which run of tabs fits on the bar.
//!
//! Pure by construction — no `self`, no caps read, no color. The monitor's tab
//! bar used to lay itself out inline in `monitor.rs` and silently let
//! `draw_line`'s `Line::Split` arm cut the tail off; the cut could land on the
//! ACTIVE tab, so the bar stopped saying where the user was. Making the
//! windowing a function means the "the active tab is always whole" rule is a
//! testable claim rather than an accident of how long the labels happen to be.

/// The digit key that jumps to visible tab `i`: `1`–`9`, then `0` for the
/// tenth. `None` past ten (no key can reach it, and the bar says so by omitting
/// the digit rather than by lying).
pub(super) fn digit(i: usize) -> Option<char> {
    match i {
        0..=8 => char::from_digit(i as u32 + 1, 10),
        9 => Some('0'),
        _ => None,
    }
}

/// The visible-tab index a digit key selects — the inverse of [`digit`], found
/// by asking [`digit`] rather than by a second table that could drift out of
/// step with the one the bar is drawn from.
pub(super) fn index_of(c: char) -> Option<usize> {
    (0..10).find(|&i| digit(i) == Some(c))
}

/// The run of tabs the bar shows, and whether anything is hidden either side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct TabWindow {
    pub start: usize,
    /// Exclusive.
    pub end: usize,
    pub clipped_left: bool,
    pub clipped_right: bool,
}

/// The contiguous run of tabs that fits in `width`, always containing `active`
/// WHOLE. `widths[i]` is the drawn width of tab `i` including its leading
/// separator. Grows outward from `active` — right first, then left — so the
/// common case (a bar that fits) returns the whole range unchanged.
///
/// Total: `active` alone wider than `width` yields `start..start + 1` (the
/// caller lets `draw_line` clip it, which is at least honest about *which* tab
/// is current); an empty `widths` yields an empty window.
pub(super) fn window(widths: &[usize], active: usize, width: usize) -> TabWindow {
    let n = widths.len();
    if n == 0 {
        return TabWindow {
            start: 0,
            end: 0,
            clipped_left: false,
            clipped_right: false,
        };
    }
    let active = active.min(n - 1);
    // The active tab is seeded unconditionally — a window that dropped it would
    // leave the bar with nothing marked, which is the bug this exists to stop.
    let mut used = widths[active];
    let mut end = active + 1;
    while end < n && used + widths[end] <= width {
        used += widths[end];
        end += 1;
    }
    let mut start = active;
    while start > 0 && used + widths[start - 1] <= width {
        used += widths[start - 1];
        start -= 1;
    }
    TabWindow {
        start,
        end,
        clipped_left: start > 0,
        clipped_right: end < n,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digits_cover_ten_tabs_and_stop() {
        assert_eq!(digit(0), Some('1'));
        assert_eq!(digit(8), Some('9'));
        // The tenth tab is `0` — before this, `MonitorTab::ALL`'s tenth entry
        // was unreachable by keyboard on a machine showing every family.
        assert_eq!(digit(9), Some('0'));
        assert_eq!(digit(10), None);
        // …and the inverse agrees at every rung, including the wrap at `0`.
        for i in 0..10 {
            assert_eq!(index_of(digit(i).expect("a digit")), Some(i), "{i}");
        }
        assert_eq!(index_of('a'), None);
    }

    #[test]
    fn a_bar_that_fits_is_shown_whole() {
        let w = vec![5, 7, 7, 7];
        let win = window(&w, 0, 100);
        assert_eq!((win.start, win.end), (0, 4));
        assert!(!win.clipped_left && !win.clipped_right);
    }

    #[test]
    fn the_window_anchors_to_whichever_end_the_cursor_is_on() {
        let w = vec![10, 10, 10, 10];
        // Active at the far right: nothing to grow into on the right, so the
        // window is right-anchored and the overflow is on the left.
        let win = window(&w, 3, 25);
        assert_eq!((win.start, win.end), (2, 4));
        assert!(win.clipped_left && !win.clipped_right);
        // Active at the far left: the mirror image.
        let win = window(&w, 0, 25);
        assert_eq!((win.start, win.end), (0, 2));
        assert!(!win.clipped_left && win.clipped_right);
    }

    #[test]
    fn an_oversized_active_tab_still_yields_a_window() {
        // Totality: the active tab alone does not fit. One tab out, clipped by
        // `draw_line` — never an empty bar.
        let w = vec![10, 40, 10];
        let win = window(&w, 1, 8);
        assert_eq!((win.start, win.end), (1, 2));
        assert!(win.clipped_left && win.clipped_right);
    }

    #[test]
    fn an_empty_bar_is_an_empty_window() {
        let win = window(&[], 0, 40);
        assert_eq!((win.start, win.end), (0, 0));
        assert!(!win.clipped_left && !win.clipped_right);
    }
}
