//! Mouse-side handling for the compositor's overlays, extracted from `run.rs`
//! (pinned by the file-size ratchet). Each function runs ON the event loop and
//! must stay I/O-free apart from the PTY write it forwards.

use termwiz::input::{Modifiers, MouseButtons, MouseEvent};

use crate::compositor::Rect;

/// Whether cell `(x, y)` falls inside rect `r`.
fn contains(r: Rect, x: usize, y: usize) -> bool {
    x >= r.x && x < r.x + r.cols && y >= r.y && y < r.y + r.rows
}

/// Outcome of the mouse arm's front-matter ([`pre_dispatch`]).
pub(crate) enum MousePre {
    /// The event was fully handled — the caller should `continue`.
    Consumed,
    /// Nothing consumed it; carry the resolved `hit_pane` and the pane frame
    /// layout into the caller's remaining wheel/press/drag dispatch.
    Fall(Option<(u32, Rect)>, Vec<(u32, Rect, Rect)>),
}

/// Front-matter of the compositor's mouse handling, extracted from `run.rs`:
///  1. a summoned detail popup is modal to the mouse — an outside left-press
///     dismisses it (like Esc) and every mouse event is swallowed so nothing
///     reaches the panes/chrome behind the dim;
///  2. resolve the pane (or bottom drawer) under the cursor;
///  3. forward the event into a mouse-reporting pane app (htop/lazygit).
///
/// Returns [`MousePre::Consumed`] when the caller should `continue`, else
/// [`MousePre::Fall`] carrying `hit_pane` for the wheel/press dispatch.
#[allow(clippy::too_many_arguments)]
pub(crate) fn pre_dispatch(
    dismiss_on_click_outside: bool,
    bar_detail: &mut Option<crate::detail::DetailOverlay>,
    m: &MouseEvent,
    mx: usize,
    my: usize,
    left: bool,
    cols: usize,
    rows: usize,
    chrome: &crate::layout::ChromeLayout,
    app_host: &mut crate::apps::AppHost,
    drawer: Option<u32>,
    panes: &mut crate::panes::Panes,
    focus: &mut crate::focus::FocusState,
    session: &mut crate::session::Session,
    mouse_left_down: &mut bool,
    mouse_selecting: &mut bool,
    mouse_sel: &mut Option<(u32, crate::copymode::Selection)>,
    dirty: &mut bool,
) -> MousePre {
    // 1. A detail popup is modal to the mouse: outside left-press dismisses it
    // (like Esc); all mouse events are swallowed while it is up.
    if dismiss_on_click_outside
        && let Some(boxr) = bar_detail.as_ref().and_then(|d| {
            d.box_rect(Rect {
                x: 0,
                y: 0,
                cols,
                rows,
            })
        })
    {
        if left && !*mouse_left_down && !contains(boxr, mx, my) {
            *bar_detail = None;
            *dirty = true;
        }
        // Inside clicks are no-ops (detail popups are keyboard-driven); reset
        // drag state since we skip the caller's branch-tail bookkeeping.
        *mouse_left_down = left;
        *mouse_selecting = false;
        *mouse_sel = None;
        return MousePre::Consumed;
    }
    // 2. Resolve the pane (or bottom drawer) under the cursor.
    let frames = session
        .active_tab()
        .map(|t| t.center.layout_framed(chrome.center))
        .unwrap_or_default();
    let hit_pane = if app_host.active_tile_mut().is_none()
        && let Some(drawer_id) = drawer
        && let Some(rect) = chrome.drawer
        && contains(rect, mx, my)
    {
        Some((drawer_id, rect))
    } else {
        frames
            .iter()
            .find(|(_, _, c)| contains(*c, mx, my))
            .map(|(id, _, c)| (*id, *c))
    };
    // 3. Forward into a mouse-reporting pane app; consumes when one is hit.
    if forward_pane_mouse(
        hit_pane,
        m,
        mx,
        my,
        left,
        panes,
        focus,
        session,
        mouse_left_down,
        mouse_selecting,
        mouse_sel,
        dirty,
    ) {
        return MousePre::Consumed;
    }
    MousePre::Fall(hit_pane, frames)
}

/// Full terminal support: when the app inside the hit pane asked for mouse
/// reporting (htop, lazygit, …), forward the event into the pane instead of
/// handling it ourselves. Holding Shift bypasses the app and forces host
/// selection — the convention every terminal uses. Returns `true` when the
/// event was consumed (the caller should `continue`).
#[allow(clippy::too_many_arguments)]
pub(crate) fn forward_pane_mouse(
    hit_pane: Option<(u32, Rect)>,
    m: &MouseEvent,
    mx: usize,
    my: usize,
    left: bool,
    panes: &mut crate::panes::Panes,
    focus: &mut crate::focus::FocusState,
    session: &mut crate::session::Session,
    mouse_left_down: &mut bool,
    mouse_selecting: &mut bool,
    mouse_sel: &mut Option<(u32, crate::copymode::Selection)>,
    dirty: &mut bool,
) -> bool {
    let Some((id, content)) = hit_pane else {
        return false;
    };
    if m.modifiers.contains(Modifiers::SHIFT) {
        return false;
    }
    let Some((mode, sgr)) = panes.table.get(&id).map(|p| p.emulator().mouse_mode()) else {
        return false;
    };
    if mode == crate::emulator::MouseMode::None {
        return false;
    }
    use crate::input::{PaneMouse, encode_mouse};
    let col = (mx - content.x) as u16;
    let row = (my - content.y) as u16;
    let ev = if m.mouse_buttons.contains(MouseButtons::VERT_WHEEL) {
        if m.mouse_buttons.contains(MouseButtons::WHEEL_POSITIVE) {
            Some(PaneMouse::WheelUp)
        } else {
            Some(PaneMouse::WheelDown)
        }
    } else if left && !*mouse_left_down {
        // A press also focuses the pane.
        focus.zone = crate::focus::Zone::Center;
        if let Some(tab) = session.active_tab_mut() {
            tab.focused_pane = id;
        }
        *mouse_sel = None;
        *dirty = true;
        Some(PaneMouse::Press(0))
    } else if left && *mouse_left_down {
        Some(PaneMouse::Drag(0))
    } else if !left && *mouse_left_down {
        Some(PaneMouse::Release(0))
    } else {
        Some(PaneMouse::Move)
    };
    if let Some(ev) = ev
        && let Some(bytes) = encode_mouse(ev, mode, sgr, col, row)
        && let Some(p) = panes.table.get_mut(&id)
    {
        let _ = p.write_input(&bytes);
    }
    *mouse_left_down = left;
    *mouse_selecting = false;
    true
}
