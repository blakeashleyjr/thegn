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
pub const SIDEBAR_COLS: usize = 26; // was 20; ~16% at 160 cols — room for dynamic titles
pub const PANEL_COLS: usize = 44; // ~27% at 160 cols

/// The strip is suppressed when the band is too short to give it ≥ this many rows
/// while leaving the center at least this many — i.e. the strip never starves the
/// center. (Each pin also keeps a 1-row label header.)
pub const STRIP_MIN_ROWS: usize = 3;

/// The right panel's width state, cycled by `e` while the panel is focused.
/// `Normal` is the resting reading width; `Half` claims half the window; `Full`
/// fills the whole band (sidebar + center suppressed), bounded only by the top
/// masthead and bottom statusbar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PanelWidth {
    #[default]
    Normal,
    Half,
    Full,
}

impl PanelWidth {
    /// Advance Normal → Half → Full → Normal.
    pub fn cycle(self) -> Self {
        match self {
            PanelWidth::Normal => PanelWidth::Half,
            PanelWidth::Half => PanelWidth::Full,
            PanelWidth::Full => PanelWidth::Normal,
        }
    }

    /// Whether the panel is widened past its resting size (drives deep content).
    pub fn is_expanded(self) -> bool {
        !matches!(self, PanelWidth::Normal)
    }

    /// Stable key for persistence.
    pub fn as_key(self) -> &'static str {
        match self {
            PanelWidth::Normal => "normal",
            PanelWidth::Half => "half",
            PanelWidth::Full => "full",
        }
    }

    pub fn from_key(s: &str) -> Self {
        match s {
            "half" => PanelWidth::Half,
            "full" => PanelWidth::Full,
            _ => PanelWidth::Normal,
        }
    }
}

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
    /// The bottom drawer's content rect (where the file-manager PTY composites),
    /// when visible. `"full"` width spans the whole band bottom (sidebar/center/
    /// panel all shorten); `"center"` spans only the center column. `None` when
    /// closed or the band is too short to spare it.
    pub drawer: Option<Rect>,
    /// The 1-row horizontal rule directly above the drawer (spans the drawer's
    /// columns). `None` whenever `drawer` is `None`.
    pub drawer_divider: Option<Rect>,
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
        PanelWidth::Normal,
        SIDEBAR_COLS,
        false,
        0.0,
        0,
        false,
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
        PanelWidth::Normal,
        sidebar_cols,
        false,
        0.0,
        0,
        false,
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
        PanelWidth::Normal,
        SIDEBAR_COLS,
        want_strip,
        strip_ratio,
        0,
        false,
    )
}

/// Compute the chrome cross reserving a bottom drawer of `drawer_rows` rows
/// (full-width or center-only) when the band can spare it. (Convenience used by
/// tests; the live loop calls [`compute_full`].)
#[allow(dead_code)]
pub fn compute_with_drawer(
    cols: usize,
    rows: usize,
    want_sidebar: bool,
    want_panel: bool,
    drawer_rows: usize,
    drawer_full_width: bool,
) -> ChromeLayout {
    compute_full(
        cols,
        rows,
        want_sidebar,
        want_panel,
        false,
        PanelWidth::Normal,
        SIDEBAR_COLS,
        false,
        0.0,
        drawer_rows,
        drawer_full_width,
    )
}

