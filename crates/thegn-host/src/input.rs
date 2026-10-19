//! Keyboard/mouse input encoding: translate termwiz events into the byte
//! sequences a terminal application expects on its stdin.

use termwiz::input::{KeyCode, Modifiers};

use crate::emulator::MouseMode;

/// Safe keyboard diagnostics. Never format the original key or action payload:
/// terminal echo policy belongs to the child and cannot authorize host logging.
pub(crate) fn log_dispatch(
    raw: &KeyCode,
    raw_mods: Modifiers,
    normalized: &crate::sequence::Key,
    dispatch: &crate::sequence::MatchResult,
) {
    if !tracing::enabled!(target: "thegn::input", tracing::Level::DEBUG) {
        return;
    }
    let result = match dispatch {
        crate::sequence::MatchResult::None => "forwarded",
        crate::sequence::MatchResult::Pending => "pending",
        crate::sequence::MatchResult::Matched(_) => "matched",
    };
    tracing::debug!(target: "thegn::input",
        raw_key = diagnostic_key(raw), raw_mods = ?raw_mods,
        norm_key = diagnostic_key(&normalized.code), norm_mods = ?normalized.mods,
        result, "key dispatch");
}

pub(crate) fn diagnostic_key(key: &KeyCode) -> &'static str {
    // An allowlist prevents future payload-bearing variants from leaking too.
    match key {
        KeyCode::Char(_) => "character",
        KeyCode::UpArrow => "up",
        KeyCode::DownArrow => "down",
        KeyCode::LeftArrow => "left",
        KeyCode::RightArrow => "right",
        KeyCode::Escape => "escape",
        KeyCode::Enter => "enter",
        KeyCode::Tab => "tab",
        KeyCode::Backspace => "backspace",
        KeyCode::Delete => "delete",
        KeyCode::Home => "home",
        KeyCode::End => "end",
        KeyCode::PageUp => "page_up",
        KeyCode::PageDown => "page_down",
        KeyCode::Function(_) => "function",
        _ => "other",
    }
}

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

