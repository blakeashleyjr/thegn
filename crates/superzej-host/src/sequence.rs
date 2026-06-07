<<<<<<< Updated upstream
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

    pub fn remove_action(&mut self, action: Action) {
        self.sequences.retain(|(_, a)| *a != action);
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
                    let action = *action;
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
||||||| Stash base
=======
//! Modal key sequence matching for the native host.
//!
//! A [`Key`] is a normalized termwiz key event. A [`SequenceMatcher`] stores
//! single-key chords and multi-key sequences (for vim-style bindings such as
//! `g g` or `Leader p`) and advances a tiny pending-state machine as input
//! arrives.

use termwiz::input::{KeyCode, Modifiers};

use crate::keymap::Action;

/// One normalized terminal key event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Key {
    pub code: KeyCode,
    pub mods: Modifiers,
}

impl Key {
    pub fn new(code: KeyCode, mods: Modifiers) -> Self {
        let mut mods = normalize_modifiers(mods);
        let code = match code {
            // Ctrl-letter terminals vary between upper/lower chars. Canonicalize
            // them so `Ctrl q` matches `Ctrl-Q` too, while Alt-W can still be a
            // distinct shifted binding.
            KeyCode::Char(c) if mods.contains(Modifiers::CTRL) && c.is_ascii_alphabetic() => {
                mods.remove(Modifiers::SHIFT);
                KeyCode::Char(c.to_ascii_lowercase())
            }
            KeyCode::Char(c) if c.is_ascii_uppercase() && mods.contains(Modifiers::SHIFT) => {
                mods.remove(Modifiers::SHIFT);
                KeyCode::Char(c)
            }
            other => other,
        };
        Key { code, mods }
    }

    pub fn char(c: char) -> Self {
        Key::new(KeyCode::Char(c), Modifiers::NONE)
    }

    /// Parse a zellij/superzej-style binding string into one or more keys.
    ///
    /// Modifier tokens apply to the next key token, so `Ctrl Alt s` is one chord
    /// while `g g` is a two-key sequence. `Leader` is treated as a space key,
    /// giving users a portable leader/prefix spelling without depending on a
    /// terminal-specific virtual modifier.
    pub fn parse_sequence(s: &str) -> Result<Vec<Key>, String> {
        let mut out = Vec::new();
        let mut mods = Modifiers::NONE;
        for raw in s.split_whitespace() {
            if let Some(m) = modifier(raw) {
                mods |= m;
                continue;
            }
            let code = key_code(raw)?;
            out.push(Key::new(code, mods));
            mods = Modifiers::NONE;
        }
        if !mods.is_empty() {
            return Err(format!("dangling modifier(s) at end of {s:?}"));
        }
        if out.is_empty() {
            return Err("empty key sequence".into());
        }
        Ok(out)
    }
}

fn normalize_modifiers(mut mods: Modifiers) -> Modifiers {
    if mods.contains(Modifiers::LEFT_ALT) || mods.contains(Modifiers::RIGHT_ALT) {
        mods |= Modifiers::ALT;
        mods.remove(Modifiers::LEFT_ALT | Modifiers::RIGHT_ALT);
    }
    if mods.contains(Modifiers::LEFT_CTRL) || mods.contains(Modifiers::RIGHT_CTRL) {
        mods |= Modifiers::CTRL;
        mods.remove(Modifiers::LEFT_CTRL | Modifiers::RIGHT_CTRL);
    }
    if mods.contains(Modifiers::LEFT_SHIFT) || mods.contains(Modifiers::RIGHT_SHIFT) {
        mods |= Modifiers::SHIFT;
        mods.remove(Modifiers::LEFT_SHIFT | Modifiers::RIGHT_SHIFT);
    }
    mods.remove(Modifiers::ENHANCED_KEY);
    mods
}

fn modifier(raw: &str) -> Option<Modifiers> {
    match raw.to_ascii_lowercase().as_str() {
        "ctrl" | "control" => Some(Modifiers::CTRL),
        "alt" | "opt" | "option" | "meta" => Some(Modifiers::ALT),
        "super" | "cmd" | "mod" | "win" => Some(Modifiers::SUPER),
        "shift" => Some(Modifiers::SHIFT),
        _ => None,
    }
}

fn key_code(raw: &str) -> Result<KeyCode, String> {
    let lower = raw.to_ascii_lowercase();
    Ok(match lower.as_str() {
        "leader" => KeyCode::Char(' '),
        "space" => KeyCode::Char(' '),
        "esc" | "escape" => KeyCode::Escape,
        "enter" | "return" => KeyCode::Enter,
        "backspace" | "bs" => KeyCode::Backspace,
        "tab" => KeyCode::Tab,
        "delete" | "del" => KeyCode::Delete,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "left" | "leftarrow" => KeyCode::LeftArrow,
        "right" | "rightarrow" => KeyCode::RightArrow,
        "up" | "uparrow" => KeyCode::UpArrow,
        "down" | "downarrow" => KeyCode::DownArrow,
        "pageup" | "page-up" | "pgup" => KeyCode::PageUp,
        "pagedown" | "page-down" | "pgdn" => KeyCode::PageDown,
        _ => {
            let mut chars = raw.chars();
            let Some(c) = chars.next() else {
                return Err("missing key".into());
            };
            if chars.next().is_some() {
                return Err(format!("unknown key token {raw:?}"));
            }
            KeyCode::Char(c)
        }
    })
}

