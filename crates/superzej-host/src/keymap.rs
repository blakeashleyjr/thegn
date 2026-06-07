//! The host keymap: terminal key chords → host `Action`s. The native host owns
//! input routing (zellij did before), so global chords are intercepted here and
//! everything else is forwarded to the focused pane. Mirrors the bindings in
//! `config/zellij.kdl` (Alt-w new worktree, Alt-o switch, Ctrl-Alt-s/p toggles,
//! splits, focus moves, …). Pure + unit-tested; the loop calls `map_key`.

use termwiz::input::{KeyCode, Modifiers};

<<<<<<< Updated upstream
use crate::sequence::{Key, MatchResult, SequenceMatcher};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Mode {
    Normal,
    VimNormal,
    VimInsert,
    Emacs,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Normal => "Normal",
            Mode::VimNormal => "VimNormal",
            Mode::VimInsert => "VimInsert",
            Mode::Emacs => "Emacs",
        }
    }
}

||||||| Stash base
=======
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Mode {
    Normal,
    VimNormal,
    VimInsert,
    Emacs,
}

>>>>>>> Stashed changes
/// A host-level action, decoupled from any key. The command palette dispatches
/// the same set (by `key()`), so keymap and palette share one action vocabulary.
#[derive(Debug, Clone, PartialEq, Eq)]
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
    SwitchMode(Mode),
    ShellCommand {
        name: String,
        run: String,
        floating: bool,
        close_on_exit: bool,
    },
    Quit,
    Custom(u16),
    SwitchMode(Mode),
}

