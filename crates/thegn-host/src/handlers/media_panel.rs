//! Shared keyboard/mouse intent mapping for the docked Media section.

use termwiz::input::KeyCode;

use crate::panel::MediaAction;

/// The section's transport keys and the painted control targets converge on
/// the same `MediaAction` before entering the off-loop media controller.
pub(crate) fn action_for_key(key: &KeyCode) -> Option<MediaAction> {
    match key {
        KeyCode::Char(' ') => Some(MediaAction::PlayPause),
        KeyCode::Char('n') => Some(MediaAction::Next),
        KeyCode::Char('p') => Some(MediaAction::Previous),
        KeyCode::Char('s') => Some(MediaAction::Shuffle),
        KeyCode::Char('L') => Some(MediaAction::Loop),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyboard_transport_keys_have_panel_intents() {
        assert_eq!(
            action_for_key(&KeyCode::Char(' ')),
            Some(MediaAction::PlayPause)
        );
        assert_eq!(action_for_key(&KeyCode::Char('n')), Some(MediaAction::Next));
        assert_eq!(
            action_for_key(&KeyCode::Char('p')),
            Some(MediaAction::Previous)
        );
        assert_eq!(action_for_key(&KeyCode::Char('x')), None);
    }
}
