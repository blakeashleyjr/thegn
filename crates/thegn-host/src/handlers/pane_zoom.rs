//! Pane zoom controls: the two "grow the focused pane" levels driven by
//! `Ctrl+Alt+z`, extracted from the pinned `run.rs` god-file.
//!
//! One key cycles three center states:
//!   1. **Tiled** — the normal split layout.
//!   2. **Maximized** — the focused pane fills the whole center region while all
//!      chrome (sidebar, panel, strip, bars) stays. Sibling panes are hidden but
//!      their PTYs live on. Follows focus (tmux `prefix z`).
//!   3. **Fullscreen** — the focused pane takes the whole window; the
//!      sidebar/panel/strip are suppressed and only the (configurable) top/bottom
//!      bars remain.
//!
//! State lives in two loop locals: `maximized: bool` (level 2) and the existing
//! `zoom: Option<Zone>` — which now only ever holds `Center` (level 3) or
//! `Sidebar`/`Panel` (the older zone-zoom). The two levels are mutually
//! exclusive. [`compute_chrome`] maps that state to the chrome grid and
//! [`grown_tree`] collapses the center to the focused leaf for both grown levels.

use crate::center::{CenterTree, PaneId};
use crate::focus::{FocusState, Zone};
use crate::layout;

/// Compute the chrome cross for the current window + zoom state. A `Center`
/// zoom (level-3 fullscreen) suppresses the sidebar/panel/strip and keeps the
/// top/bottom bars per `want_masthead`/`want_statusbar`; a `Sidebar`/`Panel`
/// zoom widens that one zone; otherwise the normal layout is computed. Level-2
/// maximize needs no branch here — its chrome is the normal tiled grid; only the
/// center tree changes (see [`grown_tree`]).
#[allow(clippy::too_many_arguments)]
pub(crate) fn compute_chrome(
    cols: usize,
    rows: usize,
    want_sidebar: bool,
    want_panel: bool,
    panel_forced: bool,
    panel_width: layout::PanelWidth,
    sidebar_cols: usize,
    zoom: Option<Zone>,
    supervisor: &crate::pins::PinSupervisor,
    // Bottom drawer reservation: `drawer_rows` (> 0) carves a slice off the band
    // bottom; `drawer_full_width` spans the whole width vs. the center column.
    // Zoom suppresses the drawer along with the rest of the chrome.
    drawer_rows: usize,
    drawer_full_width: bool,
    // Which bars survive a full-window (`Center`) zoom — the `[ui]`
    // `fullscreen_keep_masthead` / `fullscreen_keep_statusbar` flags.
    want_masthead: bool,
    want_statusbar: bool,
) -> layout::ChromeLayout {
    let strip = supervisor.strip_visible() && supervisor.has_strip_panes();
    match zoom {
        // Center zoom: full-width center (chrome columns suppressed); the
        // focused pane alone renders into it (see the render block). The
        // top/bottom bars stay per config, letting the center reclaim their
        // row(s) when dropped.
        Some(Zone::Center) => layout::compute_full_bars(
            cols,
            rows,
            false,
            false,
            false,
            layout::PanelWidth::Normal,
            sidebar_cols,
            false,
            0.0,
            0,
            false,
            want_masthead,
            want_statusbar,
        ),
        // Sidebar / panel zoom: the zone takes (nearly) the whole width; a
        // 1-col center keeps the pane math alive.
        Some(Zone::Sidebar) => {
            let mut l = layout::compute_full(
                cols,
                rows,
                true,
                false,
                false,
                layout::PanelWidth::Normal,
                sidebar_cols,
                false,
                0.0,
                0,
                false,
            );
            let w = cols.saturating_sub(2).max(1);
            if let Some(sb) = l.sidebar.as_mut() {
                sb.cols = w;
            }
            l.sep_left = Some(w);
            for r in [&mut l.center_tabs, &mut l.center] {
                r.x = (w + 1).min(cols.saturating_sub(1));
                r.cols = 1;
            }
            l.strip = None;
            l
        }
        Some(Zone::Panel) => {
            let mut l = layout::compute_full(
                cols,
                rows,
                false,
                true,
                true,
                layout::PanelWidth::Full,
                sidebar_cols,
                false,
                0.0,
                0,
                false,
            );
            let w = cols.saturating_sub(2).max(1);
            if let Some(pn) = l.panel.as_mut() {
                pn.x = cols - w;
                pn.cols = w;
            }
            l.sep_right = Some((cols - w).saturating_sub(1));
            for r in [&mut l.center_tabs, &mut l.center] {
                r.x = 0;
                r.cols = 1;
            }
            l.strip = None;
            l
        }
        // The bars are single rows, and the drawer / corner overlay are never
        // zoom targets — zooming them makes no sense; fall back to the normal
        // layout (zoom is never set to these zones; this arm is for exhaustiveness).
        Some(Zone::Masthead)
        | Some(Zone::Statusbar)
        | Some(Zone::Drawer)
        | Some(Zone::Corner)
        | None => layout::compute_full(
            cols,
            rows,
            want_sidebar,
            want_panel,
            panel_forced,
            panel_width,
            sidebar_cols,
            strip,
            supervisor.strip_ratio(),
            drawer_rows,
            drawer_full_width,
        ),
    }
}

