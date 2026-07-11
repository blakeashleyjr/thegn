//! Mouse-residue filter: when an SGR mouse report (`ESC [ < b;x;y M`) is split
//! across terminal reads, termwiz fails to reassemble it and instead emits the
//! fragment as plain key events (`Alt+[`, `<`, `3`, `2`, `;`, … `M`). Forwarded
//! to a pane those spray `[<32;56;15M` into the shell — exactly what a drag
//! flood produces. This filter watches the key stream and swallows such
//! fragments.
//!
//! False-positive risk is negligible: a human typing `Alt+[` immediately
//! followed by `<` then digits/`;` ending in `M`/`m` is not a real input
//! pattern, and an unfinished match replays nothing the pane needed (the
//! sequence was terminal chatter, not typing).

use termwiz::input::{KeyCode, Modifiers};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum State {
    /// Passing keys through.
    #[default]
    Idle,
    /// Saw `Alt+[` (or `ESC` then `[`): the CSI introducer of a fragment.
    SawCsi,
    /// Inside `< digits ; digits ; digits` — swallowing until `M`/`m`.
    InMouse,
}

/// Stateful filter; feed every key event destined for dispatch. `swallow`
/// returns `true` when the key is mouse residue and must be dropped.
#[derive(Debug, Default)]
pub struct MouseResidueFilter {
    state: State,
}

impl MouseResidueFilter {
    pub fn swallow(&mut self, key: &KeyCode, mods: Modifiers) -> bool {
        match (self.state, key) {
            // `ESC [` arrives as Alt+[ once termwiz merges the pair.
            (State::Idle, KeyCode::Char('[')) if mods == Modifiers::ALT => {
                self.state = State::SawCsi;
                true
            }
            (State::SawCsi, KeyCode::Char('<')) if mods.is_empty() => {
                self.state = State::InMouse;
                true
            }
            // Anything else after the introducer wasn't a mouse report after
            // all; resume passing (the swallowed `Alt+[` was still residue —
            // real Alt+[ chords are not bound and panes rarely want them).
            (State::SawCsi, _) => {
                self.state = State::Idle;
                false
            }
            (State::InMouse, KeyCode::Char(c))
                if mods.is_empty() && (c.is_ascii_digit() || *c == ';') =>
            {
                true
            }
            (State::InMouse, KeyCode::Char('M' | 'm')) if mods.is_empty() => {
                self.state = State::Idle;
                true
            }
            // Malformed fragment: stop swallowing.
            (State::InMouse, _) => {
                self.state = State::Idle;
                false
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
