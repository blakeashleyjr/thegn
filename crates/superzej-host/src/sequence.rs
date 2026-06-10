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

    /// The next-key candidates given the current pending prefix: for every stored
    /// sequence that begins with `self.current` and is longer, the key that would
    /// advance it plus the action it ultimately resolves to. Drives the which-key
    /// popup. Empty when nothing is pending. Deduplicated on the next key.
    pub fn pending_continuations(&self) -> Vec<(Key, Action)> {
        // Only meaningful mid-sequence: with no prefix this would list every
        // first key, which is not what the which-key popup wants.
        if self.current.is_empty() {
            return Vec::new();
        }
        let mut out: Vec<(Key, Action)> = Vec::new();
        for (sequence, action) in &self.sequences {
            if sequence.len() > self.current.len() && sequence.starts_with(&self.current) {
                let next = sequence[self.current.len()].clone();
                if !out.iter().any(|(k, _)| *k == next) {
                    out.push((next, action.clone()));
                }
            }
        }
        out
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
    fn pending_continuations_list_next_keys() {
        let mut matcher = SequenceMatcher::new();
        matcher.add_sequence(vec![Key::char(' '), Key::char('p')], Action::TogglePanel);
        matcher.add_sequence(vec![Key::char(' '), Key::char('s')], Action::ToggleSidebar);
        matcher.add_sequence(vec![Key::char('j')], Action::FocusDown);

        // Nothing pending yet → no continuations.
        assert!(matcher.pending_continuations().is_empty());
        // Feed the Space prefix; both Space-sequences are now candidates.
        assert_eq!(matcher.feed(Key::char(' ')), MatchResult::Pending);
        let cont = matcher.pending_continuations();
        assert_eq!(cont.len(), 2);
        assert!(
            cont.iter()
                .any(|(k, a)| *k == Key::char('p') && *a == Action::TogglePanel)
        );
        assert!(
            cont.iter()
                .any(|(k, a)| *k == Key::char('s') && *a == Action::ToggleSidebar)
        );
    }

    #[test]
    fn shifted_ascii_letters_normalize_to_uppercase_char() {
        assert_eq!(
            Key::modified(KeyCode::Char('x'), Modifiers::SHIFT | Modifiers::ALT),
            Key::modified(KeyCode::Char('X'), Modifiers::ALT)
        );
    }
}
