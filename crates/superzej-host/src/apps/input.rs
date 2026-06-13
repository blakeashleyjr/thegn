//! Translate the host's termwiz key events into the backend-agnostic
//! [`sz_kit::InputEvent`] an [`AppTile`] consumes — the embedded mirror of the
//! standalone harness's crossterm translator (`sz_kit::input::from_crossterm`).
//!
//! [`AppTile`]: sz_kit::AppTile

use sz_kit::input::{InputEvent, Key, Modifiers};
use termwiz::input::{KeyCode, Modifiers as TwMods};

/// Map a termwiz key press to an [`InputEvent`]. `None` for keys a tile has no
/// concept of (the host keeps those for its own dispatch).
pub fn to_kit(key: &KeyCode, mods: TwMods) -> Option<InputEvent> {
    let modifiers = Modifiers {
        ctrl: mods.contains(TwMods::CTRL),
        alt: mods.contains(TwMods::ALT),
        shift: mods.contains(TwMods::SHIFT),
    };
    let k = match key {
        // CSI-u / fixterms terminals can report Esc as a control char.
        KeyCode::Char('\x1b') => Key::Escape,
        KeyCode::Char(c) => Key::Char(*c),
        KeyCode::Enter => Key::Enter,
        KeyCode::Escape => Key::Escape,
        KeyCode::Backspace => Key::Backspace,
        KeyCode::Delete => Key::Delete,
        // termwiz has no BackTab; Shift+Tab arrives as Tab + SHIFT.
        KeyCode::Tab if mods.contains(TwMods::SHIFT) => Key::BackTab,
        KeyCode::Tab => Key::Tab,
        KeyCode::LeftArrow => Key::Left,
        KeyCode::RightArrow => Key::Right,
        KeyCode::UpArrow => Key::Up,
        KeyCode::DownArrow => Key::Down,
        KeyCode::Home => Key::Home,
        KeyCode::End => Key::End,
        KeyCode::PageUp => Key::PageUp,
        KeyCode::PageDown => Key::PageDown,
        KeyCode::Function(n) => Key::Function(*n),
        _ => return None,
    };
    Some(InputEvent::Key { key: k, modifiers })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shift_tab_becomes_backtab() {
        assert_eq!(
            to_kit(&KeyCode::Tab, TwMods::SHIFT),
            Some(InputEvent::Key {
                key: Key::BackTab,
                modifiers: Modifiers {
                    ctrl: false,
                    alt: false,
                    shift: true
                },
            })
        );
        assert_eq!(
            to_kit(&KeyCode::Tab, TwMods::NONE),
            Some(InputEvent::key(Key::Tab))
        );
    }

    #[test]
    fn csi_u_escape_char_normalizes_to_escape() {
        assert_eq!(
            to_kit(&KeyCode::Char('\x1b'), TwMods::NONE),
            Some(InputEvent::key(Key::Escape))
        );
    }

    #[test]
    fn ctrl_char_carries_modifier() {
        assert_eq!(
            to_kit(&KeyCode::Char('c'), TwMods::CTRL),
            Some(InputEvent::Key {
                key: Key::Char('c'),
                modifiers: Modifiers {
                    ctrl: true,
                    alt: false,
                    shift: false
                },
            })
        );
    }

    #[test]
    fn unknown_keys_are_none() {
        // A key with no tile meaning (e.g. a raw modifier) is the host's.
        assert_eq!(to_kit(&KeyCode::Hyper, TwMods::NONE), None);
    }
}
