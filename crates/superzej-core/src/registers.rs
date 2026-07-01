//! Vim-style named registers (a generalization of the clipboard), persisted
//! across restarts (Phase 3 of time-travel-replay).
//!
//! A register is named by a single char: `"a`–`"z` and `"0`–`"9` are ordinary
//! stores, `"` (the default register) is used when a yank/paste gives no name,
//! and `"+` is the **system clipboard** — special-cased at the host edge (a yank
//! to `+` also copies to the OS clipboard, a paste from `+` reads it), so it is
//! *not* persisted here. This module is pure and testable; the DB round-trip and
//! the `+` clipboard bridge live in the host.

use std::collections::BTreeMap;

/// The default register, used when no explicit name is given.
pub const DEFAULT: char = '"';
/// The system-clipboard register — volatile, never persisted.
pub const CLIPBOARD: char = '+';

/// A valid register name: a letter, digit, the default `"`, or clipboard `+`.
pub fn is_valid(name: char) -> bool {
    name == DEFAULT || name == CLIPBOARD || name.is_ascii_alphanumeric()
}

/// Whether a register is persisted to the DB (everything except the volatile
/// system-clipboard register).
pub fn is_persistent(name: char) -> bool {
    is_valid(name) && name != CLIPBOARD
}

/// The in-memory register store.
#[derive(Debug, Clone, Default)]
pub struct Registers {
    map: BTreeMap<char, String>,
}

impl Registers {
    pub fn new() -> Self {
        Self::default()
    }

    /// Build from persisted `(name, value)` pairs (e.g. `Db::all_registers`),
    /// ignoring any invalid names.
    pub fn from_pairs(pairs: impl IntoIterator<Item = (char, String)>) -> Self {
        let mut r = Self::new();
        for (name, value) in pairs {
            if is_valid(name) {
                r.map.insert(name, value);
            }
        }
        r
    }

    /// Store `text` in register `name`. An invalid name is dropped (returns
    /// `false`); a yank always also updates the default register `"`, matching
    /// vim, so an unnamed paste retrieves the most recent yank.
    pub fn yank(&mut self, name: char, text: String) -> bool {
        if !is_valid(name) {
            return false;
        }
        if name != DEFAULT {
            self.map.insert(DEFAULT, text.clone());
        }
        self.map.insert(name, text);
        true
    }

    /// The value in register `name`, if any.
    pub fn get(&self, name: char) -> Option<&str> {
        self.map.get(&name).map(String::as_str)
    }

    /// Iterate the persisted registers (excludes the volatile clipboard) as
    /// `(name, value)` — what the host writes back to the DB.
    pub fn persistent(&self) -> impl Iterator<Item = (char, &str)> {
        self.map
            .iter()
            .filter(|(k, _)| is_persistent(**k))
            .map(|(k, v)| (*k, v.as_str()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yank_and_get_roundtrip() {
        let mut r = Registers::new();
        assert!(r.yank('a', "hello".into()));
        assert_eq!(r.get('a'), Some("hello"));
    }

    #[test]
    fn yank_updates_default_register_too() {
        let mut r = Registers::new();
        r.yank('a', "payload".into());
        // Unnamed paste reads the default register, which mirrors the last yank.
        assert_eq!(r.get(DEFAULT), Some("payload"));
    }

    #[test]
    fn yank_to_default_does_not_recurse() {
        let mut r = Registers::new();
        r.yank(DEFAULT, "x".into());
        assert_eq!(r.get(DEFAULT), Some("x"));
    }

    #[test]
    fn overwrite_replaces_value() {
        let mut r = Registers::new();
        r.yank('a', "one".into());
        r.yank('a', "two".into());
        assert_eq!(r.get('a'), Some("two"));
    }

    #[test]
    fn get_missing_is_none() {
        let r = Registers::new();
        assert_eq!(r.get('z'), None);
    }

    #[test]
    fn invalid_name_is_rejected() {
        let mut r = Registers::new();
        assert!(!r.yank('!', "nope".into()));
        assert_eq!(r.get('!'), None);
        assert!(!is_valid('!'));
        assert!(!is_valid(' '));
    }

    #[test]
    fn clipboard_is_valid_but_not_persistent() {
        assert!(is_valid(CLIPBOARD));
        assert!(!is_persistent(CLIPBOARD));
        assert!(is_persistent('a'));
        assert!(is_persistent(DEFAULT));
    }

    #[test]
    fn from_pairs_filters_invalid_and_persistent_iterates() {
        let r = Registers::from_pairs([
            ('a', "A".to_string()),
            ('+', "clip".to_string()), // valid but volatile
            ('!', "bad".to_string()),  // invalid, dropped
        ]);
        assert_eq!(r.get('a'), Some("A"));
        assert_eq!(r.get('+'), Some("clip"));
        assert_eq!(r.get('!'), None);
        // Only 'a' is persistent ('+' excluded).
        let persisted: Vec<_> = r.persistent().collect();
        assert_eq!(persisted, vec![('a', "A")]);
    }

    #[test]
    fn digit_and_letter_registers_valid() {
        for c in ['a', 'z', '0', '9', 'Q'] {
            assert!(is_valid(c), "{c} should be a valid register");
        }
    }
}