/// The full chrome-cross computation: explicit sidebar width *and* optional top
/// strip. `want_sidebar`/`want_panel` are the user's toggle state; each is
/// additionally suppressed when the screen is too narrow — except that
/// `panel_forced` (an explicit user un-hide on a small screen) overrides the
/// panel's threshold so it keeps its readable width, up to nearly the full
/// screen (the clamp below always leaves the center ≥ 1 column).
/// `panel_width` (cycled by the accordion's `e` key) widens the panel: `Half`
/// claims half the window, `Full` fills the whole band (sidebar suppressed).
/// `drawer_rows` (> 0) reserves a bottom drawer: `drawer_full_width` spans the
/// whole band bottom — sidebar/center/panel all shorten — while a center-only
/// drawer is carved from the bottom of the center column (a mirror of the top
/// strip). The drawer is suppressed when the band can't spare it while leaving
/// the columns a usable height.
#[allow(clippy::too_many_arguments)]
pub fn compute_full(
    cols: usize,
    rows: usize,
    want_sidebar: bool,
    want_panel: bool,
    panel_forced: bool,
    panel_width: PanelWidth,
    sidebar_cols: usize,
    want_strip: bool,
    strip_ratio: f32,
    drawer_rows: usize,
    drawer_full_width: bool,
) -> ChromeLayout {
    // A full-width panel claims the whole band — the sidebar steps aside.
    let show_sidebar = want_sidebar && cols >= SIDEBAR_MIN_COLS && panel_width != PanelWidth::Full;
    let show_panel = want_panel && (cols >= PANEL_MIN_COLS || panel_forced);

    let masthead = Rect {
        x: 0,
        y: 0,
        cols,
        rows: MASTHEAD_ROWS.min(rows),
    };
    let status_rows = rows.saturating_sub(masthead.rows).min(STATUSBAR_ROWS);
    let status_y = rows.saturating_sub(status_rows);
    let statusbar = Rect {
        x: 0,
        y: status_y,
        cols,
        rows: status_rows,
    };

    // A 1-row horizontal divider caps the columns directly below the masthead
    // (skipped on terminals too short to spare a row).
    let divider_rows = rows.saturating_sub(masthead.rows + statusbar.rows).min(1);
    let divider = Rect {
        x: 0,
        y: masthead.rows,
        cols,
        rows: divider_rows,
    };

    // The band below the divider: sidebar, panel, strip, and center all live
    // here, with the column tops aligned at `band_y`.
    let band_y = masthead.rows + divider_rows;
    let band_rows = rows.saturating_sub(band_y + statusbar.rows);

    // A full-width bottom drawer claims a horizontal slice of the band bottom
    // (its own 1-row divider + `drawer_rows`) before the columns lay out, so
    // sidebar/center/panel all shorten together. Suppressed when the band can't
    // spare it while leaving the columns ≥ STRIP_MIN_ROWS. A center-only drawer
    // (`!drawer_full_width`) leaves the full band to the columns and is carved
    // from the center column further down.
    let full_drawer_rows =
        if drawer_rows > 0 && drawer_full_width && band_rows >= drawer_rows + 1 + STRIP_MIN_ROWS {
            drawer_rows
        } else {
            0
        };
    let drawer_reserve = if full_drawer_rows > 0 {
        full_drawer_rows + 1
    } else {
        0
    };
    // Rows available to the columns (sidebar / center / panel) after the
    // full-width drawer slice, if any.
    let col_rows = band_rows.saturating_sub(drawer_reserve);

    // Clamp the surface widths so the center keeps ≥ 1 column after the
    // 1-col separators between sidebar|center and center|panel are reserved.
    // No upper cap here: the fine-nudge path (`<` / `>`) is already clamped to
    // SIDEBAR_MAX_WIDTH before it is stored, and the Wide expand intentionally
    // asks for ~half the window. The `used()` shrink loop below trades width
    // back so the center keeps its mandatory column.
    let mut left = if show_sidebar {
        sidebar_cols.max(SIDEBAR_MIN_WIDTH)
    } else {
        0
    };
    let mut right = if show_panel {
        match panel_width {
            // Resting reading width.
            PanelWidth::Normal => PANEL_COLS,
            // Half the window (never below the resting width on a small screen).
            PanelWidth::Half => (cols / 2).max(PANEL_COLS),
            // The whole band: ask for everything and let the clamp below trade
            // it back to leave the center its mandatory single column.
            PanelWidth::Full => cols.saturating_sub(2),
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
        rows: col_rows,
    });
    let sep_left = (left > 0).then_some(left);
    let panel_x = cols.saturating_sub(right);
    let panel = (right > 0).then_some(Rect {
        x: panel_x,
        y: band_y,
        cols: right,
        rows: col_rows,
    });
    let sep_right = (right > 0).then_some(panel_x.saturating_sub(1));

    let center_x = left + sep_left_w;
    let center_cols = cols.saturating_sub(left + sep_left_w + sep_right_w + right);

    // The center column's tab bar sits directly below the divider, level with
    // the sidebar header and the panel switcher.
    let tabs_rows = col_rows.min(1);
    let center_tabs = Rect {
        x: center_x,
        y: band_y,
        cols: center_cols,
        rows: tabs_rows,
    };

    // Carve a top strip out of the center column when wanted and the band can
    // spare the rows (strip ≥ STRIP_MIN_ROWS while leaving center ≥ STRIP_MIN_ROWS).
    let column_rows = col_rows.saturating_sub(tabs_rows);
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
    let mut center = Rect {
        x: center_x,
        y: band_y + tabs_rows + strip_rows,
        cols: center_cols,
        rows: col_rows.saturating_sub(tabs_rows + strip_rows),
    };

    // The bottom drawer. Full-width: the slice reserved up front below the
    // columns, spanning the whole width. Center-only: carved from the bottom of
    // the center column here (its own 1-row divider + `drawer_rows`), suppressed
    // when the center can't spare it while keeping ≥ STRIP_MIN_ROWS.
    let (drawer, drawer_divider) = if full_drawer_rows > 0 {
        let div_y = band_y + col_rows;
        (
            Some(Rect {
                x: 0,
                y: div_y + 1,
                cols,
                rows: full_drawer_rows,
            }),
            Some(Rect {
                x: 0,
                y: div_y,
                cols,
                rows: 1,
            }),
        )
    } else if drawer_rows > 0
        && !drawer_full_width
        && center.rows >= drawer_rows + 1 + STRIP_MIN_ROWS
    {
        center.rows -= drawer_rows + 1;
        let div_y = center.y + center.rows;
        (
            Some(Rect {
                x: center_x,
                y: div_y + 1,
                cols: center_cols,
                rows: drawer_rows,
            }),
            Some(Rect {
                x: center_x,
                y: div_y,
                cols: center_cols,
                rows: 1,
            }),
        )
    } else {
        (None, None)
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
        drawer,
        drawer_divider,
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
    fn wide_sidebar_exceeds_the_fine_nudge_cap_and_keeps_the_center() {
        // The Wide expand (`e`) passes ~half the window as the sidebar width;
        // it must not be capped at SIDEBAR_MAX_WIDTH, yet the center keeps ≥ 1
        // column and the panel still tiles the remaining width.
        let l = compute_with_width(160, 40, true, true, 80);
        let sb = l.sidebar.unwrap();
        assert!(
            sb.cols > SIDEBAR_MAX_WIDTH,
            "wide sidebar should exceed the fine-nudge cap: {}",
            sb.cols
        );
        assert_eq!(sb.cols, 80);
        assert!(l.center.cols >= 1, "center keeps its mandatory column");
        let pn = l.panel.unwrap();
        assert_eq!(sb.cols + 1 + l.center.cols + 1 + pn.cols, 160);
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
    fn panel_width_cycle_normal_half_full() {
        let resting = compute_full(
            160,
            40,
            true,
            true,
            false,
            PanelWidth::Normal,
            SIDEBAR_COLS,
            false,
            0.0,
            0,
            false,
        );
        let half = compute_full(
            160,
            40,
            true,
            true,
            false,
            PanelWidth::Half,
            SIDEBAR_COLS,
            false,
            0.0,
            0,
            false,
        );
        let full = compute_full(
            160,
            40,
            true,
            true,
            false,
            PanelWidth::Full,
            SIDEBAR_COLS,
            false,
            0.0,
            0,
            false,
        );
        assert_eq!(resting.panel.unwrap().cols, PANEL_COLS);
        // Half claims ~half the window (≥ resting), center stays alive.
        assert!(half.panel.unwrap().cols >= 160 / 2);
        assert!(half.center.cols >= 1);
        // Full fills the band: the sidebar steps aside and the panel takes
        // nearly everything, leaving the center its mandatory single column.
        assert!(full.sidebar.is_none(), "full-width panel hides the sidebar");
        assert!(full.panel.unwrap().cols >= 160 - 4);
        assert!(full.center.cols >= 1);
        // On a small forced screen the clamp still leaves a live center.
        let tiny = compute_full(
            60,
            20,
            false,
            true,
            true,
            PanelWidth::Half,
            SIDEBAR_COLS,
            false,
            0.0,
            0,
            false,
        );
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
        // rows=1: the masthead takes the only row; the statusbar must not
        // overlap it or the first frame flashes the wrong content on tiny PTYs.
        let l = compute(160, 1, true, true);
        assert_eq!(l.masthead.rows, 1);
        assert_eq!(l.statusbar.rows, 0);
        assert_eq!(l.divider.rows, 0);
        assert_eq!(l.center_tabs.rows, 0);
        assert_eq!(l.center.rows, 0);

        // rows=2: masthead + statusbar; the band (and center) is empty but
        // never negative.
        let l = compute(160, 2, true, true);
        assert_eq!(l.masthead.rows, 1);
        assert_eq!(l.statusbar.rows, 1);
        assert_eq!(l.statusbar.y, 1);
        assert_eq!(l.divider.rows, 0);
        assert_eq!(l.center.rows, 0);
    }

    #[test]
    fn tiny_layout_rects_do_not_overlap_when_they_have_area() {
        fn overlaps(a: Rect, b: Rect) -> bool {
            a.cols > 0
                && a.rows > 0
                && b.cols > 0
                && b.rows > 0
                && a.x < b.x + b.cols
                && b.x < a.x + a.cols
                && a.y < b.y + b.rows
                && b.y < a.y + a.rows
        }

        // Sweep across heights and both drawer widths so the drawer/divider
        // rects never collide with the rest of the cross at tiny sizes.
        for rows in 0..=12 {
            for (drawer_rows, full) in [(0, false), (3, true), (3, false), (8, true)] {
                let l = compute_with_drawer(24, rows, true, true, drawer_rows, full);
                let named = [
                    ("masthead", Some(l.masthead)),
                    ("divider", Some(l.divider)),
                    ("statusbar", Some(l.statusbar)),
                    ("tabs", Some(l.center_tabs)),
                    ("center", Some(l.center)),
                    ("sidebar", l.sidebar),
                    ("panel", l.panel),
                    ("drawer", l.drawer),
                    ("drawer_divider", l.drawer_divider),
                ];
                for (i, (an, a)) in named.iter().enumerate() {
                    for (bn, b) in named.iter().skip(i + 1) {
                        if let (Some(a), Some(b)) = (a, b) {
                            assert!(
                                !overlaps(*a, *b),
                                "{an} overlaps {bn} at rows={rows} drawer={drawer_rows} full={full}: {l:?}"
                            );
                        }
                    }
                }
            }
        }
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
    fn full_width_drawer_reserves_band_bottom_and_shrinks_all_columns() {
        // 40 rows: band is 38 (masthead 1 + statusbar 1). A 10-row full-width
        // drawer + its 1-row divider claim 11 rows from the band bottom; the
        // sidebar, panel, and center all shorten by the same amount.
        let l = compute_with_drawer(160, 40, true, true, 10, true);
        let drawer = l.drawer.expect("drawer visible");
        let div = l.drawer_divider.expect("divider visible");
        assert_eq!(drawer.x, 0);
        assert_eq!(drawer.cols, 160, "full-width drawer spans the terminal");
        assert_eq!(drawer.rows, 10);
        // Drawer sits flush above the statusbar; divider directly above it.
        assert_eq!(drawer.y + drawer.rows, l.statusbar.y);
        assert_eq!(div.y + div.rows, drawer.y);
        assert_eq!(div.cols, 160);
        // All three columns end at the divider — they shorten together.
        let sb = l.sidebar.unwrap();
        let pn = l.panel.unwrap();
        assert_eq!(sb.y + sb.rows, div.y);
        assert_eq!(pn.y + pn.rows, div.y);
        assert_eq!(l.center.y + l.center.rows, div.y);
    }

    #[test]
    fn center_only_drawer_shrinks_only_center() {
        // The sidebar and panel keep their full band height; only the center
        // column gives up rows to the drawer (a mirror of the top strip).
        let full_band = compute(160, 40, true, true);
        let l = compute_with_drawer(160, 40, true, true, 10, false);
        let drawer = l.drawer.expect("drawer visible");
        let div = l.drawer_divider.expect("divider visible");
        assert_eq!(drawer.x, l.center.x, "center-only drawer spans the center");
        assert_eq!(drawer.cols, l.center.cols);
        assert_eq!(drawer.rows, 10);
        // Sidebar/panel keep full height; center shortened by drawer + divider.
        assert_eq!(l.sidebar.unwrap().rows, full_band.sidebar.unwrap().rows);
        assert_eq!(l.panel.unwrap().rows, full_band.panel.unwrap().rows);
        assert_eq!(l.center.rows, full_band.center.rows - 11);
        assert_eq!(l.center.y + l.center.rows, div.y);
        assert_eq!(div.y + div.rows, drawer.y);
        assert_eq!(drawer.y + drawer.rows, l.statusbar.y);
    }

    #[test]
    fn drawer_suppressed_when_band_too_short() {
        // A short band can't give the drawer its rows and keep the columns
        // alive: the drawer is dropped rather than overlapping.
        let l = compute_with_drawer(160, 6, true, true, 10, true);
        assert!(l.drawer.is_none());
        assert!(l.drawer_divider.is_none());
        assert!(l.center.rows >= 1);
        let l = compute_with_drawer(160, 6, true, true, 10, false);
        assert!(l.drawer.is_none());
        assert!(l.center.rows >= 1);
    }

    #[test]
    fn drawer_absent_when_zero_rows() {
        let l = compute_with_drawer(160, 40, true, true, 0, true);
        assert!(l.drawer.is_none());
        assert!(l.drawer_divider.is_none());
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
