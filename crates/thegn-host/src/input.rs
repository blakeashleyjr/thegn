//! Keyboard/mouse input encoding: translate termwiz events into the byte
//! sequences a terminal application expects on its stdin.

use termwiz::input::{KeyCode, Modifiers};

use crate::emulator::MouseMode;

/// True when a termwiz key event represents the physical Escape key.
///
/// Most terminals decode Esc as [`KeyCode::Escape`], but CSI-u/fixterms
/// terminals can report the same key as `KeyCode::Char('\x1b')` (termwiz's
/// basic CSI-u map does this for `ESC [ 27 u`). Host overlays should treat both
/// spellings as cancel/dismiss so Esc never gets appended to palette/input text.
pub(crate) fn is_escape_key(key: &KeyCode) -> bool {
    matches!(key, KeyCode::Escape | KeyCode::Char('\x1b'))
}

/// Translate a termwiz key event into the bytes a terminal application expects on
/// stdin (normal cursor-key mode).
pub(crate) fn key_bytes(key: &KeyCode, mods: Modifiers) -> Option<Vec<u8>> {
    key_bytes_mode(key, mods, false)
}

/// The CSI-u / kitty modifier parameter: `1 + bitmask` where shift=1, alt=2,
/// ctrl=4, super=8.
fn csi_u_modifier(mods: Modifiers) -> u8 {
    let mut bits = 0u8;
    if mods.contains(Modifiers::SHIFT) {
        bits |= 1;
    }
    if mods.contains(Modifiers::ALT) {
        bits |= 2;
    }
    if mods.contains(Modifiers::CTRL) {
        bits |= 4;
    }
    if mods.contains(Modifiers::SUPER) {
        bits |= 8;
    }
    1 + bits
}

/// As [`key_bytes`], honoring DECCKM: when the app set application cursor
/// keys, unmodified arrows/Home/End are SS3-encoded (`ESC O A`) — full-screen
/// apps (htop, less, vim) expect exactly the encoding their terminfo entry
/// advertises, and feeding CSI arrows instead reads as a bare `ESC` (which
/// htop treats as "reset to top").
pub(crate) fn key_bytes_mode(key: &KeyCode, mods: Modifiers, app_cursor: bool) -> Option<Vec<u8>> {
    if app_cursor && mods.is_empty() {
        let ss3 = |c: u8| Some(vec![0x1b, b'O', c]);
        match key {
            KeyCode::UpArrow => return ss3(b'A'),
            KeyCode::DownArrow => return ss3(b'B'),
            KeyCode::RightArrow => return ss3(b'C'),
            KeyCode::LeftArrow => return ss3(b'D'),
            KeyCode::Home => return ss3(b'H'),
            KeyCode::End => return ss3(b'F'),
            _ => {}
        }
    }
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
        // Shift+Tab keeps its legacy back-tab encoding (vim/readline expect it).
        KeyCode::Tab if mods == Modifiers::SHIFT => Some(b"\x1b[Z".to_vec()),
        // Ctrl/Alt/Super+Tab have no legacy byte (Tab and Ctrl-I collide), so
        // forward the CSI-u form the host's own kitty-keyboard mode (ESC [ >1u)
        // disambiguates. Tab's codepoint is 9; the modifier param is 1 + bitmask.
        KeyCode::Tab if !(mods & !Modifiers::SHIFT).is_empty() => {
            Some(format!("\x1b[9;{}u", csi_u_modifier(mods)).into_bytes())
        }
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

/// A mouse event normalized for pane forwarding, with 0-based pane-relative
/// cell coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PaneMouse {
    Press(u8), // 0 = left, 1 = middle, 2 = right
    Release(u8),
    /// Motion with this button held (left drag etc.).
    Drag(u8),
    /// Motion with no button held (AnyMotion only).
    Move,
    WheelUp,
    WheelDown,
}

