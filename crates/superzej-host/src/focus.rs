//! The focus model: one zone owns the keyboard at any time, and Ctrl+direction
//! moves focus across one spatial graph — sidebar ← center panes → panel, with
//! up/down moving within whatever currently has focus (sidebar rows, tiled
//! panes, panel widgets). Replaces the old disconnected booleans
//! (`sb.focused` / `model.panel_focused`).
//!
//! `route` is pure (zone × direction × visibility × pane geometry → a move) so
//! the whole matrix is unit-testable without a terminal.

use crate::center::{Move, PaneId, neighbor};
use crate::compositor::Rect;

/// What can own keyboard focus (modal overlays — palette, cheatsheet — sit
/// above this and capture keys before the zone is consulted).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Zone {
    Sidebar,
    #[default]
    Center,
    Panel,
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
    pub fn center(&self) -> bool {
        self.zone == Zone::Center
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
    /// The active tab's computed pane layout (content rects are fine — only
    /// relative geometry matters).
    pub layout: &'a [(PaneId, Rect)],
    pub focused_pane: PaneId,
}

/// Resolve Ctrl+direction from `zone`. The screen is the map: leaving the
/// leftmost pane lands in the sidebar, the rightmost in the panel; hidden
/// chrome is skipped (a hidden sidebar is not a focus target).
pub fn route(zone: Zone, dir: Move, ctx: &RouteCtx) -> FocusMove {
    match zone {
        Zone::Center => {
            if let Some(n) = neighbor(ctx.layout, ctx.focused_pane, dir) {
                return FocusMove::CenterPane(n);
            }
            match dir {
                Move::Left if ctx.sidebar_visible => FocusMove::Enter(Zone::Sidebar),
                Move::Right if ctx.panel_visible => FocusMove::Enter(Zone::Panel),
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
    }
}

/// May an unmatched key fall through to the focused PTY? Only when the
/// center owns the keyboard — or the drawer is open (it steals input from
/// any zone by design). The Ctrl+g keybind lock short-circuits before this.
pub fn forwards_to_pane(zone: Zone, drawer_open: bool) -> bool {
    drawer_open || zone == Zone::Center
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
        // Vertical edges are graph edges.
        assert_eq!(
            route(Zone::Center, Move::Up, &ctx(&l, 1, true, true)),
            FocusMove::None
        );
        assert_eq!(
            route(Zone::Center, Move::Down, &ctx(&l, 1, true, true)),
            FocusMove::None
        );
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
        assert!(f.center() && !f.sidebar() && !f.panel());
        f.zone = Zone::Sidebar;
        assert!(f.sidebar());
        f.zone = Zone::Panel;
        assert!(f.panel());
    }

    #[test]
    fn forwards_to_pane_matrix() {
        assert!(forwards_to_pane(Zone::Center, false));
        assert!(!forwards_to_pane(Zone::Sidebar, false));
        assert!(!forwards_to_pane(Zone::Panel, false));
        // An open drawer steals input from every zone.
        for z in [Zone::Center, Zone::Sidebar, Zone::Panel] {
            assert!(forwards_to_pane(z, true));
        }
    }
}
