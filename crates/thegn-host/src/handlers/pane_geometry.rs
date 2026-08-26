//! Keyboard pane-geometry ops — resize and swap — kept out of the run-loop
//! god-file. Both are thin routers over the pure [`crate::center`] tree
//! mutations: they resolve the focused pane's target from the session owner
//! (today the in-loop [`Session`]; via a `SessionHandle` once
//! `add-runtime-session-split` lands, they become `apply_layout` mutations with
//! no change here), apply the mutation, and reuse the debounced tab-layout
//! persist. The caller sets `need_relayout` and surfaces the returned hint.

use crate::center::{Move, PaneId, RESIZE_STEP, ResizeOutcome};
use crate::compositor::Rect;
use crate::panes::Panes;
use crate::session::Session;

/// Grow the focused pane one step toward `mv`. On [`ResizeOutcome::Resized`] the
/// new layout is persisted; the caller sets `need_relayout` and (on
/// [`ResizeOutcome::NoTarget`]) shows a statusbar hint.
pub(crate) fn resize(
    session: &mut Session,
    panes: &Panes,
    focused: PaneId,
    mv: Move,
) -> ResizeOutcome {
    let outcome = session
        .active_tab_mut()
        .map(|t| t.center.resize(focused, mv, RESIZE_STEP))
        .unwrap_or(ResizeOutcome::NoTarget);
    if outcome == ResizeOutcome::Resized {
        crate::run::persist_session_layout(session, panes);
    }
    outcome
}

/// Outcome of a keyboard [`swap`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SwapOutcome {
    /// The focused pane exchanged places with its neighbour.
    Swapped,
    /// No neighbour in that direction — a harmless no-op.
    NoNeighbour,
}

/// Exchange the focused pane with its spatial neighbour toward `mv`, resolving
/// the neighbour with the *same* geometry walk `Focus*` uses (over `center_rect`
/// — the live center rect) so focus and swap always agree. Focus follows the
/// moved pane (its id is unchanged; it simply occupies the neighbour's old
/// slot). Persists on success.
pub(crate) fn swap(
    session: &mut Session,
    panes: &Panes,
    focused: PaneId,
    center_rect: Rect,
    mv: Move,
) -> SwapOutcome {
    let layout = session
        .active_tab()
        .map(|t| t.center.layout(center_rect))
        .unwrap_or_default();
    let Some(neighbour) = crate::center::neighbor(&layout, focused, mv) else {
        return SwapOutcome::NoNeighbour;
    };
    let swapped = session
        .active_tab_mut()
        .map(|t| t.center.swap(focused, neighbour))
        .unwrap_or(false);
    if swapped {
        crate::run::persist_session_layout(session, panes);
        SwapOutcome::Swapped
    } else {
        SwapOutcome::NoNeighbour
    }
}

/// Map a resize/swap `Action` direction to a [`Move`]. `None` for non-geometry
/// actions.
pub(crate) fn resize_move(action: &crate::keymap::Action) -> Option<Move> {
    use crate::keymap::Action;
    Some(match action {
        Action::ResizeLeft => Move::Left,
        Action::ResizeRight => Move::Right,
        Action::ResizeUp => Move::Up,
        Action::ResizeDown => Move::Down,
        _ => return None,
    })
}

/// Map a swap `Action` direction to a [`Move`]. `None` for non-swap actions.
pub(crate) fn swap_move(action: &crate::keymap::Action) -> Option<Move> {
    use crate::keymap::Action;
    Some(match action {
        Action::SwapPaneLeft => Move::Left,
        Action::SwapPaneRight => Move::Right,
        Action::SwapPaneUp => Move::Up,
        Action::SwapPaneDown => Move::Down,
        _ => return None,
    })
}
