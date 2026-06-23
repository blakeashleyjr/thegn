//! The focus model: one zone owns the keyboard at any time, and Ctrl+direction
//! moves focus across one spatial graph — sidebar ← center panes → panel, with
//! the masthead above and the statusbar below, and up/down moving within
//! whatever currently has focus (sidebar rows, tiled panes, panel sections).
//! Replaces the old disconnected booleans (`sb.focused` / `model.panel_focused`).
//!
//! `route` is pure (zone × direction × visibility × pane geometry → a move) so
//! the whole matrix is unit-testable without a terminal.

use crate::center::{Move, PaneId, neighbor};
use crate::compositor::Rect;

/// What can own keyboard focus (the command palette sits above this and
/// captures keys before the zone is consulted).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Zone {
    Sidebar,
    #[default]
    Center,
    Panel,
    /// The bottom file-manager drawer (yazi). Sits below the center, above the
    /// statusbar; keys forward to its PTY only while it owns focus.
    Drawer,
    /// The top bar (brand + stats cluster).
    Masthead,
    /// The bottom bar (hints + status widgets).
    Statusbar,
}

/// The session's focus state: the zone, plus the Ctrl+g keybind lock. While
/// `locked`, every key except Ctrl+g passes through to the focused pane.
#[derive(Debug, Clone, Copy, Default)]
pub struct FocusState {
    pub zone: Zone,
    pub locked: bool,
}

impl FocusState {
    pub fn sidebar(&self) -> bool {
        self.zone == Zone::Sidebar
    }
    pub fn panel(&self) -> bool {
        self.zone == Zone::Panel
    }
    pub fn drawer(&self) -> bool {
        self.zone == Zone::Drawer
    }
    pub fn center(&self) -> bool {
        self.zone == Zone::Center
    }
    pub fn masthead(&self) -> bool {
        self.zone == Zone::Masthead
    }
    pub fn statusbar(&self) -> bool {
        self.zone == Zone::Statusbar
    }
    /// A chrome bar owns the keyboard (Esc returns to the center).
    pub fn bar(&self) -> bool {
        matches!(self.zone, Zone::Masthead | Zone::Statusbar)
    }
}

/// What a directional focus move resolves to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusMove {
    /// Stay in the center; focus this pane.
    CenterPane(PaneId),
    /// Cross a boundary into a zone.
    Enter(Zone),
    /// Move the cursor within the current zone: -1 = up, +1 = down.
    WithinZone(i8),
    /// Edge of the graph; nothing happens.
    None,
}

/// Everything the router needs to resolve a move.
pub struct RouteCtx<'a> {
    pub sidebar_visible: bool,
    pub panel_visible: bool,
    pub drawer_visible: bool,
    /// The active tab's computed pane layout (content rects are fine — only
    /// relative geometry matters).
    pub layout: &'a [(PaneId, Rect)],
    pub focused_pane: PaneId,
}

/// Resolve Ctrl+direction from `zone`. The screen is the map: leaving the
/// leftmost pane lands in the sidebar, the rightmost in the panel, the top
/// edge in the masthead, the bottom edge in the statusbar; hidden chrome is
/// skipped (a hidden sidebar is not a focus target — the bars are always up).
pub fn route(zone: Zone, dir: Move, ctx: &RouteCtx) -> FocusMove {
    match zone {
        Zone::Center => {
            if let Some(n) = neighbor(ctx.layout, ctx.focused_pane, dir) {
                return FocusMove::CenterPane(n);
            }
            match dir {
                Move::Left if ctx.sidebar_visible => FocusMove::Enter(Zone::Sidebar),
                Move::Right if ctx.panel_visible => FocusMove::Enter(Zone::Panel),
                Move::Up => FocusMove::Enter(Zone::Masthead),
                // The open drawer sits between the center and the statusbar.
                Move::Down if ctx.drawer_visible => FocusMove::Enter(Zone::Drawer),
                Move::Down => FocusMove::Enter(Zone::Statusbar),
                _ => FocusMove::None,
            }
        }
        Zone::Sidebar => match dir {
            Move::Right => FocusMove::Enter(Zone::Center),
            Move::Up => FocusMove::WithinZone(-1),
            Move::Down => FocusMove::WithinZone(1),
            Move::Left => FocusMove::None,
        },
        Zone::Panel => match dir {
            Move::Left => FocusMove::Enter(Zone::Center),
            Move::Up => FocusMove::WithinZone(-1),
            Move::Down => FocusMove::WithinZone(1),
            Move::Right => FocusMove::None,
        },
        // The drawer sits below the center, above the statusbar: up returns to
        // the center, down drops to the statusbar (left/right go to yazi while
        // focused, so they dead-end the focus graph here).
        Zone::Drawer => match dir {
            Move::Up => FocusMove::Enter(Zone::Center),
            Move::Down => FocusMove::Enter(Zone::Statusbar),
            _ => FocusMove::None,
        },
        // The bars sit above/below everything: down/up returns to the center
        // (left/right are reserved for widget selection within the bar).
        Zone::Masthead => match dir {
            Move::Down => FocusMove::Enter(Zone::Center),
            _ => FocusMove::None,
        },
        Zone::Statusbar => match dir {
            // Step back up through the drawer when it's open.
            Move::Up if ctx.drawer_visible => FocusMove::Enter(Zone::Drawer),
            Move::Up => FocusMove::Enter(Zone::Center),
            _ => FocusMove::None,
        },
    }
}

