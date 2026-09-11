//! Pane zoom controls: the two "grow the focused pane" levels driven by
//! `Ctrl+Alt+z`, extracted from the pinned `run.rs` god-file.
//!
//! One key cycles three center states ([`Grow`]):
//!   1. **Tiled** — the normal split layout.
//!   2. **Maximized** — the focused pane fills the whole center region while all
//!      chrome (sidebar, panel, strip, bars) stays. Follows focus (tmux
//!      `prefix z`).
//!   3. **Fullscreen** — the focused pane takes the whole window; the
//!      sidebar/panel/strip are suppressed and only the (configurable) top/bottom
//!      bars remain.
//!
//! In both grown levels the tab renders as a zellij-style stack: the focused
//! pane expanded, every sibling collapsed to a one-row title bar (PTYs live on).
//!
//! The level is **per tab** (`Tab::grow`), so leaving a zoomed worktree and
//! coming back restores the zoom and the pane you left. The loop keeps only the
//! older sidebar/panel zone-zoom in its `zoom: Option<Zone>` local;
//! [`effective_zoom`] folds the two into the zone [`compute_chrome`] grows.

use crate::center::{Branch, CenterTree, Dir, Grow, PaneId};
use crate::compositor::Rect;
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

/// `Ctrl+Alt+z`. On the center zone, cycle the active tab's `grow` through
/// tiled → maximize → fullscreen → tiled. On the sidebar/panel, the older
/// two-state zone-zoom toggle (the tab's level is left alone — it is remembered
/// for when focus returns). On a single-row bar there is nothing to grow — leave
/// the state untouched. Returns the status line to show (empty clears it). The
/// caller recomputes the chrome and relayouts.
pub(crate) fn cycle_or_zoom(
    zoom: &mut Option<Zone>,
    grow: &mut Grow,
    focus: &FocusState,
) -> String {
    if focus.bar() {
        return String::new();
    }
    if focus.zone == Zone::Center {
        // A grown center and a zone zoom are mutually exclusive.
        *zoom = None;
        match *grow {
            Grow::Tiled => {
                *grow = Grow::Maximized;
                "Maximized — Ctrl+Alt+z for fullscreen, again to restore".into()
            }
            Grow::Maximized => {
                *grow = Grow::Fullscreen;
                "Fullscreen — Ctrl+Alt+z to restore".into()
            }
            Grow::Fullscreen => {
                *grow = Grow::Tiled;
                String::new()
            }
        }
    } else if zoom.is_none() {
        *zoom = Some(focus.zone);
        "Zoomed — Ctrl+Alt+z to restore".into()
    } else {
        *zoom = None;
        String::new()
    }
}

/// The zone [`compute_chrome`] should grow: `Center` while the active tab is
/// fullscreen **and** the center owns focus, otherwise the sidebar/panel zone
/// zoom (if any). Non-destructive by design — a fullscreen tab whose focus has
/// moved to chrome simply shows that chrome (still stacked, i.e. maximized)
/// and goes fullscreen again when focus comes back. The loop recomputes the
/// chrome whenever this value changes, which covers every tab/worktree/workspace
/// switch path at once.
pub(crate) fn effective_zoom(
    zone_zoom: Option<Zone>,
    grow: Grow,
    focus_zone: Zone,
) -> Option<Zone> {
    if grow == Grow::Fullscreen && focus_zone == Zone::Center {
        Some(Zone::Center)
    } else {
        zone_zoom
    }
}

/// The active tab's zoom level (tiled when there is no tab).
pub(crate) fn active_grow(session: &crate::session::Session) -> Grow {
    session.active_tab().map(|t| t.grow).unwrap_or_default()
}

/// The center tree to lay out and render. A grown tab (maximized or
/// fullscreen) becomes a stack of all its panes with the focused one expanded
/// and the rest collapsed to title bars; a tiled tab is its real split tree.
/// Recomputed on every relayout, so a focus move while grown re-expands the
/// newly focused pane (tmux-style follow-focus).
pub(crate) fn grown_tree(grow: Grow, focused: PaneId, tab_tree: &CenterTree) -> CenterTree {
    if grow == Grow::Tiled {
        return tab_tree.clone();
    }
    let panes = tab_tree.pane_ids();
    let active = panes.iter().position(|p| *p == focused).unwrap_or(0);
    CenterTree::Stack { panes, active }
}

