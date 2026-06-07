//! The host keymap: terminal key chords → host `Action`s. The native host owns
//! input routing (zellij did before), so global chords are intercepted here and
//! everything else is forwarded to the focused pane. Mirrors the bindings in
//! `config/zellij.kdl` (Alt-w new worktree, Alt-o switch, Ctrl-Alt-s/p toggles,
//! splits, focus moves, …). Pure + unit-tested; the loop calls `map_key`.

use termwiz::input::{KeyCode, Modifiers};

/// A host-level action, decoupled from any key. The command palette dispatches
/// the same set (by `key()`), so keymap and palette share one action vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    NewWorktree,
    NewWorkspace,
    NewTab,
    CloseWorktree,
    SwitchWorkspace,
    Dashboard,
    NextTab,
    PrevTab,
    SplitDown,
    SplitRight,
    FocusLeft,
    FocusRight,
    FocusUp,
    FocusDown,
    ToggleSidebar,
    TogglePanel,
    ToggleDrawer,
    OpenPalette,
    Lazygit,
    Yazi,
    Editor,
    Diff,
    ScrollUp,
    ScrollDown,
    CopyPane,
    Quit,
}

impl Action {
    /// Stable key (matches the palette item keys, so a palette selection and a
    /// keybinding resolve to the same dispatch). Exercised by tests; the unified
    /// dispatch table consumes it as palette actions are wired.
    #[allow(dead_code)]
    pub fn key(self) -> &'static str {
        match self {
            Action::NewWorktree => "new-worktree",
            Action::NewWorkspace => "new-workspace",
            Action::NewTab => "new-tab",
            Action::CloseWorktree => "close-worktree",
            Action::SwitchWorkspace => "switch-workspace",
            Action::Dashboard => "dashboard",
            Action::NextTab => "next-tab",
            Action::PrevTab => "prev-tab",
            Action::SplitDown => "split-down",
            Action::SplitRight => "split-right",
            Action::FocusLeft => "focus-left",
            Action::FocusRight => "focus-right",
            Action::FocusUp => "focus-up",
            Action::FocusDown => "focus-down",
            Action::ToggleSidebar => "toggle-sidebar",
            Action::TogglePanel => "toggle-panel",
            Action::ToggleDrawer => "files-drawer",
            Action::OpenPalette => "palette",
            Action::Lazygit => "lazygit",
            Action::Yazi => "yazi",
            Action::Editor => "editor",
            Action::Diff => "show-diff",
            Action::ScrollUp => "scroll-up",
            Action::ScrollDown => "scroll-down",
            Action::CopyPane => "copy-pane",
            Action::Quit => "quit",
        }
    }
}

