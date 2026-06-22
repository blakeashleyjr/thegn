//! Backend-agnostic input. A tile never sees crossterm or termwiz types: each
//! host maps its native events into these. superzej translates termwiz in
//! `apps/input.rs`; the standalone harness translates crossterm via
//! [`from_crossterm`] / [`from_crossterm_event`] (feature `standalone`).
//!
//! These types are the canonical definitions — embedded app crates re-export
//! them so each app library and the host agree on one input vocabulary.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Modifiers {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

impl Modifiers {
    pub const NONE: Modifiers = Modifiers {
        ctrl: false,
        alt: false,
        shift: false,
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Enter,
    Escape,
    Backspace,
    Delete,
    Tab,
    BackTab,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Function(u8),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputEvent {
    Key {
        key: Key,
        modifiers: Modifiers,
    },
    Paste(String),
    Resize(u16, u16),
    /// Host heartbeat — no input, just an opportunity to pump.
    Tick,
}

impl InputEvent {
    pub fn key(key: Key) -> Self {
        Self::Key {
            key,
            modifiers: Modifiers::NONE,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputResult {
    /// The UI handled it (likely needs a redraw).
    Consumed,
    /// Not ours — the host may route it elsewhere.
    Ignored,
    /// The user asked to leave (standalone: quit; embedded: host decides).
    Exit,
}

/// Map a crossterm key event; `None` for keys this vocabulary has no concept
/// of. Uses ratatui's bundled crossterm so the version always matches.
#[cfg(feature = "standalone")]
pub fn from_crossterm(ev: &ratatui::crossterm::event::KeyEvent) -> Option<InputEvent> {
    use ratatui::crossterm::event::{KeyCode, KeyEventKind, KeyModifiers};
    // crossterm emits Press/Repeat/Release on terminals with the kitty
    // protocol; tiles only care about presses (and repeats).
    if ev.kind == KeyEventKind::Release {
        return None;
    }
    let modifiers = Modifiers {
        ctrl: ev.modifiers.contains(KeyModifiers::CONTROL),
        alt: ev.modifiers.contains(KeyModifiers::ALT),
        shift: ev.modifiers.contains(KeyModifiers::SHIFT),
    };
    let key = match ev.code {
        KeyCode::Char(c) => Key::Char(c),
        KeyCode::Enter => Key::Enter,
        KeyCode::Esc => Key::Escape,
        KeyCode::Backspace => Key::Backspace,
        KeyCode::Delete => Key::Delete,
        KeyCode::Tab => Key::Tab,
        KeyCode::BackTab => Key::BackTab,
        KeyCode::Up => Key::Up,
        KeyCode::Down => Key::Down,
        KeyCode::Left => Key::Left,
        KeyCode::Right => Key::Right,
        KeyCode::Home => Key::Home,
        KeyCode::End => Key::End,
        KeyCode::PageUp => Key::PageUp,
        KeyCode::PageDown => Key::PageDown,
        KeyCode::F(n) => Key::Function(n),
        _ => return None,
    };
    Some(InputEvent::Key { key, modifiers })
}

/// Map a full crossterm event (key/paste/resize) into an [`InputEvent`].
/// `None` for events with no tile-level meaning (mouse, focus, raw key kinds).
#[cfg(feature = "standalone")]
pub fn from_crossterm_event(ev: ratatui::crossterm::event::Event) -> Option<InputEvent> {
    use ratatui::crossterm::event::Event;
    match ev {
        Event::Key(k) => from_crossterm(&k),
        Event::Paste(s) => Some(InputEvent::Paste(s)),
        Event::Resize(cols, rows) => Some(InputEvent::Resize(cols, rows)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_helper_defaults_to_no_mods() {
        assert_eq!(
            InputEvent::key(Key::Enter),
            InputEvent::Key {
                key: Key::Enter,
                modifiers: Modifiers::NONE,
            }
        );
    }
}
