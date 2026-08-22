//! Calendar popup geometry — **pure**, and the single source of truth.
//!
//! `render` draws into the rects this produces and `hit` tests against the same
//! rects, so the painted cells and the clickable cells cannot drift apart. That
//! is a stronger guarantee than the masthead's (where a layout function and a
//! span function each compute the same thing separately).

use crate::compositor::Rect;
use chrono::NaiveDate;

/// How tightly the day grid is packed, chosen by available width.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct GridMetrics {
    /// Cells per day cell, including its marker column.
    pub cell_w: usize,
    /// Cells between day columns.
    pub gap: usize,
    /// Width of the ISO week-number gutter (0 when hidden).
    pub gutter: usize,
}

impl GridMetrics {
    /// Total width of the grid this describes.
    pub fn width(&self) -> usize {
        self.gutter + 7 * self.cell_w + 6 * self.gap
    }
}

/// The narrowest content width a month view is worth drawing at.
///
/// The tightest grid is only 21 cells, but the header above it —
/// `⟨h⟩ September 2026 ⟨l⟩` — needs more than that, and a popup whose title row
/// is truncated to `h… today ·` is worse than no grid at all. Below this the
/// caller falls back to the plain date/time readout.
pub(crate) const MIN_GRID_COLS: usize = 28;

/// The width at which the right-aligned `today · …` chip stops fitting
/// alongside the month title, and is dropped rather than colliding with it.
///
/// Sized against the widest header the grid produces —
/// `⟨h⟩ September 2026 ⟨l⟩` (23 cells) plus `today · Sat 22 Aug` (19) — and
/// deliberately BELOW the popup's own preferred width, or the chip could never
/// appear at any terminal size.
pub(crate) const TODAY_CHIP_MIN_COLS: usize = 42;

/// The widest grid that fits in `w`, in decreasing order of comfort.
///
/// Returns `None` below the tightest option — the caller falls back to a plain
/// date/time key-value popup rather than drawing a broken grid.
pub(crate) fn metrics(w: usize, week_numbers: bool) -> Option<GridMetrics> {
    if w < MIN_GRID_COLS {
        return None;
    }
    let gutter = if week_numbers { 4 } else { 0 };
    let candidates = [
        GridMetrics {
            cell_w: 4,
            gap: 1,
            gutter,
        },
        // Dropping the gutter before tightening the cells keeps two-digit days
        // comfortably spaced; the week numbers are the softer information.
        GridMetrics {
            cell_w: 4,
            gap: 1,
            gutter: 0,
        },
        GridMetrics {
            cell_w: 3,
            gap: 1,
            gutter: 0,
        },
        GridMetrics {
            cell_w: 3,
            gap: 0,
            gutter: 0,
        },
    ];
    candidates.into_iter().find(|m| m.width() <= w)
}

/// What a click inside the popup landed on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CalHit {
    PrevMonth,
    NextMonth,
    Today,
    Day(NaiveDate),
    AgendaRow(usize),
}

/// The resolved rects of one month-grid block.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct GridLayout {
    pub metrics: GridMetrics,
    /// The `⟨h⟩` keycap.
    pub prev: Rect,
    /// The `⟨l⟩` keycap.
    pub next: Rect,
    /// The right-aligned `today · …` chip.
    pub today: Rect,
    /// Every day cell, row-major, paired with its date.
    pub days: Vec<(NaiveDate, Rect)>,
}

/// Rows the header occupies above the first week row: title, blank, weekday
/// names.
pub(crate) const GRID_HEADER_ROWS: usize = 3;

