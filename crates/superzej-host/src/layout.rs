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

/// The strip is suppressed when the band is too short to give it ≥ this many rows
/// while leaving the center at least this many — i.e. the strip never starves the
/// center. (Each pin also keeps a 1-row label header.)
pub const STRIP_MIN_ROWS: usize = 3;

#[derive(Debug, Clone, PartialEq)]
pub struct ChromeLayout {
    pub tabbar: Rect,
    pub statusbar: Rect,
    pub sidebar: Option<Rect>,
    pub panel: Option<Rect>,
    /// The top pinned-program strip, when visible (spans the center's columns,
    /// directly below the tabbar). `None` when hidden or too short.
    pub strip: Option<Rect>,
    pub center: Rect,
}

impl ChromeLayout {
    /// The tabbar's label/content area, aligned with the center workspace.
    ///
    /// The tabbar background stays full width, but labels should not occupy the
    /// sidebar-owned columns when the sidebar is visible.
    pub fn tabbar_content(&self) -> Rect {
        Rect {
            x: self.center.x,
            y: self.tabbar.y,
            cols: self.center.cols,
            rows: self.tabbar.rows,
        }
    }
}

/// Compute the chrome cross for a `cols`x`rows` screen. `want_sidebar`/
/// `want_panel` are the user's toggle state; each is additionally suppressed
/// when the screen is too narrow. Back-compat shim with no strip — used by tests
/// and chrome unit tests; the live loop calls [`compute_with_strip`] directly.
#[cfg(test)]
pub fn compute(cols: usize, rows: usize, want_sidebar: bool, want_panel: bool) -> ChromeLayout {
    compute_with_strip(cols, rows, want_sidebar, want_panel, false, 0.0)
}

/// Compute the chrome cross, reserving a top strip of `strip_ratio` of the band
/// when `want_strip` is set and the band is tall enough (else the strip is
/// suppressed and its rows go to the center).
pub fn compute_with_strip(
    cols: usize,
    rows: usize,
    want_sidebar: bool,
    want_panel: bool,
    want_strip: bool,
    strip_ratio: f32,
) -> ChromeLayout {
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

    let center_x = left;
    let center_cols = cols.saturating_sub(left + right);

    // Carve a top strip out of the center column when wanted and the band can
    // spare the rows (strip ≥ STRIP_MIN_ROWS while leaving center ≥ STRIP_MIN_ROWS).
    let strip_rows = if want_strip {
        let r = (band_rows as f32 * strip_ratio.clamp(0.0, 0.9)).round() as usize;
        let r = r.max(STRIP_MIN_ROWS);
        if band_rows >= r + STRIP_MIN_ROWS {
            r
        } else {
            0
        }
    } else {
        0
    };

    let strip = (strip_rows > 0).then_some(Rect {
        x: center_x,
        y: band_y,
        cols: center_cols,
        rows: strip_rows,
    });
    let center = Rect {
        x: center_x,
        y: band_y + strip_rows,
        cols: center_cols,
        rows: band_rows.saturating_sub(strip_rows),
    };

    ChromeLayout {
        tabbar,
        statusbar,
        sidebar,
        panel,
        strip,
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
    fn tabbar_content_aligns_with_center_when_sidebar_is_visible() {
        let l = compute(160, 40, true, true);
        let content = l.tabbar_content();

        assert_eq!(content.x, SIDEBAR_COLS);
        assert_eq!(content.y, l.tabbar.y);
        assert_eq!(content.cols, l.center.cols);
        assert_eq!(content.rows, l.tabbar.rows);
        assert_eq!(l.panel.unwrap().cols, PANEL_COLS);
    }

    #[test]
    fn tabbar_content_starts_at_zero_when_sidebar_is_hidden() {
        let l = compute(160, 40, false, true);
        let content = l.tabbar_content();

        assert_eq!(content.x, 0);
        assert_eq!(content.cols, l.center.cols);
    }

    #[test]
    fn repeated_layout_compute_preserves_panel_and_tabbar_content_geometry() {
        let first = compute(160, 40, true, true);
        let second = compute(160, 40, true, true);

        assert_eq!(first.panel.unwrap().cols, PANEL_COLS);
        assert_eq!(second.panel.unwrap().cols, PANEL_COLS);
        assert_eq!(first.center, second.center);
        assert_eq!(first.tabbar_content(), second.tabbar_content());
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
    fn strip_reserves_top_rows_of_the_band_and_shrinks_center() {
        // 40 rows: band is 38 (tabbar+statusbar take 2). 20% → ~8 rows strip.
        let l = compute_with_strip(160, 40, true, true, true, 0.2);
        let strip = l.strip.expect("strip visible");
        assert_eq!(strip.y, 1, "strip sits directly below the tabbar");
        assert_eq!(strip.x, l.center.x, "strip spans the center columns");
        assert_eq!(strip.cols, l.center.cols);
        // Strip + center exactly tile the band (no gap/overlap).
        assert_eq!(strip.rows + l.center.rows, 38);
        assert_eq!(l.center.y, strip.y + strip.rows);
        assert_eq!(strip.rows, 8); // round(38 * 0.2)
    }

    #[test]
    fn strip_absent_when_not_wanted() {
        let l = compute_with_strip(160, 40, true, true, false, 0.2);
        assert!(l.strip.is_none());
        assert_eq!(l.center.y, 1);
        assert_eq!(l.center.rows, 38);
    }

    #[test]
    fn strip_suppressed_when_band_too_short() {
        // Tiny band: can't give the strip its min rows and keep the center alive.
        let l = compute_with_strip(160, 6, true, true, true, 0.5);
        assert!(l.strip.is_none(), "strip suppressed in a short band");
        assert!(l.center.rows >= 1);
    }

    #[test]
    fn strip_clamps_to_min_rows_for_small_ratios() {
        // A tiny ratio still yields at least STRIP_MIN_ROWS when the band allows.
        let l = compute_with_strip(160, 40, true, true, true, 0.01);
        assert_eq!(l.strip.unwrap().rows, STRIP_MIN_ROWS);
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
