//! Chrome layout: the fixed cross the compositor paints around the center pane
//! region — masthead (top, 2 rows: brand/stats + worktree/tab strip),
//! statusbar (bottom, 1 row), sidebar (left), panel (right), center (fills the
//! rest). A flexbox engine (taffy) is overkill for this fixed cross; it earns
//! its keep later for *widget-internal* layout (chip rows etc.). The auto-hide
//! thresholds mirror the current product (panel hides under ~100 cols, sidebar
//! under ~76).

use crate::compositor::Rect;

/// Width thresholds (in columns) below which a surface auto-collapses.
pub const PANEL_MIN_COLS: usize = 100;
pub const SIDEBAR_MIN_COLS: usize = 76;

/// Default surface extents.
pub const MASTHEAD_ROWS: usize = 1;
pub const STATUSBAR_ROWS: usize = 1;
pub const SIDEBAR_COLS: usize = 20; // ~12% at 160 cols
pub const PANEL_COLS: usize = 44; // ~27% at 160 cols

/// The strip is suppressed when the band is too short to give it ≥ this many rows
/// while leaving the center at least this many — i.e. the strip never starves the
/// center. (Each pin also keeps a 1-row label header.)
pub const STRIP_MIN_ROWS: usize = 3;

#[derive(Debug, Clone, PartialEq)]
pub struct ChromeLayout {
    /// The single-row bar above everything: text brand left, stats right.
    pub masthead: Rect,
    /// A 1-row horizontal rule between the masthead and the column band —
    /// the seam that caps the columns at a shared top level. Zero-row rect on
    /// terminals too short to spare it.
    pub divider: Rect,
    pub statusbar: Rect,
    pub sidebar: Option<Rect>,
    pub panel: Option<Rect>,
    /// Column index of the 1-col separator between the sidebar and the center
    /// (`None` when the sidebar is hidden).
    pub sep_left: Option<usize>,
    /// Column index of the 1-col separator between the center and the panel.
    pub sep_right: Option<usize>,
    /// The center column's tab bar (worktree label + tab chips), directly
    /// below the divider — level with the sidebar header and the panel's
    /// DIFF/FILES/PR/CHECKS switcher. Zero-row rect on tiny terminals.
    pub center_tabs: Rect,
    /// The top pinned-program strip, when visible (spans the center's columns,
    /// directly below the center tab bar). `None` when hidden or too short.
    pub strip: Option<Rect>,
    pub center: Rect,
}

impl ChromeLayout {
    /// Row 0 of the masthead: brand + stats cluster.
    pub fn masthead_stats_row(&self) -> Rect {
        Rect {
            x: self.masthead.x,
            y: self.masthead.y,
            cols: self.masthead.cols,
            rows: self.masthead.rows.min(1),
        }
    }
}

/// Min/max sidebar width when adjusted at runtime (item 25).
pub const SIDEBAR_MIN_WIDTH: usize = 12;
pub const SIDEBAR_MAX_WIDTH: usize = 48;

/// Compute the chrome cross with the default sidebar width and no strip.
/// (Convenience used by tests; the live loop calls [`compute_full`] with the
/// runtime sidebar width + strip state.)
#[allow(dead_code)]
pub fn compute(cols: usize, rows: usize, want_sidebar: bool, want_panel: bool) -> ChromeLayout {
    compute_full(
        cols,
        rows,
        want_sidebar,
        want_panel,
        false,
        false,
        SIDEBAR_COLS,
        false,
        0.0,
    )
}

/// Compute the chrome cross with an explicit sidebar width, no strip.
#[allow(dead_code)]
pub fn compute_with_width(
    cols: usize,
    rows: usize,
    want_sidebar: bool,
    want_panel: bool,
    sidebar_cols: usize,
) -> ChromeLayout {
    compute_full(
        cols,
        rows,
        want_sidebar,
        want_panel,
        false,
        false,
        sidebar_cols,
        false,
        0.0,
    )
}

