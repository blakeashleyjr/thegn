//! Shared keyboard/mouse intent mapping for the docked Media section.

use termwiz::input::KeyCode;

use crate::panel::MediaAction;

pub(crate) fn art_request(
    panel: &mut crate::panel::media::MediaPanelState,
    state: Option<&thegn_core::media::MediaState>,
    show_art: bool,
    visible: bool,
) -> Option<String> {
    if !visible {
        return None;
    }
    panel.wants_art(state, show_art)
}

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
    use thegn_core::media::MediaState;

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

    #[test]
    fn hidden_panel_requests_no_art_and_reopen_does() {
        let mut panel = crate::panel::media::MediaPanelState::default();
        panel.set_art_enabled(true);
        let state = MediaState {
            art_url: Some("https://example.invalid/cover.png".into()),
            ..MediaState::default()
        };

        assert!(art_request(&mut panel, Some(&state), true, false).is_none());
        assert!(art_request(&mut panel, Some(&state), true, true).is_some());
    }
}