impl Action {
    /// Stable key (matches the palette item keys, so a palette selection and a
    /// keybinding resolve to the same dispatch). Exercised by tests; the unified
    /// dispatch table consumes it as palette actions are wired.
    #[allow(dead_code)]
    pub fn key(&self) -> &str {
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
            Action::SwitchMode(Mode::Normal) => "mode-normal",
            Action::SwitchMode(Mode::VimNormal) => "mode-vim-normal",
            Action::SwitchMode(Mode::VimInsert) => "mode-vim-insert",
            Action::SwitchMode(Mode::Emacs) => "mode-emacs",
            Action::ShellCommand { name, .. } => name.as_str(),
            Action::Quit => "quit",
            Action::Custom(_) => "custom-action",
            Action::SwitchMode(Mode::Normal) => "mode-normal",
            Action::SwitchMode(Mode::VimNormal) => "mode-vim-normal",
            Action::SwitchMode(Mode::VimInsert) => "mode-vim-insert",
            Action::SwitchMode(Mode::Emacs) => "mode-emacs",
        }
    }

    pub fn from_key(key: &str) -> Option<Action> {
        Some(match key {
            "new-worktree" => Action::NewWorktree,
            "new-workspace" => Action::NewWorkspace,
            "new-tab" => Action::NewTab,
            "close-worktree" => Action::CloseWorktree,
            "switch-workspace" | "switch-repo" => Action::SwitchWorkspace,
            "dashboard" => Action::Dashboard,
            "next-tab" => Action::NextTab,
            "prev-tab" => Action::PrevTab,
            "split-down" | "new-panel-native" => Action::SplitDown,
            "split-right" | "new-panel" => Action::SplitRight,
            "focus-left" => Action::FocusLeft,
            "focus-right" => Action::FocusRight,
            "focus-up" => Action::FocusUp,
            "focus-down" => Action::FocusDown,
            "toggle-sidebar" => Action::ToggleSidebar,
            "toggle-panel" => Action::TogglePanel,
            "files" | "files-drawer" | "toggle-drawer" => Action::ToggleDrawer,
            "palette" | "menu" => Action::OpenPalette,
            "lazygit" | "tool-lazygit" => Action::Lazygit,
            "yazi" | "tool-yazi" => Action::Yazi,
            "editor" | "tool-editor" => Action::Editor,
            "show-diff" | "diff" | "tool-diff" => Action::Diff,
            "scroll-up" => Action::ScrollUp,
            "scroll-down" => Action::ScrollDown,
            "copy-pane" => Action::CopyPane,
            "quit" => Action::Quit,
            "mode-normal" => Action::SwitchMode(Mode::Normal),
            "mode-vim-normal" => Action::SwitchMode(Mode::VimNormal),
            "mode-vim-insert" => Action::SwitchMode(Mode::VimInsert),
            "mode-emacs" => Action::SwitchMode(Mode::Emacs),
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostCustomAction {
    pub name: String,
    pub run: String,
    pub floating: bool,
    pub close_on_exit: bool,
}

pub struct KeyMap {
    modes: std::collections::HashMap<Mode, SequenceMatcher>,
    custom_actions: Vec<HostCustomAction>,
    config: superzej_core::config::Config,
}

impl KeyMap {
    pub fn new() -> Self {
        Self::with_config(superzej_core::config::Config::default())
    }

    pub fn with_config(config: superzej_core::config::Config) -> Self {
        Self {
            modes: std::collections::HashMap::new(),
            custom_actions: Vec::new(),
            config,
        }
    }

    pub fn config(&self) -> &superzej_core::config::Config {
        &self.config
    }

    pub fn insert(&mut self, mode: Mode, chord: &str, action: Action) -> Result<(), String> {
        let keys = parse_chord(chord)?;
        self.insert_keys(mode, keys, action);
        Ok(())
    }

    #[allow(clippy::unwrap_or_default)]
    pub fn insert_keys(&mut self, mode: Mode, keys: Vec<Key>, action: Action) {
        self.modes
            .entry(mode)
            .or_insert_with(SequenceMatcher::new)
            .add_sequence(keys, action);
    }

    pub fn insert_all(&mut self, chord: &str, action: Action) -> Result<(), String> {
        let keys = parse_chord(chord)?;
        for mode in ALL_MODES {
            self.insert_keys(mode, keys.clone(), action);
        }
        Ok(())
    }

    pub fn remove(&mut self, mode: Mode, action: Action) {
        if let Some(m) = self.modes.get_mut(&mode) {
            m.remove_action(action);
        }
    }

    pub fn remove_all(&mut self, action: Action) {
        for mode in ALL_MODES {
            self.remove(mode, action);
        }
    }

    pub fn dispatch(&mut self, mode: Mode, key: Key) -> MatchResult {
        if let Some(matcher) = self.modes.get_mut(&mode) {
            matcher.feed(key)
        } else {
            MatchResult::None
        }
    }

    pub fn reset(&mut self) {
        for matcher in self.modes.values_mut() {
            matcher.reset();
        }
    }

    pub fn custom_actions(&self) -> &[HostCustomAction] {
        &self.custom_actions
    }
}

const ALL_MODES: [Mode; 4] = [Mode::Normal, Mode::VimNormal, Mode::VimInsert, Mode::Emacs];

fn parse_chord(s: &str) -> Result<Vec<Key>, String> {
    let mut out = Vec::new();
    let normalized = s.replace('-', " ");
    let toks: Vec<&str> = normalized.split_whitespace().collect();
    if toks.is_empty() {
        return Err("empty chord".into());
    }

    let mut mods = Modifiers::NONE;
    for tok in toks {
        match tok.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => mods |= Modifiers::CTRL,
            "alt" | "opt" | "option" => mods |= Modifiers::ALT,
            "super" | "cmd" | "mod" | "win" => mods |= Modifiers::SUPER,
            "shift" => mods |= Modifiers::SHIFT,
            "leader" | "space" => {
                out.push(Key::modified(KeyCode::Char(' '), mods));
                mods = Modifiers::NONE;
            }
            other => {
                let code = if tok.chars().count() == 1 {
                    KeyCode::Char(tok.chars().next().unwrap())
                } else {
                    match other {
                        "enter" | "return" => KeyCode::Enter,
                        "escape" | "esc" => KeyCode::Escape,
                        "backspace" | "bs" => KeyCode::Backspace,
                        "tab" => KeyCode::Tab,
                        "left" | "leftarrow" => KeyCode::LeftArrow,
                        "right" | "rightarrow" => KeyCode::RightArrow,
                        "up" | "uparrow" => KeyCode::UpArrow,
                        "down" | "downarrow" => KeyCode::DownArrow,
                        "delete" | "del" => KeyCode::Delete,
                        "pageup" | "pgup" => KeyCode::PageUp,
                        "pagedown" | "pgdn" => KeyCode::PageDown,
                        "home" => KeyCode::Home,
                        "end" => KeyCode::End,
                        _ => return Err(format!("unknown key name {other:?}")),
                    }
                };
                out.push(Key::modified(code, mods));
                mods = Modifiers::NONE;
            }
        }
    }

    if !mods.is_empty() {
        return Err(format!("dangling modifier(s) in {s:?}"));
    }

    Ok(out)
}

pub fn default_keymap() -> KeyMap {
    let mut map = KeyMap::new();
    // Global defaults: work in every host mode, so Vim/Emacs modes never trap
    // users away from workspace/pane management.
    map.insert_all("Ctrl q", Action::Quit).unwrap();
    map.insert_all("Ctrl k", Action::OpenPalette).unwrap();
    map.insert_all("Ctrl Alt s", Action::ToggleSidebar).unwrap();
    map.insert_all("Ctrl Alt p", Action::TogglePanel).unwrap();
    map.insert_all("Ctrl Alt f", Action::ToggleDrawer).unwrap();
    map.insert_all("Ctrl Alt c", Action::CopyPane).unwrap();
    map.insert_all("Ctrl Alt n", Action::SwitchMode(Mode::Normal))
        .unwrap();
    map.insert_all("Ctrl Alt v", Action::SwitchMode(Mode::VimNormal))
        .unwrap();
    map.insert_all("Ctrl Alt e", Action::SwitchMode(Mode::Emacs))
        .unwrap();

    map.insert_all("Alt w", Action::NewWorktree).unwrap();
    map.insert_all("Alt W", Action::NewWorkspace).unwrap();
    map.insert_all("Alt t", Action::NewTab).unwrap();
    map.insert_all("Alt X", Action::CloseWorktree).unwrap();
    map.insert_all("Alt o", Action::SwitchWorkspace).unwrap();
    map.insert_all("Alt d", Action::Dashboard).unwrap();
    map.insert_all("Alt n", Action::SplitDown).unwrap();
    map.insert_all("Alt N", Action::SplitRight).unwrap();
    map.insert_all("Alt g", Action::Lazygit).unwrap();
    map.insert_all("Alt y", Action::Yazi).unwrap();
    map.insert_all("Alt e", Action::Editor).unwrap();
    map.insert_all("Alt /", Action::Diff).unwrap();

    map.insert_all("Alt h", Action::FocusLeft).unwrap();
    map.insert_all("Alt j", Action::FocusDown).unwrap();
    map.insert_all("Alt k", Action::FocusUp).unwrap();
    map.insert_all("Alt l", Action::FocusRight).unwrap();
    map.insert_all("Alt Left", Action::PrevTab).unwrap();
    map.insert_all("Alt Right", Action::NextTab).unwrap();

    map.insert_all("Shift PageUp", Action::ScrollUp).unwrap();
    map.insert_all("Shift PageDown", Action::ScrollDown)
        .unwrap();

    // Vim-normal mode: one-key navigation plus leader-like Space sequences.
    map.insert(Mode::VimNormal, "h", Action::FocusLeft).unwrap();
    map.insert(Mode::VimNormal, "j", Action::FocusDown).unwrap();
    map.insert(Mode::VimNormal, "k", Action::FocusUp).unwrap();
    map.insert(Mode::VimNormal, "l", Action::FocusRight)
        .unwrap();
    map.insert(Mode::VimNormal, "H", Action::PrevTab).unwrap();
    map.insert(Mode::VimNormal, "L", Action::NextTab).unwrap();
    map.insert(Mode::VimNormal, "g g", Action::ScrollUp)
        .unwrap();
    map.insert(Mode::VimNormal, "G", Action::ScrollDown)
        .unwrap();
    map.insert(Mode::VimNormal, "Ctrl u", Action::ScrollUp)
        .unwrap();
    map.insert(Mode::VimNormal, "Ctrl d", Action::ScrollDown)
        .unwrap();
    map.insert(Mode::VimNormal, "i", Action::SwitchMode(Mode::VimInsert))
        .unwrap();
    map.insert(Mode::VimNormal, "Escape", Action::SwitchMode(Mode::Normal))
        .unwrap();
    map.insert(Mode::VimNormal, "Space p", Action::TogglePanel)
        .unwrap();
    map.insert(Mode::VimNormal, "Space s", Action::ToggleSidebar)
        .unwrap();
    map.insert(Mode::VimNormal, "Space f", Action::ToggleDrawer)
        .unwrap();
    map.insert(Mode::VimNormal, "Space w", Action::NewWorktree)
        .unwrap();
    map.insert(Mode::VimNormal, "Space W", Action::NewWorkspace)
        .unwrap();
    map.insert(Mode::VimNormal, "Space t", Action::NewTab)
        .unwrap();
    map.insert(Mode::VimNormal, "Space x", Action::CloseWorktree)
        .unwrap();
    map.insert(Mode::VimNormal, "Space q", Action::Quit)
        .unwrap();
    map.insert(Mode::VimNormal, "Space Space", Action::OpenPalette)
        .unwrap();

    // Vim-insert mode: text goes through unless a global key or Esc is pressed.
    map.insert(
        Mode::VimInsert,
        "Escape",
        Action::SwitchMode(Mode::VimNormal),
    )
    .unwrap();

    // Emacs mode: host commands hang off C-x / M-x so readline-style C-a/C-e/etc.
    // still reach the shell.
    map.insert(Mode::Emacs, "Alt x", Action::OpenPalette)
        .unwrap();
    map.insert(Mode::Emacs, "Ctrl g", Action::SwitchMode(Mode::Normal))
        .unwrap();
    map.insert(Mode::Emacs, "Ctrl x Ctrl c", Action::Quit)
        .unwrap();
    map.insert(Mode::Emacs, "Ctrl x k", Action::CloseWorktree)
        .unwrap();
    map.insert(Mode::Emacs, "Ctrl x b", Action::NextTab)
        .unwrap();
    map.insert(Mode::Emacs, "Ctrl x 2", Action::SplitDown)
        .unwrap();
    map.insert(Mode::Emacs, "Ctrl x 3", Action::SplitRight)
        .unwrap();
    map.insert(Mode::Emacs, "Ctrl x o", Action::FocusRight)
        .unwrap();

    map
}

pub fn default_keymap_with_config(cfg: &superzej_core::config::Config) -> KeyMap {
    let mut map = default_keymap();
    map.config = cfg.clone();

    for action in &cfg.actions {
        let ca = HostCustomAction {
            name: action.name.clone(),
            run: action.run.clone(),
            floating: action.floating,
            close_on_exit: action.close_on_exit,
        };
        let idx = map.custom_actions.len() as u16;
        map.custom_actions.push(ca);
        apply_override(&mut map, None, &action.name, &action.key, Some(idx));
    }

    for (id, chord) in cfg.keybinds.iter() {
        apply_override(&mut map, None, id, chord, None);
    }
    for (id, chord) in &cfg.keybinds.vim_normal {
        apply_override(&mut map, Some(Mode::VimNormal), id, chord, None);
    }
    for (id, chord) in &cfg.keybinds.vim_insert {
        apply_override(&mut map, Some(Mode::VimInsert), id, chord, None);
    }
    for (id, chord) in &cfg.keybinds.emacs {
        apply_override(&mut map, Some(Mode::Emacs), id, chord, None);
    }

    map
}

fn apply_override(
    map: &mut KeyMap,
    mode: Option<Mode>,
    id: &str,
    chord: &str,
    custom_idx: Option<u16>,
) {
    let action = if let Some(idx) = custom_idx {
        Action::Custom(idx)
    } else if let Some(a) = action_from_id(id) {
        a
    } else {
        superzej_core::msg::warn(&format!("host keymap: unknown action {id:?}; ignored"));
        return;
    };

    let parsed = match parse_chord(chord) {
        Ok(keys) => keys,
        Err(e) => {
            superzej_core::msg::warn(&format!(
                "host keymap: {id}: invalid chord {chord:?}: {e}; keeping default"
            ));
            return;
        }
    };

    if let Some(mode) = mode {
        map.remove(mode, action);
        map.insert_keys(mode, parsed, action);
    } else {
        map.remove_all(action);
        for mode in ALL_MODES {
            map.insert_keys(mode, parsed.clone(), action);
        }
    }
}

fn action_from_id(id: &str) -> Option<Action> {
    Some(match id {
        "new-worktree" => Action::NewWorktree,
        "new-workspace" => Action::NewWorkspace,
        "new-tab" => Action::NewTab,
        "close-worktree" => Action::CloseWorktree,
        "switch-workspace" | "switch-repo" => Action::SwitchWorkspace,
        "dashboard" => Action::Dashboard,
        "next-tab" => Action::NextTab,
        "prev-tab" => Action::PrevTab,
        "split-down" | "new-panel-native" => Action::SplitDown,
        "split-right" | "new-panel" => Action::SplitRight,
        "focus-left" => Action::FocusLeft,
        "focus-right" => Action::FocusRight,
        "focus-up" => Action::FocusUp,
        "focus-down" => Action::FocusDown,
        "toggle-sidebar" => Action::ToggleSidebar,
        "toggle-panel" => Action::TogglePanel,
        "files" | "files-drawer" | "toggle-drawer" => Action::ToggleDrawer,
        "menu" | "palette" => Action::OpenPalette,
        "tool-lazygit" | "lazygit" => Action::Lazygit,
        "tool-yazi" | "yazi" => Action::Yazi,
        "tool-editor" | "editor" => Action::Editor,
        "tool-diff" | "show-diff" | "diff" => Action::Diff,
        "scroll-up" => Action::ScrollUp,
        "scroll-down" => Action::ScrollDown,
        "copy-pane" => Action::CopyPane,
        "quit" => Action::Quit,
        "mode-normal" => Action::SwitchMode(Mode::Normal),
        "mode-vim-normal" | "vim-normal" => Action::SwitchMode(Mode::VimNormal),
        "mode-vim-insert" | "vim-insert" => Action::SwitchMode(Mode::VimInsert),
        "mode-emacs" | "emacs" => Action::SwitchMode(Mode::Emacs),
        _ => return None,
    })
}

#[allow(dead_code)]
pub fn map_key(key: &KeyCode, mods: Modifiers) -> Option<Action> {
    match default_keymap().dispatch(Mode::Normal, Key::modified(*key, mods)) {
        MatchResult::Matched(a) => Some(a),
        _ => None,
    }
}

#[derive(Debug, Clone, Default)]
pub struct KeyMap {
    modes: std::collections::HashMap<Mode, crate::sequence::SequenceMatcher>,
}

impl KeyMap {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, mode: Mode, seq: Vec<crate::sequence::Key>, action: Action) {
        self.modes
            .entry(mode)
            .or_default()
            .add_sequence(seq, action);
    }

    pub fn dispatch(&mut self, mode: Mode, key: crate::sequence::Key) -> crate::sequence::MatchResult {
        use crate::sequence::MatchResult;

        if let Some(matcher) = self.modes.get_mut(&mode) {
            match matcher.feed(key.clone()) {
                MatchResult::None => {}
                other => return other,
            }
        }
        if mode != Mode::Normal {
            if let Some(matcher) = self.modes.get_mut(&Mode::Normal) {
                return matcher.feed(key);
            }
        }
        MatchResult::None
    }

    pub fn from_config(cfg: &superzej_core::config::Config) -> Self {
        let mut map = default_keymap();
        apply_overrides(&mut map, Mode::Normal, &cfg.keybinds.normal);
        apply_overrides(&mut map, Mode::VimNormal, &cfg.keybinds.vim_normal);
        apply_overrides(&mut map, Mode::VimInsert, &cfg.keybinds.vim_insert);
        apply_overrides(&mut map, Mode::Emacs, &cfg.keybinds.emacs);
        for action in &cfg.actions {
            match crate::sequence::Key::parse_sequence(&action.key) {
                Ok(seq) => map.insert(
                    Mode::Normal,
                    seq,
                    Action::ShellCommand {
                        name: action.name.clone(),
                        run: action.run.clone(),
                        floating: action.floating,
                        close_on_exit: action.close_on_exit,
                    },
                ),
                Err(e) => superzej_core::config::config_warn(&format!(
                    "[[actions]] {}: {e}; skipped",
                    action.name
                )),
            }
        }
        map
    }

    fn remove_action_key(&mut self, mode: Mode, key: &str) {
        if let Some(matcher) = self.modes.get_mut(&mode) {
            matcher.remove_action_key(key);
        }
    }
}

fn bind(map: &mut KeyMap, mode: Mode, chord: &str, action: Action) {
    match crate::sequence::Key::parse_sequence(chord) {
        Ok(seq) => map.insert(mode, seq, action),
        Err(e) => superzej_core::config::config_warn(&format!(
            "host keymap: {}: {e}; skipped",
            chord
        )),
    }
}

fn apply_overrides(map: &mut KeyMap, mode: Mode, overrides: &std::collections::BTreeMap<String, String>) {
    for (id, chord) in overrides {
        let Some(action) = Action::from_key(id) else {
            superzej_core::config::config_warn(&format!(
                "[keybinds] unknown host action {id:?}; ignored"
            ));
            continue;
        };
        let action_key = action.key().to_string();
        map.remove_action_key(mode, &action_key);
        bind(map, mode, chord, action);
    }
}

pub fn default_keymap() -> KeyMap {
    let mut map = KeyMap::new();
    // Normal mode preserves the native host's pre-modal global bindings.
    for (chord, action) in [
        ("Ctrl q", Action::Quit),
        ("Ctrl k", Action::OpenPalette),
        ("Ctrl Alt s", Action::ToggleSidebar),
        ("Ctrl Alt p", Action::TogglePanel),
        ("Ctrl Alt f", Action::ToggleDrawer),
        ("Ctrl Alt c", Action::CopyPane),
        ("Ctrl Alt v", Action::SwitchMode(Mode::VimNormal)),
        ("Ctrl Alt e", Action::SwitchMode(Mode::Emacs)),
        ("Ctrl Alt n", Action::SwitchMode(Mode::Normal)),
        ("Alt w", Action::NewWorktree),
        ("Alt W", Action::NewWorkspace),
        ("Alt t", Action::NewTab),
        ("Alt X", Action::CloseWorktree),
        ("Alt o", Action::SwitchWorkspace),
        ("Alt d", Action::Dashboard),
        ("Alt n", Action::SplitDown),
        ("Alt N", Action::SplitRight),
        ("Alt g", Action::Lazygit),
        ("Alt y", Action::Yazi),
        ("Alt e", Action::Editor),
        ("Alt /", Action::Diff),
        ("Alt h", Action::FocusLeft),
        ("Alt j", Action::FocusDown),
        ("Alt k", Action::FocusUp),
        ("Alt l", Action::FocusRight),
        ("Alt Left", Action::PrevTab),
        ("Alt Right", Action::NextTab),
        ("Shift PageUp", Action::ScrollUp),
        ("Shift PageDown", Action::ScrollDown),
    ] {
        bind(&mut map, Mode::Normal, chord, action);
    }

    // Vim-normal preset: pane/tab movement without Alt, sequence scroll, and
    // explicit mode transitions. Unbound keys still fall through to the pane.
    for (chord, action) in [
        ("h", Action::FocusLeft),
        ("j", Action::FocusDown),
        ("k", Action::FocusUp),
        ("l", Action::FocusRight),
        ("H", Action::PrevTab),
        ("L", Action::NextTab),
        ("g g", Action::ScrollUp),
        ("G", Action::ScrollDown),
        ("Ctrl u", Action::ScrollUp),
        ("Ctrl d", Action::ScrollDown),
        ("i", Action::SwitchMode(Mode::VimInsert)),
        ("a", Action::SwitchMode(Mode::VimInsert)),
        ("Esc", Action::SwitchMode(Mode::Normal)),
        ("Ctrl Alt n", Action::SwitchMode(Mode::Normal)),
    ] {
        bind(&mut map, Mode::VimNormal, chord, action);
    }

    for (chord, action) in [
        ("Esc", Action::SwitchMode(Mode::VimNormal)),
        ("Ctrl Alt n", Action::SwitchMode(Mode::Normal)),
        ("Ctrl Alt e", Action::SwitchMode(Mode::Emacs)),
    ] {
        bind(&mut map, Mode::VimInsert, chord, action);
    }

    for (chord, action) in [
        ("Ctrl b", Action::FocusLeft),
        ("Ctrl f", Action::FocusRight),
        ("Ctrl p", Action::FocusUp),
        ("Ctrl n", Action::FocusDown),
        ("Alt b", Action::PrevTab),
        ("Alt f", Action::NextTab),
        ("Alt v", Action::ScrollUp),
        ("Ctrl v", Action::ScrollDown),
        ("Ctrl Alt n", Action::SwitchMode(Mode::Normal)),
        ("Ctrl Alt v", Action::SwitchMode(Mode::VimNormal)),
    ] {
        bind(&mut map, Mode::Emacs, chord, action);
    }

    map
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
<<<<<<< Updated upstream

    #[test]
    fn mode_actions_have_stable_keys() {
        assert_eq!(Action::SwitchMode(Mode::Normal).key(), "mode-normal");
        assert_eq!(Action::SwitchMode(Mode::VimNormal).key(), "mode-vim-normal");
        assert_eq!(Action::SwitchMode(Mode::VimInsert).key(), "mode-vim-insert");
        assert_eq!(Action::SwitchMode(Mode::Emacs).key(), "mode-emacs");
    }

    #[test]
    fn keymap_dispatches_sequences_by_mode() {
        let mut map = KeyMap::new();
        map.insert(Mode::VimNormal, "g g", Action::ScrollUp)
            .unwrap();
        map.insert(Mode::VimNormal, "j", Action::FocusDown).unwrap();

        assert_eq!(
            map.dispatch(Mode::Normal, Key::char('j')),
            MatchResult::None
        );
        assert_eq!(
            map.dispatch(Mode::VimNormal, Key::char('j')),
            MatchResult::Matched(Action::FocusDown)
        );
        assert_eq!(
            map.dispatch(Mode::VimNormal, Key::char('g')),
            MatchResult::Pending
        );
        assert_eq!(
            map.dispatch(Mode::VimNormal, Key::char('g')),
            MatchResult::Matched(Action::ScrollUp)
        );
    }

    #[test]
    fn default_keymap_preserves_existing_global_chords_and_adds_presets() {
        let mut map = default_keymap();
        assert_eq!(
            map.dispatch(
                Mode::Normal,
                Key::modified(KeyCode::Char('w'), Modifiers::ALT)
            ),
            MatchResult::Matched(Action::NewWorktree)
        );
        assert_eq!(
            map.dispatch(Mode::VimNormal, Key::char('j')),
            MatchResult::Matched(Action::FocusDown)
        );
        assert_eq!(
            map.dispatch(Mode::VimNormal, Key::char('i')),
            MatchResult::Matched(Action::SwitchMode(Mode::VimInsert))
        );
        assert_eq!(
            map.dispatch(Mode::VimInsert, Key::from_code(KeyCode::Escape)),
            MatchResult::Matched(Action::SwitchMode(Mode::VimNormal))
        );
        assert_eq!(
            map.dispatch(Mode::Emacs, Key::ctrl('x')),
            MatchResult::Pending
        );
        assert_eq!(
            map.dispatch(Mode::Emacs, Key::ctrl('c')),
            MatchResult::Matched(Action::Quit)
        );
    }

    #[test]
    fn config_rebinds_global_and_mode_specific_actions() {
        let mut cfg = superzej_core::config::Config::default();
        cfg.keybinds.insert("focus-down".into(), "Ctrl j".into());
        cfg.keybinds
            .vim_normal
            .insert("focus-down".into(), "J".into());
        cfg.keybinds.emacs.insert("quit".into(), "Ctrl x q".into());

        let mut map = default_keymap_with_config(&cfg);
        assert_eq!(
            map.dispatch(
                Mode::Normal,
                Key::modified(KeyCode::Char('j'), Modifiers::ALT)
            ),
            MatchResult::None
        );
        assert_eq!(
            map.dispatch(Mode::Normal, Key::ctrl('j')),
            MatchResult::Matched(Action::FocusDown)
        );
        assert_eq!(
            map.dispatch(Mode::VimNormal, Key::char('j')),
            MatchResult::None
        );
        assert_eq!(
            map.dispatch(Mode::VimNormal, Key::char('J')),
            MatchResult::Matched(Action::FocusDown)
        );
        assert_eq!(
            map.dispatch(Mode::Emacs, Key::ctrl('x')),
            MatchResult::Pending
        );
        assert_eq!(
            map.dispatch(Mode::Emacs, Key::char('q')),
            MatchResult::Matched(Action::Quit)
        );
    }
||||||| Stash base
=======

    #[test]
    fn action_keys_include_modes() {
        assert_eq!(Action::SwitchMode(Mode::Normal).key(), "mode-normal");
        assert_eq!(Action::SwitchMode(Mode::VimNormal).key(), "mode-vim-normal");
        assert_eq!(Action::SwitchMode(Mode::VimInsert).key(), "mode-vim-insert");
        assert_eq!(Action::SwitchMode(Mode::Emacs).key(), "mode-emacs");
    }

    #[test]
    fn sequence_matching_advances_state() {
        use crate::sequence::{Key, MatchResult, SequenceMatcher};

        let mut matcher = SequenceMatcher::new();
        matcher.add_sequence(vec![Key::char('g'), Key::char('g')], Action::ScrollUp);

        assert_eq!(matcher.feed(Key::char('g')), MatchResult::Pending);
        assert_eq!(
            matcher.feed(Key::char('g')),
            MatchResult::Matched(Action::ScrollUp)
        );
    }

    #[test]
    fn mode_specific_dispatch() {
        use crate::sequence::{Key, MatchResult};

        let mut map = KeyMap::new();
        map.insert(Mode::VimNormal, vec![Key::char('j')], Action::FocusDown);

        assert_eq!(map.dispatch(Mode::Normal, Key::char('j')), MatchResult::None);
        assert_eq!(
            map.dispatch(Mode::VimNormal, Key::char('j')),
            MatchResult::Matched(Action::FocusDown)
        );
    }

    #[test]
    fn default_keymap_preserves_existing_global_chords() {
        use crate::sequence::{Key, MatchResult};

        let mut map = default_keymap();
        assert_eq!(
            map.dispatch(Mode::Normal, Key::new(KeyCode::Char('w'), Modifiers::ALT)),
            MatchResult::Matched(Action::NewWorktree)
        );
        assert_eq!(
            map.dispatch(Mode::Normal, Key::new(KeyCode::Char('k'), Modifiers::CTRL)),
            MatchResult::Matched(Action::OpenPalette)
        );
    }

    #[test]
    fn config_keymap_supports_normal_and_mode_layers() {
        use crate::sequence::{Key, MatchResult};

        let cfg: superzej_core::config::Config = toml::from_str(
            r#"
            [keybinds]
            new-worktree = "Ctrl w"

            [keybinds.vim_normal]
            focus-down = "j"
            mode-vim-insert = "i"
            "#,
        )
        .unwrap();

        let mut map = KeyMap::from_config(&cfg);
        assert_eq!(
            map.dispatch(Mode::Normal, Key::new(KeyCode::Char('w'), Modifiers::CTRL)),
            MatchResult::Matched(Action::NewWorktree)
        );
        assert_eq!(
            map.dispatch(Mode::VimNormal, Key::char('j')),
            MatchResult::Matched(Action::FocusDown)
        );
        assert_eq!(
            map.dispatch(Mode::VimNormal, Key::char('i')),
            MatchResult::Matched(Action::SwitchMode(Mode::VimInsert))
        );
    }
>>>>>>> Stashed changes
}