/// `Ctrl+Alt+z`. On the center zone, cycle tiled → maximize → fullscreen →
/// tiled. On the sidebar/panel, the older two-state zone-zoom toggle. On a
/// single-row bar there is nothing to grow — leave the state untouched. Returns
/// the status line to show (empty clears it). The caller recomputes the chrome
/// and relayouts.
pub(crate) fn cycle_or_zoom(
    zoom: &mut Option<Zone>,
    maximized: &mut bool,
    focus: &FocusState,
) -> String {
    if focus.bar() {
        return String::new();
    }
    if focus.zone == Zone::Center {
        let fullscreen = *zoom == Some(Zone::Center);
        if *maximized {
            // maximize-in-chrome → full-window fullscreen
            *maximized = false;
            *zoom = Some(Zone::Center);
            "Fullscreen — Ctrl+Alt+z to restore".into()
        } else if fullscreen {
            // fullscreen → tiled
            *zoom = None;
            String::new()
        } else {
            // tiled → maximize-in-chrome
            *maximized = true;
            *zoom = None;
            "Maximized — Ctrl+Alt+z for fullscreen, again to restore".into()
        }
    } else {
        // A grown center and a zone zoom are mutually exclusive.
        *maximized = false;
        if zoom.is_none() {
            *zoom = Some(focus.zone);
            "Zoomed — Ctrl+Alt+z to restore".into()
        } else {
            *zoom = None;
            String::new()
        }
    }
}

/// The center tree to relayout: both grown levels (maximize and center
/// fullscreen) collapse to just the focused pane as a single leaf; otherwise the
/// tab's real split tree. Recomputed on every relayout, so a focus move while
/// grown re-selects the newly focused pane (tmux-style follow-focus).
pub(crate) fn grown_tree(
    zoom: Option<Zone>,
    maximized: bool,
    focused: PaneId,
    tab_tree: Option<CenterTree>,
) -> CenterTree {
    if maximized || zoom == Some(Zone::Center) {
        CenterTree::Leaf(focused)
    } else {
        tab_tree.unwrap_or(CenterTree::Leaf(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn focus(zone: Zone) -> FocusState {
        FocusState {
            zone,
            locked: false,
        }
    }

    #[test]
    fn center_cycles_tiled_maximize_fullscreen_tiled() {
        let mut zoom = None;
        let mut max = false;
        let f = focus(Zone::Center);

        // tiled → maximize
        let s = cycle_or_zoom(&mut zoom, &mut max, &f);
        assert!(max && zoom.is_none());
        assert!(s.contains("Maximized"));

        // maximize → fullscreen
        let s = cycle_or_zoom(&mut zoom, &mut max, &f);
        assert!(!max && zoom == Some(Zone::Center));
        assert!(s.contains("Fullscreen"));

        // fullscreen → tiled
        let s = cycle_or_zoom(&mut zoom, &mut max, &f);
        assert!(!max && zoom.is_none());
        assert!(s.is_empty());
    }

    #[test]
    fn sidebar_panel_do_a_two_state_zone_zoom_and_clear_maximize() {
        // A stray maximize from the center is cleared when zooming a side zone.
        let mut zoom = None;
        let mut max = true;
        let f = focus(Zone::Sidebar);
        let s = cycle_or_zoom(&mut zoom, &mut max, &f);
        assert!(!max && zoom == Some(Zone::Sidebar));
        assert!(s.contains("Zoomed"));
        // Toggle off.
        let s = cycle_or_zoom(&mut zoom, &mut max, &f);
        assert!(zoom.is_none() && s.is_empty());

        // Panel behaves the same.
        let f = focus(Zone::Panel);
        cycle_or_zoom(&mut zoom, &mut max, &f);
        assert_eq!(zoom, Some(Zone::Panel));
    }

    #[test]
    fn bars_are_not_growable_and_leave_state_untouched() {
        let mut zoom = Some(Zone::Center);
        let mut max = false;
        for z in [Zone::Masthead, Zone::Statusbar] {
            let before = zoom;
            let s = cycle_or_zoom(&mut zoom, &mut max, &focus(z));
            assert_eq!(zoom, before, "bar zoom is a no-op");
            assert!(!max);
            assert!(s.is_empty());
        }
    }

    #[test]
    fn grown_tree_collapses_to_focused_leaf_for_both_levels() {
        let split = CenterTree::single(7); // stand-in "real" tree
        // Maximized → just the focused leaf, ignoring the tab tree.
        assert_eq!(
            grown_tree(None, true, 3, Some(split.clone())),
            CenterTree::Leaf(3)
        );
        // Center fullscreen → same.
        assert_eq!(
            grown_tree(Some(Zone::Center), false, 5, Some(split.clone())),
            CenterTree::Leaf(5)
        );
        // Tiled → the tab's real tree (or a fallback leaf when absent).
        assert_eq!(grown_tree(None, false, 9, Some(split.clone())), split);
        assert_eq!(grown_tree(None, false, 9, None), CenterTree::Leaf(0));
        // A side-zone zoom does NOT collapse the center.
        assert_eq!(
            grown_tree(Some(Zone::Sidebar), false, 9, Some(split.clone())),
            split
        );
    }
}
