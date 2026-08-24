//! Mouse-side handling for the compositor's overlays, extracted from `run.rs`
//! (kept flat). Each function runs ON the event loop and
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
    monitor: &mut Option<crate::monitor::MonitorOverlay>,
    help: &mut Option<crate::help::HelpOverlay>,
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
    // Set when a click inside the detail popup produced an action the loop must
    // execute (the popup itself can't — it lacks the loop's borrows).
    detail_act: &mut Option<crate::detail::DetailAction>,
    mouse_left_down: &mut bool,
    mouse_selecting: &mut bool,
    mouse_sel: &mut Option<(u32, crate::copymode::Selection)>,
    dirty: &mut bool,
    // Pointer capture: a live sidebar drag owns the mouse wherever it wanders.
    sidebar_drag_active: bool,
) -> MousePre {
    // 0. The help overlay is modal to the mouse like a detail popup: wheel
    // scrolls the page, an outside left-press dismisses, and an inside press
    // navigates (TOC row, search hit, or a link on the clicked line).
    if let Some(h) = help.as_mut() {
        if let Some(boxr) = h.box_rect(Rect {
            x: 0,
            y: 0,
            cols,
            rows,
        }) {
            if m.mouse_buttons.contains(MouseButtons::VERT_WHEEL) {
                let delta = if m.mouse_buttons.contains(MouseButtons::WHEEL_POSITIVE) {
                    -3
                } else {
                    3
                };
                h.scroll_by(delta);
                *dirty = true;
            } else if left && !*mouse_left_down {
                // `OpenInPanel` is keyboard-only (`o`), so a click can only ask
                // to close or to stay open.
                if !contains(boxr, mx, my)
                    || h.handle_click(mx, my) == crate::help::HelpOutcome::Close
                {
                    *help = None;
                }
                *dirty = true;
            }
        }
        *mouse_left_down = left;
        *mouse_selecting = false;
        *mouse_sel = None;
        return MousePre::Consumed;
    }
    // 0b. The system monitor is modal to the mouse the same way the help
    // overlay is: the wheel scrolls it, an outside left-press dismisses it, and
    // nothing reaches the panes behind the dim.
    if let Some(mon) = monitor.as_mut() {
        if let Some(boxr) = mon.box_rect(Rect {
            x: 0,
            y: 0,
            cols,
            rows,
        }) {
            if m.mouse_buttons.contains(MouseButtons::VERT_WHEEL) {
                let delta = if m.mouse_buttons.contains(MouseButtons::WHEEL_POSITIVE) {
                    -3
                } else {
                    3
                };
                mon.wheel(delta);
                *dirty = true;
            } else if left && !*mouse_left_down && !contains(boxr, mx, my) {
                *monitor = None;
                *dirty = true;
            }
        }
        *mouse_left_down = left;
        *mouse_selecting = false;
        *mouse_sel = None;
        return MousePre::Consumed;
    }
    // 1. A detail popup owns the mouse INSIDE its own box, always — the
    // calendar's day cells and month chevrons are clickable, and a wheel notch
    // pages it. OUTSIDE the box, behaviour is unchanged: only when
    // `dismiss_overlay_on_click_outside` is set does a press dismiss and
    // swallow; with it off the event falls through to the panes as before.
    let screen = Rect {
        x: 0,
        y: 0,
        cols,
        rows,
    };
    if let Some(boxr) = bar_detail.as_ref().and_then(|d| d.box_rect(screen)) {
        if contains(boxr, mx, my) {
            if let Some(d) = bar_detail.as_mut() {
                let outcome = if m.mouse_buttons.contains(MouseButtons::VERT_WHEEL) {
                    let delta = if m.mouse_buttons.contains(MouseButtons::WHEEL_POSITIVE) {
                        -1
                    } else {
                        1
                    };
                    d.handle_wheel(delta)
                } else if left && !*mouse_left_down {
                    d.handle_click(mx, my, screen)
                } else {
                    crate::detail::DetailOutcome::Pending
                };
                match outcome {
                    crate::detail::DetailOutcome::Close => {
                        *bar_detail = None;
                        *dirty = true;
                    }
                    // A click/wheel that navigated in place still needs a
                    // repaint; `detail_act` carries anything the loop must run.
                    crate::detail::DetailOutcome::Pending => *dirty = true,
                    crate::detail::DetailOutcome::Act(a) => {
                        *dirty = true;
                        *detail_act = Some(a);
                    }
                }
            }
            *mouse_left_down = left;
            *mouse_selecting = false;
            *mouse_sel = None;
            return MousePre::Consumed;
        }
        if dismiss_on_click_outside {
            if left && !*mouse_left_down {
                *bar_detail = None;
                *dirty = true;
            }
            // Reset drag state since we skip the caller's branch-tail
            // bookkeeping.
            *mouse_left_down = left;
            *mouse_selecting = false;
            *mouse_sel = None;
            return MousePre::Consumed;
        }
        // Flag off: fall through to the pane/chrome dispatch, exactly as before.
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
        sidebar_drag_active,
    ) {
        return MousePre::Consumed;
    }
    MousePre::Fall(hit_pane, frames)
}

/// Whether a mouse event over a pane belongs to the pane's app.
///
/// `sidebar_drag_active` is **pointer capture**: while a sidebar drag is armed
/// or in flight the gesture owns the pointer wherever it wanders. Without it, a
/// drag whose pointer crossed a mouse-reporting pane had its RELEASE written
/// into that pane and consumed — so `on_release` never ran, the phase stayed
/// `Dragging`, and the next left-drag anywhere in the app was hijacked back
/// into the sidebar handler.
///
/// Shift bypassing the app is the convention every terminal uses, and it still
/// applies whenever no drag is in flight.
pub(crate) fn should_forward_to_pane(
    pane_reports_mouse: bool,
    shift: bool,
    sidebar_drag_active: bool,
) -> bool {
    pane_reports_mouse && !shift && !sidebar_drag_active
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
    sidebar_drag_active: bool,
) -> bool {
    let Some((id, content)) = hit_pane else {
        return false;
    };
    let Some((mode, sgr)) = panes.table.get(&id).map(|p| p.emulator().mouse_mode()) else {
        return false;
    };
    if !should_forward_to_pane(
        mode != crate::emulator::MouseMode::None,
        m.modifiers.contains(Modifiers::SHIFT),
        sidebar_drag_active,
    ) {
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
