//! Mouse-residue filter: when an SGR mouse report (`ESC [ < b;x;y M`) is split
//! across terminal reads, termwiz fails to reassemble it and instead emits the
//! fragment as plain key events (`Alt+[`, `<`, `3`, `2`, `;`, … `M`). Forwarded
//! to a pane those spray `[<32;56;15M` into the shell — exactly what a drag
//! flood produces. This filter watches the key stream and swallows such
//! fragments.
//!
//! The same `Alt+[` residue can introduce a split xterm `CSI n[;m]~` key.
//! Those bodies are buffered only up to a fixed bound, then decoded or replayed
//! as ordinary keys when malformed or unknown.

use termwiz::input::{KeyCode, KeyEvent, Modifiers};

const CSI_KEY_BUFFER_LIMIT: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
enum State {
    /// Passing keys through.
    #[default]
    Idle,
    /// Saw `Alt+[` (or `ESC` then `[`): the CSI introducer of a fragment.
    SawCsi,
    /// Inside `< digits ; digits ; digits` — swallowing until `M`/`m`.
    InMouse,
    /// Buffering the body of a possible xterm `CSI n[;m]~` key.
    InKeyCsi(Vec<KeyEvent>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Pass,
    Swallow,
    Emit(KeyEvent),
    Replay(Vec<KeyEvent>),
}

/// Stateful filter; feed every key event destined for dispatch. `swallow`
/// returns `true` when the key is mouse residue and must be dropped.
#[derive(Debug, Default)]
pub struct MouseResidueFilter {
    state: State,
}

impl MouseResidueFilter {
    /// Reassemble xterm modified tilde keys as well as discarding split SGR
    /// mouse reports. The swallowed `Alt+[` remains swallowed on malformed
    /// sequences, matching the historical Alt+[ policy.
    pub fn feed(&mut self, key: &KeyCode, mods: Modifiers) -> Verdict {
        let state = std::mem::replace(&mut self.state, State::Idle);
        match state {
            State::Idle => {
                if *key == KeyCode::Char('[') && mods == Modifiers::ALT {
                    self.state = State::SawCsi;
                    Verdict::Swallow
                } else {
                    Verdict::Pass
                }
            }
            State::SawCsi => match key {
                KeyCode::Char('<') if mods.is_empty() => {
                    self.state = State::InMouse;
                    Verdict::Swallow
                }
                KeyCode::Char(c) if mods.is_empty() && c.is_ascii_digit() => {
                    self.state = State::InKeyCsi(vec![key_event(key, mods)]);
                    Verdict::Swallow
                }
                _ => {
                    self.state = State::Idle;
                    Verdict::Pass
                }
            },
            State::InMouse => match key {
                KeyCode::Char(c) if mods.is_empty() && (c.is_ascii_digit() || *c == ';') => {
                    Verdict::Swallow
                }
                KeyCode::Char('M' | 'm') if mods.is_empty() => {
                    self.state = State::Idle;
                    Verdict::Swallow
                }
                _ => {
                    self.state = State::Idle;
                    Verdict::Pass
                }
            },
            State::InKeyCsi(buffer) => self.feed_key_csi(buffer, key, mods),
        }
    }

    fn feed_key_csi(
        &mut self,
        mut buffer: Vec<KeyEvent>,
        key: &KeyCode,
        mods: Modifiers,
    ) -> Verdict {
        if mods.is_empty() && matches!(key, KeyCode::Char(c) if c.is_ascii_digit() || *c == ';') {
            if buffer.len() < CSI_KEY_BUFFER_LIMIT {
                buffer.push(key_event(key, mods));
                self.state = State::InKeyCsi(buffer);
                return Verdict::Swallow;
            }
            let mut replay = buffer;
            replay.push(key_event(key, mods));
            return Verdict::Replay(replay);
        }
        if mods.is_empty() && *key == KeyCode::Char('~') {
            let event = parse_tilde_key(&buffer);
            if let Some(event) = event {
                return Verdict::Emit(event);
            }
        }
        let mut replay = buffer;
        replay.push(key_event(key, mods));
        Verdict::Replay(replay)
    }

