//! Chrome layout: the fixed cross the compositor paints around the center pane
//! region — tabbar (top, 1 row), statusbar (bottom, 1 row), sidebar (left),
//! panel (right), center (fills the rest). A flexbox engine (taffy) is overkill
//! for this fixed cross; it earns its keep later for *widget-internal* layout
//! (chip rows etc.). The auto-hide thresholds mirror the current product
//! (panel hides under ~100 cols, sidebar under ~76).

use crate::compositor::Rect;

/// Width thresholds (in columns) below which a surface auto-collapses.
pub const PANEL_MIN_COLS: usize = 100;
pub const SIDEBAR_MIN_COLS: usize = 76;

/// Default surface extents.
pub const TABBAR_ROWS: usize = 1;
pub const STATUSBAR_ROWS: usize = 1;
pub const SIDEBAR_COLS: usize = 20; // ~12% at 160 cols
pub const PANEL_COLS: usize = 44; // ~27% at 160 cols

#[derive(Debug, Clone, PartialEq)]
pub struct ChromeLayout {
    pub tabbar: Rect,
    pub statusbar: Rect,
    pub sidebar: Option<Rect>,
    pub panel: Option<Rect>,
    pub center: Rect,
}

/// Compute the chrome cross for a `cols`x`rows` screen. `want_sidebar`/
/// `want_panel` are the user's toggle state; each is additionally suppressed
/// when the screen is too narrow.
pub fn compute(cols: usize, rows: usize, want_sidebar: bool, want_panel: bool) -> ChromeLayout {
    let show_sidebar = want_sidebar && cols >= SIDEBAR_MIN_COLS;
    let show_panel = want_panel && cols >= PANEL_MIN_COLS;

    let tabbar = Rect {
        x: 0,
        y: 0,
        cols,
        rows: TABBAR_ROWS.min(rows),
    };
    let status_y = rows.saturating_sub(STATUSBAR_ROWS);
    let statusbar = Rect {
        x: 0,
        y: status_y,
        cols,
        rows: rows.min(STATUSBAR_ROWS),
    };

    // The band between the bars.
    let band_y = TABBAR_ROWS.min(rows);
    let band_rows = rows.saturating_sub(TABBAR_ROWS + STATUSBAR_ROWS);

    // Clamp surface widths so the center keeps at least 1 column.
    let mut left = if show_sidebar { SIDEBAR_COLS } else { 0 };
    let mut right = if show_panel { PANEL_COLS } else { 0 };
    while left + right + 1 > cols && (left > 0 || right > 0) {
        if right >= left && right > 0 {
            right = right.saturating_sub(1);
        } else if left > 0 {
            left = left.saturating_sub(1);
        } else {
            break;
        }
    }

    let sidebar = (left > 0).then_some(Rect {
        x: 0,
        y: band_y,
        cols: left,
        rows: band_rows,
    });
    let panel = (right > 0).then_some(Rect {
        x: cols.saturating_sub(right),
        y: band_y,
        cols: right,
        rows: band_rows,
    });
    let center = Rect {
        x: left,
        y: band_y,
        cols: cols.saturating_sub(left + right),
        rows: band_rows,
    };

    ChromeLayout {
        tabbar,
        statusbar,
        sidebar,
        panel,
        center,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_screen_shows_both_surfaces_and_center_fills_the_gap() {
        let l = compute(160, 40, true, true);
        assert_eq!(
            l.tabbar,
            Rect {
                x: 0,
                y: 0,
                cols: 160,
                rows: 1
            }
        );
        assert_eq!(l.statusbar.y, 39);
        let sb = l.sidebar.unwrap();
        let pn = l.panel.unwrap();
        assert_eq!(sb.cols, SIDEBAR_COLS);
        assert_eq!(pn.cols, PANEL_COLS);
        // Surfaces + center tile the full width exactly, no overlap.
        assert_eq!(sb.cols + l.center.cols + pn.cols, 160);
        assert_eq!(l.center.x, SIDEBAR_COLS);
        assert_eq!(pn.x, 160 - PANEL_COLS);
        // The band sits between the bars.
        assert_eq!(l.center.y, 1);
        assert_eq!(l.center.rows, 38);
    }

    #[test]
    fn narrow_screen_auto_hides_panel_then_sidebar() {
        // 90 cols: below the panel threshold (100) but above sidebar (76).
        let l = compute(90, 40, true, true);
        assert!(l.panel.is_none(), "panel should auto-hide under 100 cols");
        assert!(l.sidebar.is_some(), "sidebar still shown at 90 cols");

        // 70 cols: both auto-hide.
        let l = compute(70, 40, true, true);
        assert!(l.panel.is_none());
        assert!(l.sidebar.is_none());
        assert_eq!(l.center.cols, 70);
        assert_eq!(l.center.x, 0);
    }

    #[test]
    fn toggled_off_surfaces_are_absent_even_when_wide() {
        let l = compute(200, 40, false, false);
        assert!(l.sidebar.is_none());
        assert!(l.panel.is_none());
        assert_eq!(l.center.cols, 200);
    }

    #[test]
    fn center_keeps_at_least_one_column_when_squeezed() {
        // Pathologically narrow but with both wanted (thresholds bypassed by
        // forcing): the clamp must never produce a zero/negative center.
        let l = compute(SIDEBAR_MIN_COLS, 10, true, true);
        assert!(l.center.cols >= 1);
        let used = l.sidebar.map(|r| r.cols).unwrap_or(0)
            + l.center.cols
            + l.panel.map(|r| r.cols).unwrap_or(0);
        assert_eq!(used, SIDEBAR_MIN_COLS);
    }
}