/// Compute the chrome cross, reserving a top strip of `strip_ratio` of the band
/// when `want_strip` is set and the band is tall enough (else the strip is
/// suppressed and its rows go to the center). Uses the default sidebar width.
/// (Convenience used by tests; the live loop calls [`compute_full`].)
#[allow(dead_code)]
pub fn compute_with_strip(
    cols: usize,
    rows: usize,
    want_sidebar: bool,
    want_panel: bool,
    want_strip: bool,
    strip_ratio: f32,
) -> ChromeLayout {
    compute_full(
        cols,
        rows,
        want_sidebar,
        want_panel,
        false,
        false,
        SIDEBAR_COLS,
        want_strip,
        strip_ratio,
    )
}

/// The full chrome-cross computation: explicit sidebar width *and* optional top
/// strip. `want_sidebar`/`want_panel` are the user's toggle state; each is
/// additionally suppressed when the screen is too narrow — except that
/// `panel_forced` (an explicit user un-hide on a small screen) overrides the
/// panel's threshold so it keeps its readable width, up to nearly the full
/// screen (the clamp below always leaves the center ≥ 1 column).
/// `panel_expanded` (a drilled-in diff / file preview) widens the panel to a
/// reading width of ~2/3 of the window while the view is open.
#[allow(clippy::too_many_arguments)]
pub fn compute_full(
    cols: usize,
    rows: usize,
    want_sidebar: bool,
    want_panel: bool,
    panel_forced: bool,
    panel_expanded: bool,
    sidebar_cols: usize,
    want_strip: bool,
    strip_ratio: f32,
) -> ChromeLayout {
    let show_sidebar = want_sidebar && cols >= SIDEBAR_MIN_COLS;
    let show_panel = want_panel && (cols >= PANEL_MIN_COLS || panel_forced);

    let masthead = Rect {
        x: 0,
        y: 0,
        cols,
        rows: MASTHEAD_ROWS.min(rows),
    };
    let status_y = rows.saturating_sub(STATUSBAR_ROWS);
    let statusbar = Rect {
        x: 0,
        y: status_y,
        cols,
        rows: rows.min(STATUSBAR_ROWS),
    };

    // A 1-row horizontal divider caps the columns directly below the masthead
    // (skipped on terminals too short to spare a row).
    let divider_rows = rows.saturating_sub(masthead.rows + STATUSBAR_ROWS).min(1);
    let divider = Rect {
        x: 0,
        y: masthead.rows,
        cols,
        rows: divider_rows,
    };

    // The band below the divider: sidebar, panel, strip, and center all live
    // here, with the column tops aligned at `band_y`.
    let band_y = masthead.rows + divider_rows;
    let band_rows = rows.saturating_sub(band_y + STATUSBAR_ROWS);

    // Clamp the surface widths so the center keeps ≥ 1 column after the
    // 1-col separators between sidebar|center and center|panel are reserved.
    let mut left = if show_sidebar {
        sidebar_cols.clamp(SIDEBAR_MIN_WIDTH, SIDEBAR_MAX_WIDTH)
    } else {
        0
    };
    let mut right = if show_panel {
        if panel_expanded {
            // A drilled-in document view earns a reading width: ~2/3 of the
            // window (never less than the resting width); the clamp below
            // still trades it back if the screen can't afford it.
            (cols * 2 / 3).max(PANEL_COLS)
        } else {
            PANEL_COLS
        }
    } else {
        0
    };
    let used = |l: usize, r: usize| {
        l + r + usize::from(l > 0) + usize::from(r > 0) + 1 // + min center
    };
    while used(left, right) > cols && (left > 0 || right > 0) {
        if right >= left && right > 0 {
            right = right.saturating_sub(1);
        } else if left > 0 {
            left = left.saturating_sub(1);
        } else {
            break;
        }
    }
    let sep_left_w = usize::from(left > 0);
    let sep_right_w = usize::from(right > 0);

    let sidebar = (left > 0).then_some(Rect {
        x: 0,
        y: band_y,
        cols: left,
        rows: band_rows,
    });
    let sep_left = (left > 0).then_some(left);
    let panel_x = cols.saturating_sub(right);
    let panel = (right > 0).then_some(Rect {
        x: panel_x,
        y: band_y,
        cols: right,
        rows: band_rows,
    });
    let sep_right = (right > 0).then_some(panel_x.saturating_sub(1));

    let center_x = left + sep_left_w;
    let center_cols = cols.saturating_sub(left + sep_left_w + sep_right_w + right);

    // The center column's tab bar sits directly below the divider, level with
    // the sidebar header and the panel switcher.
    let tabs_rows = band_rows.min(1);
    let center_tabs = Rect {
        x: center_x,
        y: band_y,
        cols: center_cols,
        rows: tabs_rows,
    };

    // Carve a top strip out of the center column when wanted and the band can
    // spare the rows (strip ≥ STRIP_MIN_ROWS while leaving center ≥ STRIP_MIN_ROWS).
    let column_rows = band_rows.saturating_sub(tabs_rows);
    let strip_rows = if want_strip {
        let r = (column_rows as f32 * strip_ratio.clamp(0.0, 0.9)).round() as usize;
        let r = r.max(STRIP_MIN_ROWS);
        if column_rows >= r + STRIP_MIN_ROWS {
            r
        } else {
            0
        }
    } else {
        0
    };

    let strip = (strip_rows > 0).then_some(Rect {
        x: center_x,
        y: band_y + tabs_rows,
        cols: center_cols,
        rows: strip_rows,
    });
    let center = Rect {
        x: center_x,
        y: band_y + tabs_rows + strip_rows,
        cols: center_cols,
        rows: band_rows.saturating_sub(tabs_rows + strip_rows),
    };

    ChromeLayout {
        masthead,
        divider,
        statusbar,
        sidebar,
        panel,
        sep_left,
        sep_right,
        center_tabs,
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
            l.masthead,
            Rect {
                x: 0,
                y: 0,
                cols: 160,
                rows: 1
            }
        );
        assert_eq!(l.statusbar.y, 39);
        // A 1-row divider caps the columns just under the masthead.
        assert_eq!(
            l.divider,
            Rect {
                x: 0,
                y: 1,
                cols: 160,
                rows: 1
            }
        );
        let sb = l.sidebar.unwrap();
        let pn = l.panel.unwrap();
        assert_eq!(sb.cols, SIDEBAR_COLS);
        assert_eq!(pn.cols, PANEL_COLS);
        // All three column tops align directly below the divider (the center
        // column opens with its tab bar; panes start one row lower).
        assert_eq!(sb.y, 2);
        assert_eq!(pn.y, 2);
        assert_eq!(l.center_tabs.y, 2);
        assert_eq!(l.center.y, 3);
        // Separators sit between the columns; everything tiles the width with
        // a 1-col gutter each side of the center.
        assert_eq!(l.sep_left, Some(SIDEBAR_COLS));
        assert_eq!(l.sep_right, Some(160 - PANEL_COLS - 1));
        assert_eq!(sb.cols + 1 + l.center.cols + 1 + pn.cols, 160);
        assert_eq!(l.center.x, SIDEBAR_COLS + 1);
        assert_eq!(pn.x, 160 - PANEL_COLS);
        assert_eq!(l.center.rows, 36);
    }

    #[test]
    fn center_tabs_sit_below_the_divider_spanning_the_center() {
        let l = compute(160, 40, true, true);
        let stats = l.masthead_stats_row();
        assert_eq!((stats.y, stats.rows), (0, 1));
        assert_eq!(stats.cols, 160);
        // The tab bar tops the center column, level with the other headers.
        assert_eq!(l.center_tabs.y, 2);
        assert_eq!(l.center_tabs.rows, 1);
        assert_eq!(l.center_tabs.x, l.center.x);
        assert_eq!(l.center_tabs.cols, l.center.cols);
        assert_eq!(l.center.y, 3, "panes start below the tab bar");
    }

    #[test]
    fn expanded_panel_takes_a_reading_width_and_retracts() {
        let resting = compute_full(160, 40, true, true, false, false, SIDEBAR_COLS, false, 0.0);
        let expanded = compute_full(160, 40, true, true, false, true, SIDEBAR_COLS, false, 0.0);
        assert_eq!(resting.panel.unwrap().cols, PANEL_COLS);
        // ~2/3 of the window while a document view is open.
        assert_eq!(expanded.panel.unwrap().cols, 160 * 2 / 3);
        assert!(expanded.center.cols >= 1);
        // On a small forced screen the clamp still leaves a live center.
        let tiny = compute_full(60, 20, false, true, true, true, SIDEBAR_COLS, false, 0.0);
        assert!(tiny.panel.unwrap().cols >= PANEL_COLS);
        assert!(tiny.center.cols >= 1);
    }

    #[test]
    fn repeated_layout_compute_preserves_panel_and_tab_strip_geometry() {
        let first = compute(160, 40, true, true);
        let second = compute(160, 40, true, true);

        assert_eq!(first.panel.unwrap().cols, PANEL_COLS);
        assert_eq!(second.panel.unwrap().cols, PANEL_COLS);
        assert_eq!(first.center, second.center);
        assert_eq!(first.center_tabs, second.center_tabs);
    }

    #[test]
    fn masthead_clamps_on_tiny_heights() {
        // rows=1: the masthead takes the only row; no divider/tab-bar rows.
        let l = compute(160, 1, true, true);
        assert_eq!(l.masthead.rows, 1);
        assert_eq!(l.divider.rows, 0);
        assert_eq!(l.center_tabs.rows, 0);

        // rows=2: masthead + statusbar; the band (and center) is empty but
        // never negative.
        let l = compute(160, 2, true, true);
        assert_eq!(l.masthead.rows, 1);
        assert_eq!(l.divider.rows, 0);
        assert_eq!(l.center.rows, 0);
    }

    #[test]
    fn narrow_screen_auto_hides_panel_then_sidebar() {
        // 90 cols: below the panel threshold (100) but above sidebar (76).
        let l = compute(90, 40, true, true);
        assert!(l.panel.is_none(), "panel should auto-hide under 100 cols");
        assert!(l.sidebar.is_some(), "sidebar still shown at 90 cols");

        // 70 cols: both auto-hide; no separators, center spans full width.
        let l = compute(70, 40, true, true);
        assert!(l.panel.is_none());
        assert!(l.sidebar.is_none());
        assert_eq!(l.sep_left, None);
        assert_eq!(l.sep_right, None);
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
        // 40 rows: the center column below the tab bar is 36 rows
        // (masthead 1 + divider 1 + tab bar 1 + statusbar 1). 20% → 7.
        let l = compute_with_strip(160, 40, true, true, true, 0.2);
        let strip = l.strip.expect("strip visible");
        assert_eq!(strip.y, 3, "strip sits directly below the tab bar");
        assert_eq!(strip.x, l.center.x, "strip spans the center columns");
        assert_eq!(strip.cols, l.center.cols);
        // Strip + center exactly tile the column (no gap/overlap).
        assert_eq!(strip.rows + l.center.rows, 36);
        assert_eq!(l.center.y, strip.y + strip.rows);
        assert_eq!(strip.rows, 7); // round(36 * 0.2)
    }

    #[test]
    fn strip_absent_when_not_wanted() {
        let l = compute_with_strip(160, 40, true, true, false, 0.2);
        assert!(l.strip.is_none());
        assert_eq!(l.center.y, 3);
        assert_eq!(l.center.rows, 36);
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
            + usize::from(l.sep_left.is_some())
            + l.center.cols
            + usize::from(l.sep_right.is_some())
            + l.panel.map(|r| r.cols).unwrap_or(0);
        assert_eq!(used, SIDEBAR_MIN_COLS);
    }
}
