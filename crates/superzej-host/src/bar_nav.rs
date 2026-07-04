//! Selection navigation for the chrome bars (masthead + statusbar).
//!
//! When a bar owns the keyboard (Ctrl+↑/↓ crossed into it), the item highlight
//! steps left/right. The Ctrl chord does this via the focus router, but plain
//! ←/→ (and h/l) should step too — otherwise the highlight is stuck on the
//! first item (LOC) and the alert badges to its right are unreachable.

use termwiz::input::KeyCode;

use crate::chrome::{self, FrameModel};
use crate::focus::FocusState;
use crate::layout::ChromeLayout;

/// Step a bar's selected-item index by `delta` (-1/+1), clamped to `[0, count)`.
/// An empty bar stays at 0.
pub fn step_bar_sel(sel: usize, delta: i8, count: usize) -> usize {
    if count == 0 {
        return 0;
    }
    if delta < 0 {
        sel.saturating_sub(1)
    } else {
        (sel + 1).min(count - 1)
    }
}

/// While a chrome bar (masthead/statusbar) owns the keyboard, plain ←/→ (and
/// h/l) step the selected item over the same item list the router uses — so the
/// alert badges past LOC are reachable without the Ctrl chord. Returns whether
/// the key was consumed.
pub fn step_focused_bar(
    focus: &FocusState,
    model: &mut FrameModel,
    chrome: &ChromeLayout,
    key: &KeyCode,
) -> bool {
    let delta: i8 = match key {
        KeyCode::LeftArrow | KeyCode::Char('h') => -1,
        KeyCode::RightArrow | KeyCode::Char('l') => 1,
        _ => return false,
    };
    if focus.statusbar() {
        let count = chrome::statusbar_items(model).len();
        model.statusbar_sel = step_bar_sel(model.statusbar_sel, delta, count);
        true
    } else if focus.masthead() {
        let count = chrome::masthead_item_spans(model, chrome).len();
        model.masthead_sel = step_bar_sel(model.masthead_sel, delta, count);
        true
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_clamps_at_both_ends() {
        // Right steps up to count-1 then clamps.
        assert_eq!(step_bar_sel(0, 1, 3), 1);
        assert_eq!(step_bar_sel(2, 1, 3), 2);
        // Left steps down to 0 then clamps.
        assert_eq!(step_bar_sel(1, -1, 3), 0);
        assert_eq!(step_bar_sel(0, -1, 3), 0);
        // An empty bar stays at 0.
        assert_eq!(step_bar_sel(0, 1, 0), 0);
    }

    #[test]
    fn only_arrow_and_hl_keys_are_consumed() {
        let focus = FocusState {
            zone: crate::focus::Zone::Statusbar,
            locked: false,
        };
        let mut model = FrameModel::default();
        let chrome = crate::layout::compute(160, 10, false, false);
        // A non-nav key is ignored.
        assert!(!step_focused_bar(
            &focus,
            &mut model,
            &chrome,
            &KeyCode::Char('x')
        ));
        // Arrow/hl keys are consumed when a bar is focused.
        assert!(step_focused_bar(
            &focus,
            &mut model,
            &chrome,
            &KeyCode::RightArrow
        ));
        // Not consumed when no bar owns focus.
        let center = FocusState::default();
        assert!(!step_focused_bar(
            &center,
            &mut model,
            &chrome,
            &KeyCode::RightArrow
        ));
    }
}