/// The result of feeding one key into a sequence matcher.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MatchResult {
    None,
    Pending,
    Matched(Action),
}

/// Stateful matcher for a mode's bindings.
#[derive(Debug, Clone, Default)]
pub struct SequenceMatcher {
    sequences: Vec<(Vec<Key>, Action)>,
    current: Vec<Key>,
}

impl SequenceMatcher {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_sequence(&mut self, seq: Vec<Key>, action: Action) {
        if !seq.is_empty() {
            self.sequences.push((seq, action));
        }
    }

    pub fn remove_action_key(&mut self, key: &str) {
        self.sequences.retain(|(_, action)| action.key() != key);
        self.current.clear();
    }

    pub fn reset(&mut self) {
        self.current.clear();
    }

    pub fn feed(&mut self, key: Key) -> MatchResult {
        self.current.push(key);
        self.match_current(true)
    }

    fn match_current(&mut self, retry_last_key: bool) -> MatchResult {
        let mut exact = None;
        let mut longer_prefix = false;

        for (seq, action) in &self.sequences {
            if seq.starts_with(&self.current) {
                if seq.len() == self.current.len() {
                    exact = Some(action.clone());
                } else {
                    longer_prefix = true;
                }
            }
        }

        if let Some(action) = exact {
            if longer_prefix {
                return MatchResult::Pending;
            }
            self.current.clear();
            return MatchResult::Matched(action);
        }
        if longer_prefix {
            return MatchResult::Pending;
        }

        // If a pending prefix fails, let the final key start a fresh match so a
        // sequence typo doesn't swallow a subsequent single-chord command.
        if retry_last_key && self.current.len() > 1 {
            let last = self.current.last().cloned().expect("current has a key");
            self.current.clear();
            self.current.push(last);
            return self.match_current(false);
        }

        self.current.clear();
        MatchResult::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::Mode;

    #[test]
    fn parse_single_chord_with_modifiers() {
        assert_eq!(
            Key::parse_sequence("Ctrl Alt s").unwrap(),
            vec![Key::new(KeyCode::Char('s'), Modifiers::CTRL | Modifiers::ALT)]
        );
        assert_eq!(
            Key::parse_sequence("Shift PageUp").unwrap(),
            vec![Key::new(KeyCode::PageUp, Modifiers::SHIFT)]
        );
    }

    #[test]
    fn parse_multi_key_sequence_and_leader() {
        assert_eq!(
            Key::parse_sequence("g g").unwrap(),
            vec![Key::char('g'), Key::char('g')]
        );
        assert_eq!(
            Key::parse_sequence("Leader p").unwrap(),
            vec![Key::char(' '), Key::char('p')]
        );
    }

    #[test]
    fn normalizes_ctrl_letters_and_side_modifiers() {
        assert_eq!(
            Key::new(KeyCode::Char('Q'), Modifiers::CTRL | Modifiers::SHIFT),
            Key::new(KeyCode::Char('q'), Modifiers::CTRL)
        );
        assert_eq!(
            Key::new(KeyCode::Char('s'), Modifiers::LEFT_CTRL | Modifiers::LEFT_ALT),
            Key::new(KeyCode::Char('s'), Modifiers::CTRL | Modifiers::ALT)
        );
    }

    #[test]
    fn matcher_resets_after_miss_and_retries_last_key() {
        let mut matcher = SequenceMatcher::new();
        matcher.add_sequence(vec![Key::char('g'), Key::char('g')], Action::ScrollUp);
        matcher.add_sequence(vec![Key::char('x')], Action::Quit);

        assert_eq!(matcher.feed(Key::char('g')), MatchResult::Pending);
        assert_eq!(
            matcher.feed(Key::char('x')),
            MatchResult::Matched(Action::Quit)
        );
        assert_eq!(matcher.feed(Key::char('g')), MatchResult::Pending);
    }

    #[test]
    fn matcher_reset_clears_pending_sequence() {
        let mut matcher = SequenceMatcher::new();
        matcher.add_sequence(vec![Key::char('g'), Key::char('g')], Action::ScrollUp);

        assert_eq!(matcher.feed(Key::char('g')), MatchResult::Pending);
        matcher.reset();
        assert_eq!(matcher.feed(Key::char('g')), MatchResult::Pending);
    }

    #[test]
    fn exact_prefix_waits_for_longer_sequence() {
        let mut matcher = SequenceMatcher::new();
        matcher.add_sequence(vec![Key::char('g')], Action::ScrollUp);
        matcher.add_sequence(
            vec![Key::char('g'), Key::char('g')],
            Action::SwitchMode(Mode::VimNormal),
        );

        assert_eq!(matcher.feed(Key::char('g')), MatchResult::Pending);
        assert_eq!(
            matcher.feed(Key::char('g')),
            MatchResult::Matched(Action::SwitchMode(Mode::VimNormal))
        );
    }
}
>>>>>>> Stashed changes