/// The legacy C0 byte a terminal sends for `Ctrl+<c>`, or `None` when no such
/// byte exists for this character.
///
/// The legacy mapping is only defined for ASCII letters and for
/// ``@ [ \ ] ^ _ ?`` (plus space, which shares NUL with `Ctrl+@`). The naive
/// `to_ascii_uppercase() - 0x40` arithmetic silently produces a *valid but
/// wrong* byte for everything else: `Ctrl+1` becomes `0x11` — Ctrl-Q, i.e.
/// XON, which can un-freeze/freeze flow control in the child. Chords with no
/// legacy byte get the CSI-u form instead (see [`key_bytes_mode`]).
fn legacy_ctrl_byte(c: char) -> Option<u8> {
    match c {
        'a'..='z' | 'A'..='Z' => Some((c.to_ascii_uppercase() as u8) - 0x40),
        ' ' | '@' => Some(0x00),
        '[' => Some(0x1b),
        '\\' => Some(0x1c),
        ']' => Some(0x1d),
        '^' => Some(0x1e),
        '_' => Some(0x1f),
        // Ctrl+? is DEL, not `0x1f` (that is Ctrl+_).
        '?' => Some(0x7f),
        // Already a control byte (a terminal reporting the legacy spelling
        // with the modifier still attached): forward it verbatim.
        c if (c as u32) < 0x20 || c == '\x7f' => Some(c as u8),
        _ => None,
    }
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
    let modified_csi = |final_byte: u8| {
        format!("\x1b[1;{}{}", csi_u_modifier(mods), final_byte as char).into_bytes()
    };
    let tilde_key = |number: u8| {
        if mods.is_empty() {
            format!("\x1b[{number}~").into_bytes()
        } else {
            format!("\x1b[{number};{}~", csi_u_modifier(mods)).into_bytes()
        }
    };
    let meta = |byte: u8| {
        if mods.contains(Modifiers::ALT) {
            vec![0x1b, byte]
        } else {
            vec![byte]
        }
    };
    match key {
        KeyCode::Char(c) => {
            // ALT is the legacy meta prefix: ESC followed by the un-ALTed
            // encoding. Not applied to the CSI-u branch below, where
            // `csi_u_modifier` already carries the ALT bit (prefixing there
            // too would show the child a doubled modifier).
            let meta = |bytes: Vec<u8>| {
                if mods.contains(Modifiers::ALT) {
                    let mut out = Vec::with_capacity(bytes.len() + 1);
                    out.push(0x1b);
                    out.extend_from_slice(&bytes);
                    out
                } else {
                    bytes
                }
            };
            if mods.contains(Modifiers::CTRL) {
                match legacy_ctrl_byte(*c) {
                    Some(b) => Some(meta(vec![b])),
                    // No legacy byte exists for this chord (Ctrl+<digit> and
                    // friends), so forward the CSI-u form rather than a wrong
                    // control byte — see `legacy_ctrl_byte`.
                    None => {
                        Some(format!("\x1b[{};{}u", *c as u32, csi_u_modifier(mods)).into_bytes())
                    }
                }
            } else {
                let mut buf = [0u8; 4];
                Some(meta(c.encode_utf8(&mut buf).as_bytes().to_vec()))
            }
        }
        // Enter's Shift form needs the per-pane opt-in handled by
        // `key_bytes_pane`; all other forms preserve the traditional byte.
        KeyCode::Enter => Some(meta(b'\r')),
        // xterm's common Backspace conventions are DEL, Ctrl-H, and ESC+DEL
        // for Alt. Other modifiers do not change the byte.
        KeyCode::Backspace => Some(meta(if mods.contains(Modifiers::CTRL) {
            0x08
        } else {
            0x7f
        })),
        // Shift+Tab keeps its legacy back-tab encoding (vim/readline expect it).
        KeyCode::Tab if mods == Modifiers::SHIFT => Some(b"\x1b[Z".to_vec()),
        // Ctrl/Alt/Super+Tab have no legacy byte (Tab and Ctrl-I collide), so
        // forward the CSI-u form: it is the only spelling that carries the
        // modifier at all, and an unrecognized CSI is inert in an app that
        // doesn't read it. thegn does NOT push the kitty keyboard protocol
        // (`ESC [ >1u`) — see the note in `run.rs` — so this is "no legacy byte
        // exists", not "a mode we enabled". Tab's codepoint is 9; the modifier
        // param is 1 + bitmask.
        KeyCode::Tab if !(mods & !Modifiers::SHIFT).is_empty() => {
            Some(format!("\x1b[9;{}u", csi_u_modifier(mods)).into_bytes())
        }
        KeyCode::Tab => Some(vec![b'\t']),
        KeyCode::Escape => Some(vec![0x1b]),
        KeyCode::LeftArrow if mods.is_empty() => Some(b"\x1b[D".to_vec()),
        KeyCode::LeftArrow => Some(modified_csi(b'D')),
        KeyCode::RightArrow if mods.is_empty() => Some(b"\x1b[C".to_vec()),
        KeyCode::RightArrow => Some(modified_csi(b'C')),
        KeyCode::UpArrow if mods.is_empty() => Some(b"\x1b[A".to_vec()),
        KeyCode::UpArrow => Some(modified_csi(b'A')),
        KeyCode::DownArrow if mods.is_empty() => Some(b"\x1b[B".to_vec()),
        KeyCode::DownArrow => Some(modified_csi(b'B')),
        KeyCode::Home if mods.is_empty() => Some(b"\x1b[H".to_vec()),
        KeyCode::Home => Some(modified_csi(b'H')),
        KeyCode::End if mods.is_empty() => Some(b"\x1b[F".to_vec()),
        KeyCode::End => Some(modified_csi(b'F')),
        KeyCode::Delete => Some(tilde_key(3)),
        KeyCode::Insert => Some(tilde_key(2)),
        KeyCode::PageUp => Some(tilde_key(5)),
        KeyCode::PageDown => Some(tilde_key(6)),
        KeyCode::Function(n @ 1..=4) if mods.is_empty() => Some(vec![0x1b, b'O', b'P' + (*n - 1)]),
        KeyCode::Function(n @ 1..=4) => Some(modified_csi(b'P' + (*n - 1))),
        KeyCode::Function(n) => match n {
            5 => Some(tilde_key(15)),
            6 => Some(tilde_key(17)),
            7 => Some(tilde_key(18)),
            8 => Some(tilde_key(19)),
            9 => Some(tilde_key(20)),
            10 => Some(tilde_key(21)),
            11 => Some(tilde_key(23)),
            12 => Some(tilde_key(24)),
            _ => None,
        },
        _ => None,
    }
}

