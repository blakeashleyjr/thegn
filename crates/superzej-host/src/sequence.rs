//! Stateful key-sequence matching for the native host.
//!
//! A `Key` is the normalized termwiz key event the host sees. `SequenceMatcher`
//! stores ordered key sequences (single chords or multi-key chains such as
//! `g g` / `Ctrl x Ctrl c`) and reports whether input matched, is a valid prefix,
//! or should fall through to the focused pane.

use termwiz::input::{KeyCode, Modifiers};

use crate::keymap::Action;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Key {
    pub code: KeyCode,
    pub mods: Modifiers,
}

impl Key {
    #[allow(dead_code)]
    pub fn from_code(code: KeyCode) -> Self {
        Self::modified(code, Modifiers::NONE)
    }

    #[allow(dead_code)]
    pub fn char(c: char) -> Self {
        Self::from_code(KeyCode::Char(c))
    }

    #[allow(dead_code)]
    pub fn ctrl(c: char) -> Self {
        Self::modified(KeyCode::Char(c), Modifiers::CTRL)
    }

    pub fn modified(code: KeyCode, mods: Modifiers) -> Self {
        // termwiz reports shifted ASCII letters as uppercase chars; normalize
        // parser-created `Shift x` to the same shape so configured chords match.
        if let KeyCode::Char(c) = code {
            if mods.contains(Modifiers::SHIFT) && c.is_ascii_alphabetic() {
                return Self {
                    code: KeyCode::Char(c.to_ascii_uppercase()),
                    mods: mods - Modifiers::SHIFT,
                };
            }
        }
        Self { code, mods }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MatchResult {
    None,
    Pending,
    Matched(Action),
}

#[derive(Debug, Clone, Default)]
pub struct SequenceMatcher {
    sequences: Vec<(Vec<Key>, Action)>,
    current: Vec<Key>,
}

impl SequenceMatcher {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_sequence(&mut self, sequence: Vec<Key>, action: Action) {
        if sequence.is_empty() {
            return;
        }
        self.sequences.push((sequence, action));
    }

    pub fn remove_action(&mut self, action: &Action) {
        self.sequences.retain(|(_, a)| a != action);
        self.current.clear();
    }

    pub fn reset(&mut self) {
        self.current.clear();
    }

    pub fn feed(&mut self, key: Key) -> MatchResult {
        self.current.push(key);

        let mut pending = false;
        for (sequence, action) in &self.sequences {
            if sequence.starts_with(&self.current) {
                if sequence.len() == self.current.len() {
                    let action = action.clone();
                    self.current.clear();
                    return MatchResult::Matched(action);
                }
                pending = true;
            }
        }

        if pending {
            MatchResult::Pending
        } else {
            self.current.clear();
            MatchResult::None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::Action;

    #[test]
    fn sequence_matching_advances_state() {
        let mut matcher = SequenceMatcher::new();
        matcher.add_sequence(vec![Key::char('g'), Key::char('g')], Action::ScrollUp);

        assert_eq!(matcher.feed(Key::char('g')), MatchResult::Pending);
        assert_eq!(
            matcher.feed(Key::char('g')),
            MatchResult::Matched(Action::ScrollUp)
        );
    }

    #[test]
    fn failed_sequence_resets() {
        let mut matcher = SequenceMatcher::new();
        matcher.add_sequence(vec![Key::char('g'), Key::char('g')], Action::ScrollUp);

        assert_eq!(matcher.feed(Key::char('g')), MatchResult::Pending);
        assert_eq!(matcher.feed(Key::char('x')), MatchResult::None);
        assert_eq!(matcher.feed(Key::char('g')), MatchResult::Pending);
    }

    #[test]
    fn shifted_ascii_letters_normalize_to_uppercase_char() {
        assert_eq!(
            Key::modified(KeyCode::Char('x'), Modifiers::SHIFT | Modifiers::ALT),
            Key::modified(KeyCode::Char('X'), Modifiers::ALT)
        );
    }
}