/// Encode a mouse event for the app inside a pane, honoring the reporting
/// `mode` it requested and its encoding (SGR vs legacy X10 bytes). Returns
/// `None` when the mode doesn't include this event kind. `col`/`row` are
/// 0-based pane-relative cells.
pub(crate) fn encode_mouse(
    ev: PaneMouse,
    mode: MouseMode,
    sgr: bool,
    col: u16,
    row: u16,
) -> Option<Vec<u8>> {
    let wanted = match (ev, mode) {
        (_, MouseMode::None) => false,
        (PaneMouse::Press(_) | PaneMouse::WheelUp | PaneMouse::WheelDown, _) => true,
        (PaneMouse::Release(_), m) => m != MouseMode::Press,
        (PaneMouse::Drag(_), MouseMode::ButtonMotion | MouseMode::AnyMotion) => true,
        (PaneMouse::Move, MouseMode::AnyMotion) => true,
        _ => false,
    };
    if !wanted {
        return None;
    }
    let (code, release) = match ev {
        PaneMouse::Press(b) => (b, false),
        PaneMouse::Release(b) => (b, true),
        PaneMouse::Drag(b) => (b + 32, false),
        PaneMouse::Move => (35, false),
        PaneMouse::WheelUp => (64, false),
        PaneMouse::WheelDown => (65, false),
    };
    let (x, y) = (col as u32 + 1, row as u32 + 1);
    if sgr {
        let fin = if release { 'm' } else { 'M' };
        Some(format!("\x1b[<{code};{x};{y}{fin}").into_bytes())
    } else {
        // Legacy X10 bytes: 32 + code (release reports button 3), 32 + coord.
        let code = if release { 3 } else { code };
        let clamp = |v: u32| (32 + v).min(255) as u8;
        Some(vec![0x1b, b'[', b'M', 32 + code, clamp(x), clamp(y)])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_key_helper_accepts_termwiz_csi_u_escape_char() {
        assert!(is_escape_key(&KeyCode::Escape));
        assert!(is_escape_key(&KeyCode::Char('\x1b')));
        assert!(!is_escape_key(&KeyCode::Char('q')));
    }

    #[test]
    fn shift_tab_forwards_reverse_tab_sequence() {
        assert_eq!(
            key_bytes_mode(&KeyCode::Tab, Modifiers::SHIFT, false).unwrap(),
            b"\x1b[Z"
        );
        assert_eq!(key_bytes(&KeyCode::Tab, Modifiers::NONE).unwrap(), b"\t");
    }

    #[test]
    fn modified_tab_forwards_csi_u() {
        // Ctrl+Tab has no legacy byte; forward the CSI-u form (Tab=9, ctrl mod=5)
        // instead of silently collapsing to a plain Tab.
        assert_eq!(
            key_bytes(&KeyCode::Tab, Modifiers::CTRL).unwrap(),
            b"\x1b[9;5u"
        );
        // Ctrl+Shift+Tab still disambiguates (mod = 1 + shift(1) + ctrl(4) = 6).
        assert_eq!(
            key_bytes(&KeyCode::Tab, Modifiers::CTRL | Modifiers::SHIFT).unwrap(),
            b"\x1b[9;6u"
        );
        // Alt+Tab (mod = 1 + alt(2) = 3).
        assert_eq!(
            key_bytes(&KeyCode::Tab, Modifiers::ALT).unwrap(),
            b"\x1b[9;3u"
        );
    }

    #[test]
    fn app_cursor_switches_arrows_to_ss3() {
        assert_eq!(
            key_bytes_mode(&KeyCode::UpArrow, Modifiers::NONE, true).unwrap(),
            b"\x1bOA"
        );
        assert_eq!(
            key_bytes_mode(&KeyCode::UpArrow, Modifiers::NONE, false).unwrap(),
            b"\x1b[A"
        );
        // Modified arrows keep CSI even in app-cursor mode.
        assert_eq!(
            key_bytes_mode(&KeyCode::UpArrow, Modifiers::CTRL, true),
            key_bytes(&KeyCode::UpArrow, Modifiers::CTRL)
        );
        assert_eq!(
            key_bytes_mode(&KeyCode::Home, Modifiers::NONE, true).unwrap(),
            b"\x1bOH"
        );
    }

    #[test]
    fn mouse_encoding_honors_mode_and_format() {
        use crate::emulator::MouseMode as M;
        // Press always reports (when any mode is on), SGR formats with M.
        assert_eq!(
            encode_mouse(PaneMouse::Press(0), M::PressRelease, true, 4, 9).unwrap(),
            b"\x1b[<0;5;10M"
        );
        // Release uses lowercase m in SGR, suppressed entirely in Press mode.
        assert_eq!(
            encode_mouse(PaneMouse::Release(0), M::PressRelease, true, 4, 9).unwrap(),
            b"\x1b[<0;5;10m"
        );
        assert!(encode_mouse(PaneMouse::Release(0), M::Press, true, 4, 9).is_none());
        // Drag only in motion modes; +32 button code.
        assert!(encode_mouse(PaneMouse::Drag(0), M::PressRelease, true, 1, 1).is_none());
        assert_eq!(
            encode_mouse(PaneMouse::Drag(0), M::ButtonMotion, true, 1, 1).unwrap(),
            b"\x1b[<32;2;2M"
        );
        // Bare motion only in AnyMotion.
        assert!(encode_mouse(PaneMouse::Move, M::ButtonMotion, true, 0, 0).is_none());
        assert_eq!(
            encode_mouse(PaneMouse::Move, M::AnyMotion, true, 0, 0).unwrap(),
            b"\x1b[<35;1;1M"
        );
        // Wheel.
        assert_eq!(
            encode_mouse(PaneMouse::WheelUp, M::Press, true, 0, 0).unwrap(),
            b"\x1b[<64;1;1M"
        );
        // Legacy X10 byte encoding.
        assert_eq!(
            encode_mouse(PaneMouse::Press(0), M::PressRelease, false, 0, 0).unwrap(),
            vec![0x1b, b'[', b'M', 32, 33, 33]
        );
        assert_eq!(
            encode_mouse(PaneMouse::Release(0), M::PressRelease, false, 0, 0).unwrap(),
            vec![0x1b, b'[', b'M', 35, 33, 33]
        );
        // Nothing when the app didn't ask.
        assert!(encode_mouse(PaneMouse::Press(0), M::None, true, 0, 0).is_none());
    }
}