/// Map a key chord to a host action, or `None` to forward the key to the pane.
pub fn map_key(key: &KeyCode, mods: Modifiers) -> Option<Action> {
    let alt = mods.contains(Modifiers::ALT);
    let ctrl = mods.contains(Modifiers::CTRL);
    let ctrl_alt = ctrl && alt;

    match key {
        // Ctrl chords.
        KeyCode::Char('q') | KeyCode::Char('Q') if ctrl && !alt => Some(Action::Quit),
        KeyCode::Char('k') | KeyCode::Char('K') if ctrl && !alt => Some(Action::OpenPalette),
        // Ctrl-Alt chrome/drawer toggles.
        KeyCode::Char('s') | KeyCode::Char('S') if ctrl_alt => Some(Action::ToggleSidebar),
        KeyCode::Char('p') | KeyCode::Char('P') if ctrl_alt => Some(Action::TogglePanel),
        KeyCode::Char('f') | KeyCode::Char('F') if ctrl_alt => Some(Action::ToggleDrawer),
        KeyCode::Char('c') | KeyCode::Char('C') if ctrl_alt => Some(Action::CopyPane),
        // Alt chords (worktree/workspace/tab lifecycle + tools). Uppercase = shift.
        KeyCode::Char('w') if alt && !ctrl => Some(Action::NewWorktree),
        KeyCode::Char('W') if alt && !ctrl => Some(Action::NewWorkspace),
        KeyCode::Char('t') if alt && !ctrl => Some(Action::NewTab),
        KeyCode::Char('X') if alt && !ctrl => Some(Action::CloseWorktree),
        KeyCode::Char('o') if alt && !ctrl => Some(Action::SwitchWorkspace),
        KeyCode::Char('d') if alt && !ctrl => Some(Action::Dashboard),
        KeyCode::Char('n') if alt && !ctrl => Some(Action::SplitDown),
        KeyCode::Char('N') if alt && !ctrl => Some(Action::SplitRight),
        KeyCode::Char('g') if alt && !ctrl => Some(Action::Lazygit),
        KeyCode::Char('y') if alt && !ctrl => Some(Action::Yazi),
        KeyCode::Char('e') if alt && !ctrl => Some(Action::Editor),
        KeyCode::Char('/') if alt && !ctrl => Some(Action::Diff),
        // Alt focus moves (vim hjkl) + tab nav (arrows).
        KeyCode::Char('h') if alt && !ctrl => Some(Action::FocusLeft),
        KeyCode::Char('j') if alt && !ctrl => Some(Action::FocusDown),
        KeyCode::Char('k') if alt && !ctrl => Some(Action::FocusUp),
        KeyCode::Char('l') if alt && !ctrl => Some(Action::FocusRight),
        KeyCode::LeftArrow if alt => Some(Action::PrevTab),
        KeyCode::RightArrow if alt => Some(Action::NextTab),
        // Scrollback (terminal convention: Shift-PageUp/Down).
        KeyCode::PageUp if mods.contains(Modifiers::SHIFT) => Some(Action::ScrollUp),
        KeyCode::PageDown if mods.contains(Modifiers::SHIFT) => Some(Action::ScrollDown),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(c: char, m: Modifiers) -> Option<Action> {
        map_key(&KeyCode::Char(c), m)
    }

    #[test]
    fn alt_chords_map_to_lifecycle_actions() {
        assert_eq!(k('w', Modifiers::ALT), Some(Action::NewWorktree));
        assert_eq!(k('W', Modifiers::ALT), Some(Action::NewWorkspace));
        assert_eq!(k('o', Modifiers::ALT), Some(Action::SwitchWorkspace));
        assert_eq!(k('t', Modifiers::ALT), Some(Action::NewTab));
    }

    #[test]
    fn ctrl_and_ctrl_alt_distinguished() {
        assert_eq!(k('k', Modifiers::CTRL), Some(Action::OpenPalette));
        assert_eq!(k('q', Modifiers::CTRL), Some(Action::Quit));
        assert_eq!(
            k('s', Modifiers::CTRL | Modifiers::ALT),
            Some(Action::ToggleSidebar)
        );
        assert_eq!(
            k('p', Modifiers::CTRL | Modifiers::ALT),
            Some(Action::TogglePanel)
        );
        assert_eq!(
            k('f', Modifiers::CTRL | Modifiers::ALT),
            Some(Action::ToggleDrawer)
        );
        // Plain Ctrl-S is NOT a sidebar toggle (that's Ctrl-Alt-S).
        assert_eq!(k('s', Modifiers::CTRL), None);
    }

    #[test]
    fn focus_and_tab_nav() {
        assert_eq!(k('h', Modifiers::ALT), Some(Action::FocusLeft));
        assert_eq!(k('l', Modifiers::ALT), Some(Action::FocusRight));
        assert_eq!(
            map_key(&KeyCode::LeftArrow, Modifiers::ALT),
            Some(Action::PrevTab)
        );
        assert_eq!(
            map_key(&KeyCode::RightArrow, Modifiers::ALT),
            Some(Action::NextTab)
        );
    }

    #[test]
    fn shift_pageup_down_scroll_but_plain_pageup_forwards() {
        assert_eq!(
            map_key(&KeyCode::PageUp, Modifiers::SHIFT),
            Some(Action::ScrollUp)
        );
        assert_eq!(
            map_key(&KeyCode::PageDown, Modifiers::SHIFT),
            Some(Action::ScrollDown)
        );
        // Plain PageUp is forwarded to the pane (apps use it).
        assert_eq!(map_key(&KeyCode::PageUp, Modifiers::NONE), None);
    }

    #[test]
    fn unmodified_keys_forward_to_pane() {
        assert_eq!(k('w', Modifiers::NONE), None);
        assert_eq!(k('a', Modifiers::NONE), None);
        assert_eq!(map_key(&KeyCode::Enter, Modifiers::NONE), None);
    }

    #[test]
    fn action_keys_match_palette_item_keys() {
        // The palette and keymap must agree on dispatch keys.
        assert_eq!(Action::NewWorktree.key(), "new-worktree");
        assert_eq!(Action::Quit.key(), "quit");
        assert_eq!(Action::ToggleDrawer.key(), "files-drawer");
    }
}