/// [`grown_tree`] for the session's active tab — the one tree the relayout,
/// the renderer and mouse hit-testing all share, so what you click is what is
/// drawn.
pub(crate) fn displayed_tree(session: &crate::session::Session) -> CenterTree {
    session
        .active_tab()
        .map(|t| grown_tree(t.grow, t.focused_pane, &t.center))
        .unwrap_or(CenterTree::Leaf(0))
}

/// The pane geometry directional focus routes over. While grown, a vertical
/// move walks the stack in its visual order (bar above = up, bar below = down),
/// falling off either end exactly like the top/bottom pane of a split; every
/// other case — and every horizontal move — uses the tab's real split layout.
/// Only relative geometry matters to the router, so the stack order is laid out
/// as a unit column rather than on the real center rect.
pub(crate) fn nav_layout(
    tab: Option<&crate::session::Tab>,
    center: Rect,
    vertical: bool,
) -> Vec<(PaneId, Rect)> {
    let Some(t) = tab else {
        return Vec::new();
    };
    if vertical && t.grow != Grow::Tiled {
        let ids = t.center.pane_ids();
        let rows = ids.len().max(1) * 2;
        return CenterTree::Split {
            dir: Dir::Col,
            children: ids
                .into_iter()
                .map(|p| Branch {
                    weight: 1.0,
                    child: CenterTree::Leaf(p),
                })
                .collect(),
        }
        .layout(Rect {
            x: 0,
            y: 0,
            cols: 2,
            rows,
        });
    }
    t.center.layout(center)
}