/// May an unmatched key fall through to the focused PTY? Only when a PTY zone
/// owns the keyboard: the center panes, or the bottom drawer (yazi) while it is
/// focused. The Ctrl+g keybind lock short-circuits before this.
pub fn forwards_to_pane(zone: Zone) -> bool {
    matches!(zone, Zone::Center | Zone::Drawer)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: usize, cols: usize) -> Rect {
        Rect {
            x,
            y: 0,
            cols,
            rows: 40,
        }
    }

    /// Two panes side by side: 1 (left) | 2 (right).
    fn two_panes() -> Vec<(PaneId, Rect)> {
        vec![(1, rect(0, 50)), (2, rect(50, 50))]
    }

    fn ctx(layout: &[(PaneId, Rect)], focused: PaneId, sb: bool, pn: bool) -> RouteCtx<'_> {
        RouteCtx {
            sidebar_visible: sb,
            panel_visible: pn,
            drawer_visible: false,
            layout,
            focused_pane: focused,
        }
    }

    fn ctx_drawer(layout: &[(PaneId, Rect)], focused: PaneId) -> RouteCtx<'_> {
        RouteCtx {
            sidebar_visible: true,
            panel_visible: true,
            drawer_visible: true,
            layout,
            focused_pane: focused,
        }
    }

    #[test]
    fn center_moves_between_panes_first() {
        let l = two_panes();
        assert_eq!(
            route(Zone::Center, Move::Right, &ctx(&l, 1, true, true)),
            FocusMove::CenterPane(2)
        );
        assert_eq!(
            route(Zone::Center, Move::Left, &ctx(&l, 2, true, true)),
            FocusMove::CenterPane(1)
        );
    }

    #[test]
    fn center_edges_cross_into_visible_chrome() {
        let l = two_panes();
        assert_eq!(
            route(Zone::Center, Move::Left, &ctx(&l, 1, true, true)),
            FocusMove::Enter(Zone::Sidebar)
        );
        assert_eq!(
            route(Zone::Center, Move::Right, &ctx(&l, 2, true, true)),
            FocusMove::Enter(Zone::Panel)
        );
        // Hidden chrome is not a focus target.
        assert_eq!(
            route(Zone::Center, Move::Left, &ctx(&l, 1, false, true)),
            FocusMove::None
        );
        assert_eq!(
            route(Zone::Center, Move::Right, &ctx(&l, 2, true, false)),
            FocusMove::None
        );
    }

    #[test]
    fn center_vertical_edges_reach_the_bars() {
        let l = two_panes();
        assert_eq!(
            route(Zone::Center, Move::Up, &ctx(&l, 1, true, true)),
            FocusMove::Enter(Zone::Masthead)
        );
        assert_eq!(
            route(Zone::Center, Move::Down, &ctx(&l, 1, true, true)),
            FocusMove::Enter(Zone::Statusbar)
        );
        // Stacked panes still move between panes first.
        let stacked = vec![
            (
                1,
                Rect {
                    x: 0,
                    y: 0,
                    cols: 100,
                    rows: 20,
                },
            ),
            (
                2,
                Rect {
                    x: 0,
                    y: 20,
                    cols: 100,
                    rows: 20,
                },
            ),
        ];
        assert_eq!(
            route(Zone::Center, Move::Down, &ctx(&stacked, 1, true, true)),
            FocusMove::CenterPane(2)
        );
        assert_eq!(
            route(Zone::Center, Move::Down, &ctx(&stacked, 2, true, true)),
            FocusMove::Enter(Zone::Statusbar)
        );
        assert_eq!(
            route(Zone::Center, Move::Up, &ctx(&stacked, 1, true, true)),
            FocusMove::Enter(Zone::Masthead)
        );
    }

    #[test]
    fn bars_return_to_center_and_dead_end_outward() {
        let l = two_panes();
        let c = ctx(&l, 1, true, true);
        assert_eq!(
            route(Zone::Masthead, Move::Down, &c),
            FocusMove::Enter(Zone::Center)
        );
        assert_eq!(route(Zone::Masthead, Move::Up, &c), FocusMove::None);
        assert_eq!(route(Zone::Masthead, Move::Left, &c), FocusMove::None);
        assert_eq!(
            route(Zone::Statusbar, Move::Up, &c),
            FocusMove::Enter(Zone::Center)
        );
        assert_eq!(route(Zone::Statusbar, Move::Down, &c), FocusMove::None);
    }

    #[test]
    fn sidebar_walks_rows_and_exits_right() {
        let l = two_panes();
        let c = ctx(&l, 1, true, true);
        assert_eq!(
            route(Zone::Sidebar, Move::Up, &c),
            FocusMove::WithinZone(-1)
        );
        assert_eq!(
            route(Zone::Sidebar, Move::Down, &c),
            FocusMove::WithinZone(1)
        );
        assert_eq!(
            route(Zone::Sidebar, Move::Right, &c),
            FocusMove::Enter(Zone::Center)
        );
        assert_eq!(route(Zone::Sidebar, Move::Left, &c), FocusMove::None);
    }

    #[test]
    fn panel_walks_widgets_and_exits_left() {
        let l = two_panes();
        let c = ctx(&l, 1, true, true);
        assert_eq!(route(Zone::Panel, Move::Up, &c), FocusMove::WithinZone(-1));
        assert_eq!(route(Zone::Panel, Move::Down, &c), FocusMove::WithinZone(1));
        assert_eq!(
            route(Zone::Panel, Move::Left, &c),
            FocusMove::Enter(Zone::Center)
        );
        assert_eq!(route(Zone::Panel, Move::Right, &c), FocusMove::None);
    }

    #[test]
    fn focus_state_helpers() {
        let mut f = FocusState::default();
        assert!(f.center() && !f.sidebar() && !f.panel() && !f.bar());
        f.zone = Zone::Sidebar;
        assert!(f.sidebar());
        f.zone = Zone::Panel;
        assert!(f.panel());
        f.zone = Zone::Masthead;
        assert!(f.masthead() && f.bar());
        f.zone = Zone::Statusbar;
        assert!(f.statusbar() && f.bar());
    }

    #[test]
    fn forwards_to_pane_matrix() {
        // Only the PTY zones forward keys: the center and the focused drawer.
        assert!(forwards_to_pane(Zone::Center));
        assert!(forwards_to_pane(Zone::Drawer));
        assert!(!forwards_to_pane(Zone::Sidebar));
        assert!(!forwards_to_pane(Zone::Panel));
        assert!(!forwards_to_pane(Zone::Masthead));
        assert!(!forwards_to_pane(Zone::Statusbar));
    }

    #[test]
    fn drawer_sits_between_center_and_statusbar() {
        let l = two_panes();
        // Down from the bottom-edge center pane enters the open drawer instead
        // of the statusbar; up leaves it back to the center.
        assert_eq!(
            route(Zone::Center, Move::Down, &ctx_drawer(&l, 1)),
            FocusMove::Enter(Zone::Drawer)
        );
        assert_eq!(
            route(Zone::Drawer, Move::Up, &ctx_drawer(&l, 1)),
            FocusMove::Enter(Zone::Center)
        );
        assert_eq!(
            route(Zone::Drawer, Move::Down, &ctx_drawer(&l, 1)),
            FocusMove::Enter(Zone::Statusbar)
        );
        // Left/right dead-end (those keys drive yazi while focused).
        assert_eq!(route(Zone::Drawer, Move::Left, &ctx_drawer(&l, 1)), FocusMove::None);
        assert_eq!(route(Zone::Drawer, Move::Right, &ctx_drawer(&l, 1)), FocusMove::None);
        // Statusbar steps back up through the drawer when it's open.
        assert_eq!(
            route(Zone::Statusbar, Move::Up, &ctx_drawer(&l, 1)),
            FocusMove::Enter(Zone::Drawer)
        );
        // With the drawer closed, down/up skip it (center ↔ statusbar directly).
        assert_eq!(
            route(Zone::Center, Move::Down, &ctx(&l, 1, true, true)),
            FocusMove::Enter(Zone::Statusbar)
        );
        assert_eq!(
            route(Zone::Statusbar, Move::Up, &ctx(&l, 1, true, true)),
            FocusMove::Enter(Zone::Center)
        );
    }
}