/// Encode a key for one pane, including Shift+Enter when that application
/// requested a keyboard mode that can distinguish it.
pub(crate) fn key_bytes_pane(
    key: &KeyCode,
    mods: Modifiers,
    app_cursor: bool,
    extended_keys: bool,
) -> Option<Vec<u8>> {
    if extended_keys && *key == KeyCode::Enter && mods == Modifiers::SHIFT {
        Some(b"\x1b[13;2u".to_vec())
    } else {
        key_bytes_mode(key, mods, app_cursor)
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

    /// `Ctrl+<digit>` has no legacy control byte. The old
    /// `to_ascii_uppercase() - 0x40` arithmetic invented one anyway, and the
    /// byte it invented for `Ctrl+1` was `0x11` — **XON**, which un-freezes
    /// flow control in the child. This is the whole reason the encoding
    /// changed; guard it by name.
    #[test]
    fn ctrl_digit_is_never_the_xon_control_byte() {
        let one = key_bytes(&KeyCode::Char('1'), Modifiers::CTRL).unwrap();
        assert_ne!(one, vec![0x11], "Ctrl+1 must not forward XON (Ctrl-Q)");
        assert_eq!(one, b"\x1b[49;5u");
    }

    #[test]
    fn ctrl_digit_forwards_csi_u() {
        // No legacy byte exists, so the CSI-u form (codepoint ; 1+ctrl(4)).
        for (c, want) in [
            ('0', &b"\x1b[48;5u"[..]),
            ('1', &b"\x1b[49;5u"[..]),
            ('2', &b"\x1b[50;5u"[..]),
            ('9', &b"\x1b[57;5u"[..]),
        ] {
            assert_eq!(
                key_bytes(&KeyCode::Char(c), Modifiers::CTRL).unwrap(),
                want,
                "Ctrl+{c}"
            );
        }
    }

    #[test]
    fn ctrl_letter_and_legacy_punctuation_are_byte_identical() {
        // Every TUI (vim, readline) depends on these exact bytes.
        for (c, want) in [
            ('c', 0x03u8),
            ('C', 0x03),
            ('a', 0x01),
            ('[', 0x1b),
            (']', 0x1d),
            ('\\', 0x1c),
            ('^', 0x1e),
            ('_', 0x1f),
            ('@', 0x00),
            (' ', 0x00),
        ] {
            assert_eq!(
                key_bytes(&KeyCode::Char(c), Modifiers::CTRL).unwrap(),
                vec![want],
                "Ctrl+{c:?}"
            );
        }
        // Ctrl+? is DEL (the legacy byte), not 0x1f.
        assert_eq!(
            key_bytes(&KeyCode::Char('?'), Modifiers::CTRL).unwrap(),
            vec![0x7f]
        );
    }

    #[test]
    fn alt_char_is_esc_prefixed() {
        assert_eq!(
            key_bytes(&KeyCode::Char('w'), Modifiers::ALT).unwrap(),
            vec![0x1b, b'w']
        );
        assert_eq!(
            key_bytes(&KeyCode::Char('1'), Modifiers::ALT).unwrap(),
            vec![0x1b, b'1']
        );
        // Ctrl+Alt over a chord that HAS a legacy byte: ESC + that byte.
        assert_eq!(
            key_bytes(&KeyCode::Char('c'), Modifiers::CTRL | Modifiers::ALT).unwrap(),
            vec![0x1b, 0x03]
        );
        // Ctrl+Alt over a chord that does NOT: the ALT bit rides in the CSI-u
        // modifier (1 + alt(2) + ctrl(4) = 7) — a SINGLE ESC, not a doubled
        // meta prefix.
        assert_eq!(
            key_bytes(&KeyCode::Char('1'), Modifiers::CTRL | Modifiers::ALT).unwrap(),
            b"\x1b[49;7u"
        );
    }

    #[test]
    fn unmodified_chars_round_trip_utf8() {
        assert_eq!(
            key_bytes(&KeyCode::Char('w'), Modifiers::NONE).unwrap(),
            b"w"
        );
        assert_eq!(
            key_bytes(&KeyCode::Char('é'), Modifiers::NONE).unwrap(),
            "é".as_bytes()
        );
    }

    /// Lock down the legacy bytes for unmodified functional keys, alongside
    /// the intentional Enter/Backspace conventions and special Tab handling.
    #[test]
    fn functional_keys_are_byte_identical() {
        let cases: &[(KeyCode, Modifiers, &[u8])] = &[
            (KeyCode::Enter, Modifiers::NONE, b"\r"),
            (KeyCode::Enter, Modifiers::ALT, b"\x1b\r"),
            (KeyCode::Enter, Modifiers::CTRL, b"\r"),
            (KeyCode::Backspace, Modifiers::NONE, b"\x7f"),
            (KeyCode::Backspace, Modifiers::ALT, b"\x1b\x7f"),
            (KeyCode::Backspace, Modifiers::CTRL, b"\x08"),
            (KeyCode::Tab, Modifiers::NONE, b"\t"),
            (KeyCode::Escape, Modifiers::NONE, b"\x1b"),
            (KeyCode::Escape, Modifiers::ALT, b"\x1b"),
            (KeyCode::LeftArrow, Modifiers::NONE, b"\x1b[D"),
            (KeyCode::RightArrow, Modifiers::NONE, b"\x1b[C"),
            (KeyCode::UpArrow, Modifiers::NONE, b"\x1b[A"),
            (KeyCode::DownArrow, Modifiers::NONE, b"\x1b[B"),
            (KeyCode::UpArrow, Modifiers::ALT, b"\x1b[1;3A"),
            (KeyCode::Delete, Modifiers::NONE, b"\x1b[3~"),
            (KeyCode::Home, Modifiers::NONE, b"\x1b[H"),
            (KeyCode::End, Modifiers::NONE, b"\x1b[F"),
        ];
        for (key, mods, want) in cases {
            assert_eq!(
                key_bytes(key, *mods).as_deref(),
                Some(*want),
                "{key:?} + {mods:?}"
            );
        }
    }

    #[test]
    fn functional_keys_encode_shift_alt_ctrl_and_ctrl_shift() {
        let mods = [
            Modifiers::NONE,
            Modifiers::SHIFT,
            Modifiers::ALT,
            Modifiers::CTRL,
            Modifiers::CTRL | Modifiers::SHIFT,
        ];
        let cases: &[(KeyCode, [&[u8]; 5])] = &[
            (KeyCode::Enter, [b"\r", b"\r", b"\x1b\r", b"\r", b"\r"]),
            (
                KeyCode::Backspace,
                [b"\x7f", b"\x7f", b"\x1b\x7f", b"\x08", b"\x08"],
            ),
            (
                KeyCode::Escape,
                [b"\x1b", b"\x1b", b"\x1b", b"\x1b", b"\x1b"],
            ),
            (
                KeyCode::Tab,
                [b"\t", b"\x1b[Z", b"\x1b[9;3u", b"\x1b[9;5u", b"\x1b[9;6u"],
            ),
            (
                KeyCode::LeftArrow,
                [
                    b"\x1b[D",
                    b"\x1b[1;2D",
                    b"\x1b[1;3D",
                    b"\x1b[1;5D",
                    b"\x1b[1;6D",
                ],
            ),
            (
                KeyCode::RightArrow,
                [
                    b"\x1b[C",
                    b"\x1b[1;2C",
                    b"\x1b[1;3C",
                    b"\x1b[1;5C",
                    b"\x1b[1;6C",
                ],
            ),
            (
                KeyCode::UpArrow,
                [
                    b"\x1b[A",
                    b"\x1b[1;2A",
                    b"\x1b[1;3A",
                    b"\x1b[1;5A",
                    b"\x1b[1;6A",
                ],
            ),
            (
                KeyCode::DownArrow,
                [
                    b"\x1b[B",
                    b"\x1b[1;2B",
                    b"\x1b[1;3B",
                    b"\x1b[1;5B",
                    b"\x1b[1;6B",
                ],
            ),
            (
                KeyCode::Home,
                [
                    b"\x1b[H",
                    b"\x1b[1;2H",
                    b"\x1b[1;3H",
                    b"\x1b[1;5H",
                    b"\x1b[1;6H",
                ],
            ),
            (
                KeyCode::End,
                [
                    b"\x1b[F",
                    b"\x1b[1;2F",
                    b"\x1b[1;3F",
                    b"\x1b[1;5F",
                    b"\x1b[1;6F",
                ],
            ),
            (
                KeyCode::Delete,
                [
                    b"\x1b[3~",
                    b"\x1b[3;2~",
                    b"\x1b[3;3~",
                    b"\x1b[3;5~",
                    b"\x1b[3;6~",
                ],
            ),
            (
                KeyCode::Insert,
                [
                    b"\x1b[2~",
                    b"\x1b[2;2~",
                    b"\x1b[2;3~",
                    b"\x1b[2;5~",
                    b"\x1b[2;6~",
                ],
            ),
            (
                KeyCode::PageUp,
                [
                    b"\x1b[5~",
                    b"\x1b[5;2~",
                    b"\x1b[5;3~",
                    b"\x1b[5;5~",
                    b"\x1b[5;6~",
                ],
            ),
            (
                KeyCode::PageDown,
                [
                    b"\x1b[6~",
                    b"\x1b[6;2~",
                    b"\x1b[6;3~",
                    b"\x1b[6;5~",
                    b"\x1b[6;6~",
                ],
            ),
            (
                KeyCode::Function(1),
                [
                    b"\x1bOP",
                    b"\x1b[1;2P",
                    b"\x1b[1;3P",
                    b"\x1b[1;5P",
                    b"\x1b[1;6P",
                ],
            ),
            (
                KeyCode::Function(2),
                [
                    b"\x1bOQ",
                    b"\x1b[1;2Q",
                    b"\x1b[1;3Q",
                    b"\x1b[1;5Q",
                    b"\x1b[1;6Q",
                ],
            ),
            (
                KeyCode::Function(3),
                [
                    b"\x1bOR",
                    b"\x1b[1;2R",
                    b"\x1b[1;3R",
                    b"\x1b[1;5R",
                    b"\x1b[1;6R",
                ],
            ),
            (
                KeyCode::Function(4),
                [
                    b"\x1bOS",
                    b"\x1b[1;2S",
                    b"\x1b[1;3S",
                    b"\x1b[1;5S",
                    b"\x1b[1;6S",
                ],
            ),
            (
                KeyCode::Function(5),
                [
                    b"\x1b[15~",
                    b"\x1b[15;2~",
                    b"\x1b[15;3~",
                    b"\x1b[15;5~",
                    b"\x1b[15;6~",
                ],
            ),
            (
                KeyCode::Function(6),
                [
                    b"\x1b[17~",
                    b"\x1b[17;2~",
                    b"\x1b[17;3~",
                    b"\x1b[17;5~",
                    b"\x1b[17;6~",
                ],
            ),
            (
                KeyCode::Function(7),
                [
                    b"\x1b[18~",
                    b"\x1b[18;2~",
                    b"\x1b[18;3~",
                    b"\x1b[18;5~",
                    b"\x1b[18;6~",
                ],
            ),
            (
                KeyCode::Function(8),
                [
                    b"\x1b[19~",
                    b"\x1b[19;2~",
                    b"\x1b[19;3~",
                    b"\x1b[19;5~",
                    b"\x1b[19;6~",
                ],
            ),
            (
                KeyCode::Function(9),
                [
                    b"\x1b[20~",
                    b"\x1b[20;2~",
                    b"\x1b[20;3~",
                    b"\x1b[20;5~",
                    b"\x1b[20;6~",
                ],
            ),
            (
                KeyCode::Function(10),
                [
                    b"\x1b[21~",
                    b"\x1b[21;2~",
                    b"\x1b[21;3~",
                    b"\x1b[21;5~",
                    b"\x1b[21;6~",
                ],
            ),
            (
                KeyCode::Function(11),
                [
                    b"\x1b[23~",
                    b"\x1b[23;2~",
                    b"\x1b[23;3~",
                    b"\x1b[23;5~",
                    b"\x1b[23;6~",
                ],
            ),
            (
                KeyCode::Function(12),
                [
                    b"\x1b[24~",
                    b"\x1b[24;2~",
                    b"\x1b[24;3~",
                    b"\x1b[24;5~",
                    b"\x1b[24;6~",
                ],
            ),
        ];
        for (key, expected) in cases {
            for (mods, want) in mods.into_iter().zip(expected) {
                assert_eq!(
                    key_bytes(key, mods).as_deref(),
                    Some(*want),
                    "{key:?} + {mods:?}"
                );
            }
        }
        for mods in mods {
            assert_eq!(key_bytes(&KeyCode::Function(13), mods), None);
        }
    }

    #[test]
    fn functional_keys_encode_super_alt_shift_and_ctrl_alt() {
        let mods = [
            Modifiers::SUPER,
            Modifiers::ALT | Modifiers::SHIFT,
            Modifiers::CTRL | Modifiers::ALT,
        ];
        let cases: &[(KeyCode, [&[u8]; 3])] = &[
            (KeyCode::Enter, [b"\r", b"\x1b\r", b"\x1b\r"]),
            (KeyCode::Backspace, [b"\x7f", b"\x1b\x7f", b"\x1b\x08"]),
            (KeyCode::Escape, [b"\x1b", b"\x1b", b"\x1b"]),
            (KeyCode::Tab, [b"\x1b[9;9u", b"\x1b[9;4u", b"\x1b[9;7u"]),
            (
                KeyCode::LeftArrow,
                [b"\x1b[1;9D", b"\x1b[1;4D", b"\x1b[1;7D"],
            ),
            (
                KeyCode::RightArrow,
                [b"\x1b[1;9C", b"\x1b[1;4C", b"\x1b[1;7C"],
            ),
            (KeyCode::UpArrow, [b"\x1b[1;9A", b"\x1b[1;4A", b"\x1b[1;7A"]),
            (
                KeyCode::DownArrow,
                [b"\x1b[1;9B", b"\x1b[1;4B", b"\x1b[1;7B"],
            ),
            (KeyCode::Home, [b"\x1b[1;9H", b"\x1b[1;4H", b"\x1b[1;7H"]),
            (KeyCode::End, [b"\x1b[1;9F", b"\x1b[1;4F", b"\x1b[1;7F"]),
            (KeyCode::Delete, [b"\x1b[3;9~", b"\x1b[3;4~", b"\x1b[3;7~"]),
            (KeyCode::Insert, [b"\x1b[2;9~", b"\x1b[2;4~", b"\x1b[2;7~"]),
            (KeyCode::PageUp, [b"\x1b[5;9~", b"\x1b[5;4~", b"\x1b[5;7~"]),
            (
                KeyCode::PageDown,
                [b"\x1b[6;9~", b"\x1b[6;4~", b"\x1b[6;7~"],
            ),
            (
                KeyCode::Function(1),
                [b"\x1b[1;9P", b"\x1b[1;4P", b"\x1b[1;7P"],
            ),
            (
                KeyCode::Function(2),
                [b"\x1b[1;9Q", b"\x1b[1;4Q", b"\x1b[1;7Q"],
            ),
            (
                KeyCode::Function(3),
                [b"\x1b[1;9R", b"\x1b[1;4R", b"\x1b[1;7R"],
            ),
            (
                KeyCode::Function(4),
                [b"\x1b[1;9S", b"\x1b[1;4S", b"\x1b[1;7S"],
            ),
            (
                KeyCode::Function(5),
                [b"\x1b[15;9~", b"\x1b[15;4~", b"\x1b[15;7~"],
            ),
            (
                KeyCode::Function(6),
                [b"\x1b[17;9~", b"\x1b[17;4~", b"\x1b[17;7~"],
            ),
            (
                KeyCode::Function(7),
                [b"\x1b[18;9~", b"\x1b[18;4~", b"\x1b[18;7~"],
            ),
            (
                KeyCode::Function(8),
                [b"\x1b[19;9~", b"\x1b[19;4~", b"\x1b[19;7~"],
            ),
            (
                KeyCode::Function(9),
                [b"\x1b[20;9~", b"\x1b[20;4~", b"\x1b[20;7~"],
            ),
            (
                KeyCode::Function(10),
                [b"\x1b[21;9~", b"\x1b[21;4~", b"\x1b[21;7~"],
            ),
            (
                KeyCode::Function(11),
                [b"\x1b[23;9~", b"\x1b[23;4~", b"\x1b[23;7~"],
            ),
            (
                KeyCode::Function(12),
                [b"\x1b[24;9~", b"\x1b[24;4~", b"\x1b[24;7~"],
            ),
        ];
        for (key, expected) in cases {
            for (mods, want) in mods.into_iter().zip(expected) {
                assert_eq!(
                    key_bytes(key, mods).as_deref(),
                    Some(*want),
                    "{key:?} + {mods:?}"
                );
            }
        }
    }

    #[test]
    fn f1_through_f4_are_ss3_with_or_without_decckm() {
        for (n, final_byte) in (1..=4).zip(b"PQRS") {
            let key = KeyCode::Function(n);
            let want = [0x1b, b'O', *final_byte];
            assert_eq!(
                key_bytes_mode(&key, Modifiers::NONE, false).as_deref(),
                Some(&want[..])
            );
            assert_eq!(
                key_bytes_mode(&key, Modifiers::NONE, true).as_deref(),
                Some(&want[..])
            );
        }
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
            key_bytes_mode(&KeyCode::UpArrow, Modifiers::SHIFT, true).unwrap(),
            b"\x1b[1;2A"
        );
        assert_eq!(
            key_bytes_mode(&KeyCode::Home, Modifiers::NONE, true).unwrap(),
            b"\x1bOH"
        );
    }

    #[test]
    fn shift_enter_uses_csi_u_only_when_the_pane_opted_in() {
        assert_eq!(
            key_bytes_pane(&KeyCode::Enter, Modifiers::SHIFT, false, true).as_deref(),
            Some(&b"\x1b[13;2u"[..])
        );
        assert_eq!(
            key_bytes_pane(&KeyCode::Enter, Modifiers::SHIFT, false, false).as_deref(),
            Some(&b"\r"[..])
        );
        for (mods, expected) in [
            (Modifiers::CTRL, &b"\r"[..]),
            (Modifiers::ALT, &b"\x1b\r"[..]),
            (Modifiers::SHIFT | Modifiers::ALT, &b"\x1b\r"[..]),
            (Modifiers::NONE, &b"\r"[..]),
        ] {
            assert_eq!(
                key_bytes_pane(&KeyCode::Enter, mods, false, true).as_deref(),
                Some(expected)
            );
        }
    }

    #[test]
    fn pane_encoder_delegates_non_shift_enter_keys_unchanged() {
        for key in [KeyCode::UpArrow, KeyCode::Delete, KeyCode::Char('x')] {
            for app_cursor in [false, true] {
                for extended_keys in [false, true] {
                    assert_eq!(
                        key_bytes_pane(&key, Modifiers::NONE, app_cursor, extended_keys),
                        key_bytes_mode(&key, Modifiers::NONE, app_cursor),
                        "{key:?}, app_cursor={app_cursor}, extended_keys={extended_keys}"
                    );
                }
            }
        }
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

#[cfg(test)]
mod diagnostic_tests {
    use super::*;
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);
    impl Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn compositor_log_targets_cannot_serialize_raw_keyboard_payloads() {
        // Cover every tracing target in the compositor, including startup/
        // dormant-frame paths outside normal key dispatch.
        let source = include_str!("run.rs");
        for call in source.split("tracing::").skip(1) {
            let call: String = call
                .split(");")
                .next()
                .unwrap()
                .chars()
                .filter(|c| !c.is_whitespace())
                .collect();
            for forbidden in [
                "?k.key",
                "?input_key.code",
                "?dispatch",
                "key=?",
                "event=?k,",
                "input=?k,",
            ] {
                assert!(
                    !call.contains(forbidden),
                    "raw keyboard payload in tracing call: {forbidden}"
                );
            }
        }
        assert!(source.contains("key = crate::input::diagnostic_key(&k.key)"));
    }

    #[test]
    fn broad_diagnostics_never_capture_characters_or_action_payloads() {
        for filter in [
            "debug,thegn=debug,thegn_core=debug,thegn_svc=debug",
            "trace",
        ] {
            let capture = Capture::default();
            let writer = capture.clone();
            let subscriber = tracing_subscriber::fmt()
                .with_env_filter(filter)
                .without_time()
                .with_ansi(false)
                .with_writer(move || writer.clone())
                .finish();
            tracing::subscriber::with_default(subscriber, || {
                for key in [
                    KeyCode::Char('☃'),
                    KeyCode::Char('密'),
                    KeyCode::Char('\x01'),
                    KeyCode::Numpad7,
                ] {
                    tracing::debug!(target: "thegn::frame", key = diagnostic_key(&key), "dormant dismissed by key");
                    let normalized = crate::sequence::Key::modified(key, Modifiers::CTRL);
                    log_dispatch(
                        &key,
                        Modifiers::CTRL,
                        &normalized,
                        &crate::sequence::MatchResult::None,
                    );
                    log_dispatch(
                        &key,
                        Modifiers::CTRL,
                        &normalized,
                        &crate::sequence::MatchResult::Pending,
                    );
                    log_dispatch(
                        &key,
                        Modifiers::CTRL,
                        &normalized,
                        &crate::sequence::MatchResult::Matched(crate::keymap::Action::Custom(
                            54321,
                        )),
                    );
                }
            });
            let output = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
            for forbidden in ["☃", "密", "Char(", "Numpad7", "54321", "Custom(", "\\u{1}"] {
                assert!(
                    !output.contains(forbidden),
                    "sensitive payload {forbidden:?} in {output}"
                );
            }
            for retained in ["CTRL", "forwarded", "pending", "matched", "character"] {
                assert!(output.contains(retained), "missing diagnostic {retained}");
            }
        }
    }
}