/// Focus a pane selected by a stack-bar click or directional keyboard routing.
/// A zoom-derived stack follows focus by itself; a stack in the tab's real tree
/// also needs its `active` index moved, or restoring tiled would display a
/// different pane from the one receiving keyboard input.
pub(crate) fn activate_stack_member(
    session: &mut crate::session::Session,
    focus: &mut FocusState,
    pane: PaneId,
) {
    if let Some(tab) = session.active_tab_mut() {
        tab.focused_pane = pane;
        tab.center.activate_stack_member(pane);
    }
    focus.zone = Zone::Center;
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

    /// 1 | (2 / 3): a row split whose right column is split vertically.
    fn three_panes() -> CenterTree {
        let mut t = CenterTree::single(1);
        t.split(1, Dir::Row, 2);
        t.split(2, Dir::Col, 3);
        t
    }

    fn center_rect() -> Rect {
        Rect {
            x: 0,
            y: 0,
            cols: 100,
            rows: 40,
        }
    }

    #[test]
    fn center_cycles_tiled_maximize_fullscreen_tiled() {
        let mut zoom = None;
        let mut grow = Grow::Tiled;
        let f = focus(Zone::Center);

        // tiled → maximize
        let s = cycle_or_zoom(&mut zoom, &mut grow, &f);
        assert!(grow == Grow::Maximized && zoom.is_none());
        assert!(s.contains("Maximized"));

        // maximize → fullscreen
        let s = cycle_or_zoom(&mut zoom, &mut grow, &f);
        assert!(grow == Grow::Fullscreen && zoom.is_none());
        assert!(s.contains("Fullscreen"));

        // fullscreen → tiled
        let s = cycle_or_zoom(&mut zoom, &mut grow, &f);
        assert!(grow == Grow::Tiled && zoom.is_none());
        assert!(s.is_empty());
    }

    #[test]
    fn sidebar_panel_do_a_two_state_zone_zoom_and_keep_the_tab_level() {
        // The tab's remembered level survives a side-zone zoom.
        let mut zoom = None;
        let mut grow = Grow::Maximized;
        let f = focus(Zone::Sidebar);
        let s = cycle_or_zoom(&mut zoom, &mut grow, &f);
        assert!(grow == Grow::Maximized && zoom == Some(Zone::Sidebar));
        assert!(s.contains("Zoomed"));
        // Toggle off.
        let s = cycle_or_zoom(&mut zoom, &mut grow, &f);
        assert!(zoom.is_none() && s.is_empty());

        // Panel behaves the same.
        let f = focus(Zone::Panel);
        cycle_or_zoom(&mut zoom, &mut grow, &f);
        assert_eq!(zoom, Some(Zone::Panel));
        // Growing the center clears a lingering zone zoom.
        cycle_or_zoom(&mut zoom, &mut grow, &focus(Zone::Center));
        assert_eq!(zoom, None);
    }

    #[test]
    fn bars_are_not_growable_and_leave_state_untouched() {
        let mut zoom = Some(Zone::Sidebar);
        let mut grow = Grow::Fullscreen;
        for z in [Zone::Masthead, Zone::Statusbar] {
            let s = cycle_or_zoom(&mut zoom, &mut grow, &focus(z));
            assert_eq!(zoom, Some(Zone::Sidebar), "bar zoom is a no-op");
            assert_eq!(grow, Grow::Fullscreen);
            assert!(s.is_empty());
        }
    }

    #[test]
    fn effective_zoom_is_fullscreen_only_while_the_center_owns_focus() {
        use Zone::*;
        // A fullscreen tab with a focused center suppresses the chrome.
        assert_eq!(effective_zoom(None, Grow::Fullscreen, Center), Some(Center));
        // Focus elsewhere → the chrome comes back (non-destructively).
        assert_eq!(effective_zoom(None, Grow::Fullscreen, Sidebar), None);
        // Maximize never grows the chrome; tiled neither.
        assert_eq!(effective_zoom(None, Grow::Maximized, Center), None);
        assert_eq!(effective_zoom(None, Grow::Tiled, Center), None);
        // A side-zone zoom passes through.
        assert_eq!(effective_zoom(Some(Panel), Grow::Tiled, Panel), Some(Panel));
        assert_eq!(
            effective_zoom(Some(Sidebar), Grow::Fullscreen, Sidebar),
            Some(Sidebar)
        );
    }

    #[test]
    fn grown_tree_stacks_every_pane_with_the_focused_one_expanded() {
        let split = three_panes();
        for grow in [Grow::Maximized, Grow::Fullscreen] {
            assert_eq!(
                grown_tree(grow, 2, &split),
                CenterTree::Stack {
                    panes: vec![1, 2, 3],
                    active: 1
                }
            );
        }
        // The focused pane is the one laid out; the others are bars.
        let t = grown_tree(Grow::Maximized, 3, &split);
        assert_eq!(t.layout(center_rect())[0].0, 3);
        assert_eq!(t.stack_bars(center_rect()).len(), 2);
        // A stale focus id expands the first pane rather than nothing.
        assert_eq!(
            grown_tree(Grow::Maximized, 99, &split),
            CenterTree::Stack {
                panes: vec![1, 2, 3],
                active: 0
            }
        );
        // Tiled → the tab's real tree.
        assert_eq!(grown_tree(Grow::Tiled, 2, &split), split);
    }

    #[test]
    fn grown_hit_geometry_is_the_drawn_geometry() {
        // The mouse regression: while grown, the focused pane's framed rect is
        // the whole center minus the bars — NOT its tiled split rect.
        let split = three_panes();
        let tiled = split.layout_framed(center_rect());
        let grown = grown_tree(Grow::Maximized, 1, &split).layout_framed(center_rect());
        assert_eq!(grown.len(), 1, "hidden panes are not hit targets");
        let (id, frame, _) = grown[0];
        assert_eq!(id, 1);
        assert_eq!(frame.cols, 100, "expanded across the whole center");
        assert_eq!(frame.rows, 40 - 2, "two collapsed bars below it");
        let tiled_1 = tiled.iter().find(|(p, _, _)| *p == 1).unwrap().1;
        assert_ne!(frame, tiled_1);
    }

    #[test]
    fn nav_layout_walks_the_stack_vertically_only_while_grown() {
        use crate::center::{Move, neighbor};
        let mut tab = crate::session::Tab::new("1");
        tab.center = three_panes();
        tab.focused_pane = 1;
        // Tiled: ↓ from the full-height left pane follows the real geometry
        // (the centre-based walk lands on the lower-right pane, 3).
        let l = nav_layout(Some(&tab), center_rect(), true);
        assert_eq!(neighbor(&l, 1, Move::Down), Some(3));
        // Grown: ↓ steps to the next stack member, ↑ off the top is the edge.
        tab.grow = Grow::Maximized;
        let l = nav_layout(Some(&tab), center_rect(), true);
        assert_eq!(neighbor(&l, 1, Move::Down), Some(2));
        assert_eq!(neighbor(&l, 2, Move::Down), Some(3));
        assert_eq!(neighbor(&l, 3, Move::Up), Some(2));
        assert_eq!(neighbor(&l, 1, Move::Up), None);
        assert_eq!(neighbor(&l, 3, Move::Down), None);
        // Horizontal moves keep the real split geometry.
        let l = nav_layout(Some(&tab), center_rect(), false);
        assert_eq!(neighbor(&l, 1, Move::Right), Some(2));
        assert!(nav_layout(None, center_rect(), true).is_empty());
    }

    #[test]
    fn activating_a_stack_bar_focuses_that_pane() {
        let mut session = crate::session::Session::default();
        let mut g =
            crate::session::WorktreeGroup::new("main", crate::session::GroupKind::Home, "/tmp/x");
        let tab = &mut g.tabs[0];
        tab.center = three_panes();
        tab.focused_pane = 1;
        tab.grow = Grow::Fullscreen;
        session.worktrees.push(g);
        let mut f = focus(Zone::Sidebar);
        activate_stack_member(&mut session, &mut f, 3);
        assert_eq!(f.zone, Zone::Center);
        let t = session.active_tab().unwrap();
        assert_eq!(t.focused_pane, 3);
        assert_eq!(t.grow, Grow::Fullscreen, "the zoom level is kept");
        assert_eq!(displayed_tree(&session).layout(center_rect())[0].0, 3);
    }

    #[test]
    fn keyboard_stack_focus_remains_visible_when_restoring_tiled() {
        use crate::center::Move;
        use crate::focus::{FocusMove, NavMove, RouteCtx, resolve_nav, route};
        for initial in [Grow::Maximized, Grow::Fullscreen] {
            for alt_navigation in [false, true] {
                for arrangement in 0..3 {
                    let mut group = crate::session::WorktreeGroup::terminal("stack");
                    let original = CenterTree::Stack {
                        panes: vec![1, 2],
                        active: 0,
                    };
                    group.tabs[0].center = match arrangement {
                        0 => original,
                        1 => CenterTree::Split {
                            dir: Dir::Row,
                            children: vec![
                                Branch {
                                    weight: 1.0,
                                    child: original,
                                },
                                Branch {
                                    weight: 1.0,
                                    child: CenterTree::Leaf(3),
                                },
                            ],
                        },
                        _ => three_panes(),
                    };
                    group.tabs[0].focused_pane = 1;
                    group.tabs[0].grow = initial;
                    let mut session = crate::session::Session {
                        worktrees: vec![group],
                        ..Default::default()
                    };
                    let mut f = focus(Zone::Center);
                    let geometry = nav_layout(session.active_tab(), center_rect(), true);
                    // Alt first resolves to the directional focus action; Ctrl
                    // enters that action directly. Both then share route and
                    // the same mutation seam used by run.rs.
                    if alt_navigation {
                        assert_eq!(
                            resolve_nav(true, Move::Down, &geometry, 1),
                            NavMove::Focus(Move::Down),
                        );
                    }
                    let resolved = route(
                        f.zone,
                        Move::Down,
                        &RouteCtx {
                            sidebar_visible: true,
                            panel_visible: true,
                            drawer_visible: false,
                            layout: &geometry,
                            focused_pane: 1,
                        },
                    );
                    assert_eq!(resolved, FocusMove::CenterPane(2));
                    let FocusMove::CenterPane(id) = resolved else {
                        unreachable!()
                    };
                    activate_stack_member(&mut session, &mut f, id);
                    assert_eq!(displayed_tree(&session).layout(center_rect())[0].0, 2);
                    let mut zone_zoom = None;
                    while active_grow(&session) != Grow::Tiled {
                        cycle_or_zoom(
                            &mut zone_zoom,
                            &mut session.active_tab_mut().unwrap().grow,
                            &f,
                        );
                    }
                    let tab = session.active_tab().unwrap();
                    assert_eq!(tab.focused_pane, 2);
                    assert!(
                        tab.center
                            .layout(center_rect())
                            .iter()
                            .any(|(id, _)| *id == tab.focused_pane)
                    );
                    if arrangement != 2 {
                        assert!(
                            !tab.center
                                .layout(center_rect())
                                .iter()
                                .any(|(id, _)| *id == 1)
                        );
                    }
                    assert_eq!(f.zone, Zone::Center);
                }
            }
        }
    }
}