/// Lay out a month-grid block whose top-left content cell is `(x, y0)`.
///
/// `y0` is signed because a scrolled popup can place a block above the box; the
/// rects come back in absolute screen coordinates and callers clip.
pub(crate) fn grid_layout(
    x: usize,
    y0: i64,
    w: usize,
    weeks: &[[NaiveDate; 7]],
    week_numbers: bool,
    title_w: usize,
    today_w: usize,
) -> Option<GridLayout> {
    let m = metrics(w, week_numbers)?;
    let row = |dy: i64, cx: usize, cw: usize| Rect {
        x: cx,
        // A block scrolled above the viewport has no valid row; the caller
        // clips, so clamp rather than wrapping into an enormous usize.
        y: dy.max(0) as usize,
        cols: cw,
        rows: 1,
    };
    // Header: `⟨h⟩ <Month Year> ⟨l⟩` on the left, `today · …` right-aligned.
    let prev_w = 3;
    let next_w = 3;
    let prev = row(y0, x, prev_w);
    let next = row(y0, x + prev_w + 1 + title_w + 1, next_w);
    let today = row(y0, x + w.saturating_sub(today_w), today_w);

    let mut days = Vec::with_capacity(weeks.len() * 7);
    for (r, week) in weeks.iter().enumerate() {
        let wy = y0 + (GRID_HEADER_ROWS + r) as i64;
        for (c, date) in week.iter().enumerate() {
            let cx = x + m.gutter + c * (m.cell_w + m.gap);
            days.push((*date, row(wy, cx, m.cell_w)));
        }
    }
    Some(GridLayout {
        metrics: m,
        prev,
        next,
        today,
        days,
    })
}

/// Whether `(px, py)` is inside `r`, and `r` is inside the clip.
pub(crate) fn hit_rect(clip: Rect, r: Rect, px: usize, py: usize) -> bool {
    // Reject rows scrolled outside the box first, so a day cell that isn't
    // actually painted is never clickable.
    if py < clip.y || py >= clip.y + clip.rows {
        return false;
    }
    px >= r.x && px < r.x + r.cols && py >= r.y && py < r.y + r.rows
}

/// Resolve a click against a laid-out grid.
pub(crate) fn hit_grid(clip: Rect, g: &GridLayout, px: usize, py: usize) -> Option<CalHit> {
    if hit_rect(clip, g.prev, px, py) {
        return Some(CalHit::PrevMonth);
    }
    if hit_rect(clip, g.next, px, py) {
        return Some(CalHit::NextMonth);
    }
    if hit_rect(clip, g.today, px, py) {
        return Some(CalHit::Today);
    }
    g.days
        .iter()
        .find(|(_, r)| hit_rect(clip, *r, px, py))
        .map(|(d, _)| CalHit::Day(*d))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_today_chip_threshold_is_actually_reachable() {
        // The popup asks for a fixed preferred width; a chip threshold above it
        // would silently hide the chip at every terminal size. This pins the
        // relationship rather than the two constants drifting apart.
        let roomy = GridMetrics {
            cell_w: 4,
            gap: 1,
            gutter: 0,
        }
        .width();
        // `preferred_cols` floors at 44; the chip must fit inside that.
        assert!(
            TODAY_CHIP_MIN_COLS <= 44,
            "the chip could never appear: threshold {TODAY_CHIP_MIN_COLS} > 44"
        );
        assert!(
            TODAY_CHIP_MIN_COLS > roomy.min(MIN_GRID_COLS),
            "the chip should be dropped before the grid is"
        );
        assert!(MIN_GRID_COLS < TODAY_CHIP_MIN_COLS);
    }

    #[test]
    fn densities_step_down_and_then_give_up() {
        // Below the header-aware minimum there is no grid at all.
        assert!(metrics(MIN_GRID_COLS - 1, false).is_none());
        assert!(metrics(0, false).is_none());
        // Widths step through the four densities, widest first.
        let roomy = metrics(64, true).unwrap();
        assert_eq!((roomy.cell_w, roomy.gap, roomy.gutter), (4, 1, 4));
        // No room for the gutter: it goes before the cells tighten.
        let no_gutter = metrics(36, true).unwrap();
        assert_eq!((no_gutter.cell_w, no_gutter.gutter), (4, 0));
        let tight = metrics(30, false).unwrap();
        assert_eq!((tight.cell_w, tight.gap), (3, 1));
        let cramped = metrics(MIN_GRID_COLS, false).unwrap();
        assert!(cramped.width() <= MIN_GRID_COLS);
        // Every density is seven columns wide and fits what it was given.
        for w in MIN_GRID_COLS..80 {
            let m = metrics(w, false).unwrap();
            assert!(m.width() <= w, "density overflows at {w}");
        }
    }
}