    /// Compatibility wrapper for the existing mouse-filter call sites/tests.
    pub fn swallow(&mut self, key: &KeyCode, mods: Modifiers) -> bool {
        matches!(self.feed(key, mods), Verdict::Swallow | Verdict::Emit(_))
    }
}

fn key_event(key: &KeyCode, modifiers: Modifiers) -> KeyEvent {
    KeyEvent {
        key: *key,
        modifiers,
    }
}

fn parse_tilde_key(buffer: &[KeyEvent]) -> Option<KeyEvent> {
    let text: String = buffer
        .iter()
        .map(|event| match event.key {
            KeyCode::Char(c) if event.modifiers.is_empty() => Some(c),
            _ => None,
        })
        .collect::<Option<String>>()?;
    let mut parts = text.split(';');
    let number = parts.next()?.parse::<u8>().ok()?;
    let modifier_param = parts.next().map(str::parse::<u8>).transpose().ok()?;
    if parts.next().is_some() {
        return None;
    }
    let code = match number {
        1 | 7 => KeyCode::Home,
        2 => KeyCode::Insert,
        3 => KeyCode::Delete,
        4 | 8 => KeyCode::End,
        5 => KeyCode::PageUp,
        6 => KeyCode::PageDown,
        _ => return None,
    };
    let modifier = modifier_param.unwrap_or(1);
    if !(1..=16).contains(&modifier) {
        return None;
    }
    let bits = modifier - 1;
    let mut modifiers = Modifiers::NONE;
    if bits & 1 != 0 {
        modifiers |= Modifiers::SHIFT;
    }
    if bits & 2 != 0 {
        modifiers |= Modifiers::ALT;
    }
    if bits & 4 != 0 {
        modifiers |= Modifiers::CTRL;
    }
    if bits & 8 != 0 {
        modifiers |= Modifiers::SUPER;
    }
    Some(KeyEvent {
        key: code,
        modifiers,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key_sequence(body: &str) -> Verdict {
        let mut filter = MouseResidueFilter::default();
        assert_eq!(
            filter.feed(&KeyCode::Char('['), Modifiers::ALT),
            Verdict::Swallow
        );
        let mut verdict = Verdict::Pass;
        for c in body.chars() {
            verdict = filter.feed(&KeyCode::Char(c), Modifiers::NONE);
        }
        verdict
    }

    fn feed(f: &mut MouseResidueFilter, s: &str, alt_first: bool) -> Vec<bool> {
        s.chars()
            .enumerate()
            .map(|(i, c)| {
                let mods = if i == 0 && alt_first {
                    Modifiers::ALT
                } else {
                    Modifiers::NONE
                };
                f.swallow(&KeyCode::Char(c), mods)
            })
            .collect()
    }

    #[test]
    fn swallows_a_split_drag_fragment() {
        let mut f = MouseResidueFilter::default();
        // termwiz delivers: Alt+[ then `<32;56;15M` as plain chars.
        let out = feed(&mut f, "[<32;56;15M", true);
        assert!(
            out.iter().all(|&s| s),
            "every fragment key swallowed: {out:?}"
        );
        // Back to passing afterwards.
        assert!(!f.swallow(&KeyCode::Char('x'), Modifiers::NONE));
    }

    #[test]
    fn ordinary_typing_passes_through() {
        let mut f = MouseResidueFilter::default();
        for c in "ls -la <<< done;Mm".chars() {
            assert!(!f.swallow(&KeyCode::Char(c), Modifiers::NONE), "{c}");
        }
        assert!(!f.swallow(&KeyCode::Enter, Modifiers::NONE));
    }

    #[test]
    fn introducer_without_mouse_body_resumes() {
        let mut f = MouseResidueFilter::default();
        assert!(f.swallow(&KeyCode::Char('['), Modifiers::ALT));
        // Next key isn't '<': not a mouse report; it passes.
        assert!(!f.swallow(&KeyCode::Char('a'), Modifiers::NONE));
        assert!(!f.swallow(&KeyCode::Char('b'), Modifiers::NONE));
    }

    #[test]
    fn release_fragment_lowercase_m_terminates() {
        let mut f = MouseResidueFilter::default();
        let out = feed(&mut f, "[<0;12;5m", true);
        assert!(out.iter().all(|&s| s), "{out:?}");
    }

    #[test]
    fn xterm_modified_tilde_keys_are_reassembled() {
        for (sequence, key, modifiers) in [
            ("5;5~", KeyCode::PageUp, Modifiers::CTRL),
            ("3;2~", KeyCode::Delete, Modifiers::SHIFT),
            ("6;3~", KeyCode::PageDown, Modifiers::ALT),
            ("2;6~", KeyCode::Insert, Modifiers::CTRL | Modifiers::SHIFT),
            ("5~", KeyCode::PageUp, Modifiers::NONE),
        ] {
            assert_eq!(
                key_sequence(sequence),
                Verdict::Emit(KeyEvent { key, modifiers }),
                "CSI {sequence}"
            );
        }
    }

    #[test]
    fn malformed_and_unknown_tilde_keys_replay_buffered_text() {
        assert_eq!(
            key_sequence("5;x"),
            Verdict::Replay(vec![
                key_event(&KeyCode::Char('5'), Modifiers::NONE),
                key_event(&KeyCode::Char(';'), Modifiers::NONE),
                key_event(&KeyCode::Char('x'), Modifiers::NONE),
            ])
        );
        assert_eq!(
            key_sequence("9;5~"),
            Verdict::Replay(
                "9;5~"
                    .chars()
                    .map(|c| key_event(&KeyCode::Char(c), Modifiers::NONE))
                    .collect()
            )
        );
    }

    #[test]
    fn overlong_key_sequence_replays_at_the_fixed_buffer_bound() {
        let mut filter = MouseResidueFilter::default();
        assert_eq!(
            filter.feed(&KeyCode::Char('['), Modifiers::ALT),
            Verdict::Swallow
        );
        for c in "12345678".chars() {
            assert_eq!(
                filter.feed(&KeyCode::Char(c), Modifiers::NONE),
                Verdict::Swallow
            );
        }
        assert_eq!(
            filter.feed(&KeyCode::Char('9'), Modifiers::NONE),
            Verdict::Replay(
                "123456789"
                    .chars()
                    .map(|c| key_event(&KeyCode::Char(c), Modifiers::NONE))
                    .collect()
            )
        );
        assert_eq!(
            filter.feed(&KeyCode::Char('x'), Modifiers::NONE),
            Verdict::Pass
        );
    }
}
