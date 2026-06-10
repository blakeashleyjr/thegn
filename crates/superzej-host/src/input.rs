//! Keyboard input encoding: translate termwiz key events into the byte
//! sequences a terminal application expects on its stdin.

use termwiz::input::{KeyCode, Modifiers};

/// Translate a termwiz key event into the bytes a terminal app expects on stdin.
pub(crate) fn key_bytes(key: &KeyCode, mods: Modifiers) -> Option<Vec<u8>> {
    match key {
        KeyCode::Char(c) => {
            if mods.contains(Modifiers::CTRL) {
                let b = (c.to_ascii_uppercase() as u8).wrapping_sub(0x40);
                Some(vec![b & 0x1f])
            } else {
                let mut buf = [0u8; 4];
                Some(c.encode_utf8(&mut buf).as_bytes().to_vec())
            }
        }
        KeyCode::Enter => Some(vec![b'\r']),
        KeyCode::Backspace => Some(vec![0x7f]),
        KeyCode::Tab => Some(vec![b'\t']),
        KeyCode::Escape => Some(vec![0x1b]),
        KeyCode::LeftArrow => Some(b"\x1b[D".to_vec()),
        KeyCode::RightArrow => Some(b"\x1b[C".to_vec()),
        KeyCode::UpArrow => Some(b"\x1b[A".to_vec()),
        KeyCode::DownArrow => Some(b"\x1b[B".to_vec()),
        KeyCode::Delete => Some(b"\x1b[3~".to_vec()),
        KeyCode::Home => Some(b"\x1b[H".to_vec()),
        KeyCode::End => Some(b"\x1b[F".to_vec()),
        _ => None,
    }
}
