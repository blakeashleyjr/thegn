//! The host keymap: terminal key chords → host `Action`s. The native host owns
//! input routing, so global chords are intercepted here and everything else is
//! forwarded to the focused pane (Alt-w new worktree, Alt-o switch, Ctrl-Alt-s/p
//! toggles, Alt-1..9 pins, splits, focus moves, …). User overrides come from
//! `[keybinds]` in the config. Pure + unit-tested; the loop calls `map_key`.

use termwiz::input::{KeyCode, Modifiers};

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
/// A host-level action, decoupled from any key. The command palette dispatches
/// the same set (by `key()`), so keymap and palette share one action vocabulary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    NewWorktree,
    NewWorkspace,
    NewTab,
    /// Zellij-style smart split: along the focused pane's longer dimension.
    NewPane,
    /// Fullscreen the focused zone (pane / sidebar / panel); toggles off.
    ToggleZoom,
    /// Cycle through the named theme presets (storm → light → abyss → …).
    CycleTheme,
    /// Pick a font family from fontconfig and patch the live alacritty profile.
    SwitchFont,
    /// Close the active tab within the worktree (closing the last tab closes
    /// the whole worktree group).
    CloseTab,
    CloseWorktree,
    SwitchWorkspace,
    Dashboard,
    NextTab,
    PrevTab,
    /// Move to the next worktree group (Alt+Down); restores its active tab.
    NextWorktree,
    /// Move to the previous worktree group (Alt+Up).
    PrevWorktree,
    SplitDown,
    SplitRight,
    FocusLeft,
    FocusRight,
    FocusUp,
    FocusDown,
    ToggleSidebar,
    TogglePanel,
    ToggleDrawer,
    /// Move keyboard focus into the sidebar tree (shows it if hidden).
    FocusSidebar,
    /// Move keyboard focus into the right panel (shows it if hidden).
    FocusPanel,
    OpenPalette,
    Lazygit,
    Yazi,
    Editor,
    Diff,
    ScrollUp,
    ScrollDown,
    CopyPane,
    /// Open an incremental fuzzy-search overlay over the focused pane's history.
    SearchPane,
    /// Open the search overlay scoped to the active worktree (Tab → cycle wider).
    SearchGlobal,
    /// Toggle the Ctrl+g keybind lock: while locked every key except Ctrl+g
    /// passes through to the focused pane (compositor chords are suspended).
    ToggleKeyLock,
    SwitchMode(Mode),
    /// Launch-or-focus the pin at 1-based index N (the `Alt-1..9` mapping).
    SummonPin(u8),
    /// Show/hide the top pinned-program strip.
    ToggleStrip,
    /// Grow the top strip (more rows).
    GrowStrip,
    /// Shrink the top strip (fewer rows).
    ShrinkStrip,
    /// Promote the focused center pane into the top strip as a pin.
    PromotePin,
    /// Unpin (stop + remove) the focused strip/float pin.
    Unpin,
    Quit,
    Custom(u16),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActionSpec {
    /// Stable action id; matches [`Action::key`] and command-palette dispatch.
    pub id: &'static str,
    /// Human label shown in the command palette and help surfaces.
    pub label: &'static str,
    /// Short label for compact bottom-bar hints.
    pub hint: &'static str,
    /// Built-in normal-mode/default chords. Config layers may override these.
    pub default_chords: &'static [&'static str],
    /// Whether this action should be surfaced in the command palette.
    pub palette: bool,
}

/// Native-host action registry: one table drives palette rows, compact hints,
/// and tests that every real host action is discoverable. Keep ids aligned with
/// [`Action::key`] / [`Action::from_key`]; legacy aliases stay in `from_key`.
pub const ACTION_SPECS: &[ActionSpec] = &[
    ActionSpec {
        id: "new-worktree",
        label: "New worktree",
        hint: "worktree",
        default_chords: &["Alt w"],
        palette: true,
    },
    ActionSpec {
        id: "new-workspace",
        label: "New workspace",
        hint: "workspace",
        default_chords: &["Alt W"],
        palette: true,
    },
    ActionSpec {
        id: "new-tab",
        label: "New tab — same worktree",
        hint: "tab",
        default_chords: &["Alt t"],
        palette: true,
    },
    ActionSpec {
        id: "new-pane",
        label: "New pane — smart split",
        hint: "smart split",
        default_chords: &["Alt p"],
        palette: true,
    },
    ActionSpec {
        id: "split-down",
        label: "Split pane down",
        hint: "split↓",
        default_chords: &["Alt n"],
        palette: true,
    },
    ActionSpec {
        id: "split-right",
        label: "Split pane right",
        hint: "split→",
        default_chords: &["Alt N"],
        palette: true,
    },
    ActionSpec {
        id: "zoom",
        label: "Toggle zoom",
        hint: "zoom",
        default_chords: &["Ctrl Alt z"],
        palette: true,
    },
    ActionSpec {
        id: "cycle-theme",
        label: "Cycle theme",
        hint: "theme",
        default_chords: &["Ctrl Alt t"],
        palette: true,
    },
    ActionSpec {
        id: "switch-font",
        label: "Switch font",
        hint: "font",
        default_chords: &["Alt f", "Alt F"],
        palette: true,
    },
    ActionSpec {
        id: "close-tab",
        label: "Close tab",
        hint: "close tab",
        default_chords: &["Alt x"],
        palette: true,
    },
    ActionSpec {
        id: "close-worktree",
        label: "Close worktree",
        hint: "close worktree",
        default_chords: &["Alt X"],
        palette: true,
    },
    ActionSpec {
        id: "switch-workspace",
        label: "Switch workspace",
        hint: "switch",
        default_chords: &["Alt o"],
        palette: true,
    },
    ActionSpec {
        id: "dashboard",
        label: "Dashboard",
        hint: "dashboard",
        default_chords: &["Alt d"],
        palette: true,
    },
    ActionSpec {
        id: "prev-tab",
        label: "Previous tab",
        hint: "prev tab",
        default_chords: &["Alt Left"],
        palette: true,
    },
    ActionSpec {
        id: "next-tab",
        label: "Next tab",
        hint: "next tab",
        default_chords: &["Alt Right"],
        palette: true,
    },
    ActionSpec {
        id: "prev-worktree",
        label: "Previous worktree",
        hint: "prev worktree",
        default_chords: &["Alt Up"],
        palette: true,
    },
    ActionSpec {
        id: "next-worktree",
        label: "Next worktree",
        hint: "next worktree",
        default_chords: &["Alt Down"],
        palette: true,
    },
    ActionSpec {
        id: "focus-left",
        label: "Focus left",
        hint: "focus←",
        default_chords: &["Ctrl Left", "Ctrl h"],
        palette: true,
    },
    ActionSpec {
        id: "focus-right",
        label: "Focus right",
        hint: "focus→",
        default_chords: &["Ctrl Right", "Ctrl l"],
        palette: true,
    },
    ActionSpec {
        id: "focus-up",
        label: "Focus up",
        hint: "focus↑",
        default_chords: &["Ctrl Up", "Ctrl k"],
        palette: true,
    },
    ActionSpec {
        id: "focus-down",
        label: "Focus down",
        hint: "focus↓",
        default_chords: &["Ctrl Down", "Ctrl j"],
        palette: true,
    },
    ActionSpec {
        id: "toggle-sidebar",
        label: "Toggle sidebar",
        hint: "sidebar",
        default_chords: &["Ctrl Alt s"],
        palette: true,
    },
    ActionSpec {
        id: "toggle-panel",
        label: "Toggle diff / PR panel",
        hint: "panel",
        default_chords: &["Ctrl Alt p"],
        palette: true,
    },
    ActionSpec {
        id: "files-drawer",
        label: "Toggle files drawer",
        hint: "drawer",
        default_chords: &["Ctrl Alt f"],
        palette: true,
    },
    ActionSpec {
        id: "focus-sidebar",
        label: "Focus workspace sidebar",
        hint: "sidebar",
        default_chords: &["Alt s"],
        palette: true,
    },
    ActionSpec {
        id: "focus-panel",
        label: "Focus diff / PR panel",
        hint: "panel",
        default_chords: &["Alt ."],
        palette: true,
    },
    ActionSpec {
        id: "palette",
        label: "Command palette",
        hint: "menu",
        default_chords: &["Ctrl Space"],
        palette: true,
    },
    ActionSpec {
        id: "lazygit",
        label: "Open lazygit",
        hint: "lazygit",
        default_chords: &["Alt g"],
        palette: true,
    },
    ActionSpec {
        id: "yazi",
        label: "Open yazi drawer",
        hint: "yazi",
        default_chords: &["Alt y"],
        palette: true,
    },
    ActionSpec {
        id: "editor",
        label: "Open editor",
        hint: "editor",
        default_chords: &["Alt e"],
        palette: true,
    },
    ActionSpec {
        id: "show-diff",
        label: "Open git diff",
        hint: "diff",
        default_chords: &["Alt /"],
        palette: true,
    },
    ActionSpec {
        id: "scroll-up",
        label: "Scroll pane up",
        hint: "scroll↑",
        default_chords: &["Shift PageUp"],
        palette: true,
    },
    ActionSpec {
        id: "scroll-down",
        label: "Scroll pane down",
        hint: "scroll↓",
        default_chords: &["Shift PageDown"],
        palette: true,
    },
    ActionSpec {
        id: "copy-pane",
        label: "Copy pane contents",
        hint: "copy",
        default_chords: &["Ctrl Alt c"],
        palette: true,
    },
    ActionSpec {
        id: "search-pane",
        label: "Search pane history",
        hint: "search",
        default_chords: &["/"],
        palette: true,
    },
    ActionSpec {
        id: "search-global",
        label: "Search across all panes (worktree scope)",
        hint: "search all",
        default_chords: &["Ctrl /"],
        palette: true,
    },
    ActionSpec {
        id: "toggle-key-lock",
        label: "Lock/unlock keybinds (pass through)",
        hint: "lock",
        default_chords: &["Ctrl g"],
        palette: true,
    },
    ActionSpec {
        id: "mode-normal",
        label: "Switch to Normal mode",
        hint: "normal",
        default_chords: &["Ctrl Alt n"],
        palette: true,
    },
    ActionSpec {
        id: "mode-vim-normal",
        label: "Switch to Vim-normal mode",
        hint: "vim",
        default_chords: &["Ctrl Alt v"],
        palette: true,
    },
    ActionSpec {
        id: "mode-vim-insert",
        label: "Switch to Vim-insert mode",
        hint: "insert",
        default_chords: &[],
        palette: true,
    },
    ActionSpec {
        id: "mode-emacs",
        label: "Switch to Emacs mode",
        hint: "emacs",
        default_chords: &["Ctrl Alt e"],
        palette: true,
    },
    ActionSpec {
        id: "toggle-strip",
        label: "Toggle pin strip",
        hint: "pins",
        default_chords: &["Ctrl Alt b"],
        palette: true,
    },
    ActionSpec {
        id: "grow-strip",
        label: "Grow pin strip",
        hint: "pins+",
        default_chords: &["Ctrl Alt ]"],
        palette: true,
    },
    ActionSpec {
        id: "shrink-strip",
        label: "Shrink pin strip",
        hint: "pins-",
        default_chords: &["Ctrl Alt ["],
        palette: true,
    },
    ActionSpec {
        id: "promote-pin",
        label: "Promote pane to pin strip",
        hint: "pin pane",
        default_chords: &["Ctrl Alt P"],
        palette: true,
    },
    ActionSpec {
        id: "unpin",
        label: "Unpin focused/first pin",
        hint: "unpin",
        default_chords: &["Ctrl Alt U"],
        palette: true,
    },
    ActionSpec {
        id: "quit",
        label: "Quit superzej",
        hint: "quit",
        default_chords: &["Ctrl q"],
        palette: true,
    },
];

pub fn action_specs() -> &'static [ActionSpec] {
    ACTION_SPECS
}

pub fn action_spec(id: &str) -> Option<&'static ActionSpec> {
    ACTION_SPECS.iter().find(|s| s.id == id)
}

pub fn chord_hint_for(cfg: &superzej_core::config::Config, id: &str) -> Option<String> {
    let mut chord = action_spec(id)
        .and_then(|s| s.default_chords.first().copied())
        .map(str::to_string);
    for layer in cfg.effective_keybinds(None, None) {
        if let Some(override_chord) = layer.normal.get(id) {
            chord = Some(override_chord.clone());
        }
    }
    chord.map(|c| c.replace(' ', "-"))
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
            Action::NewPane => "new-pane",
            Action::ToggleZoom => "zoom",
            Action::CycleTheme => "cycle-theme",
            Action::SwitchFont => "switch-font",
            Action::CloseTab => "close-tab",
            Action::CloseWorktree => "close-worktree",
            Action::SwitchWorkspace => "switch-workspace",
            Action::Dashboard => "dashboard",
            Action::NextTab => "next-tab",
            Action::PrevTab => "prev-tab",
            Action::NextWorktree => "next-worktree",
            Action::PrevWorktree => "prev-worktree",
            Action::SplitDown => "split-down",
            Action::SplitRight => "split-right",
            Action::FocusLeft => "focus-left",
            Action::FocusRight => "focus-right",
            Action::FocusUp => "focus-up",
            Action::FocusDown => "focus-down",
            Action::ToggleSidebar => "toggle-sidebar",
            Action::TogglePanel => "toggle-panel",
            Action::ToggleDrawer => "files-drawer",
            Action::FocusSidebar => "focus-sidebar",
            Action::FocusPanel => "focus-panel",
            Action::OpenPalette => "palette",
            Action::Lazygit => "lazygit",
            Action::Yazi => "yazi",
            Action::Editor => "editor",
            Action::Diff => "show-diff",
            Action::ScrollUp => "scroll-up",
            Action::ScrollDown => "scroll-down",
            Action::CopyPane => "copy-pane",
            Action::SearchPane => "search-pane",
            Action::SearchGlobal => "search-global",
            Action::ToggleKeyLock => "toggle-key-lock",
            Action::SwitchMode(Mode::Normal) => "mode-normal",
            Action::SwitchMode(Mode::VimNormal) => "mode-vim-normal",
            Action::SwitchMode(Mode::VimInsert) => "mode-vim-insert",
            Action::SwitchMode(Mode::Emacs) => "mode-emacs",
            Action::SummonPin(_) => "summon-pin",
            Action::ToggleStrip => "toggle-strip",
            Action::GrowStrip => "grow-strip",
            Action::ShrinkStrip => "shrink-strip",
            Action::PromotePin => "promote-pin",
            Action::Unpin => "unpin",
            Action::Quit => "quit",
            Action::Custom(_) => "custom-action",
        }
    }

    pub fn from_key(key: &str) -> Option<Action> {
        Some(match key {
            "new-worktree" => Action::NewWorktree,
            "new-workspace" => Action::NewWorkspace,
            "new-tab" => Action::NewTab,
            "new-pane" => Action::NewPane,
            "zoom" | "toggle-zoom" | "fullscreen" => Action::ToggleZoom,
            "cycle-theme" | "theme" => Action::CycleTheme,
            "switch-font" | "font" => Action::SwitchFont,
            "close-tab" => Action::CloseTab,
            "close-worktree" => Action::CloseWorktree,
            "switch-workspace" | "switch-repo" => Action::SwitchWorkspace,
            "dashboard" => Action::Dashboard,
            "next-tab" => Action::NextTab,
            "prev-tab" => Action::PrevTab,
            "next-worktree" => Action::NextWorktree,
            "prev-worktree" => Action::PrevWorktree,
            "split-down" | "new-panel-native" => Action::SplitDown,
            "split-right" | "new-panel" => Action::SplitRight,
            "focus-left" => Action::FocusLeft,
            "focus-right" => Action::FocusRight,
            "focus-up" => Action::FocusUp,
            "focus-down" => Action::FocusDown,
            "toggle-sidebar" => Action::ToggleSidebar,
            "toggle-panel" => Action::TogglePanel,
            "files" | "files-drawer" | "toggle-drawer" => Action::ToggleDrawer,
            "focus-sidebar" => Action::FocusSidebar,
            "focus-panel" => Action::FocusPanel,
            "palette" | "menu" => Action::OpenPalette,
            "lazygit" | "tool-lazygit" => Action::Lazygit,
            "yazi" | "tool-yazi" => Action::Yazi,
            "editor" | "tool-editor" => Action::Editor,
            "show-diff" | "diff" | "tool-diff" => Action::Diff,
            "scroll-up" => Action::ScrollUp,
            "scroll-down" => Action::ScrollDown,
            "copy-pane" => Action::CopyPane,
            "search-pane" | "search" => Action::SearchPane,
            "search-global" => Action::SearchGlobal,
            "toggle-key-lock" | "key-lock" | "lock" => Action::ToggleKeyLock,
            "quit" => Action::Quit,
            "mode-normal" => Action::SwitchMode(Mode::Normal),
            "mode-vim-normal" | "vim-normal" => Action::SwitchMode(Mode::VimNormal),
            "mode-vim-insert" | "vim-insert" => Action::SwitchMode(Mode::VimInsert),
            "mode-emacs" | "emacs" => Action::SwitchMode(Mode::Emacs),
            "toggle-strip" => Action::ToggleStrip,
            "grow-strip" => Action::GrowStrip,
            "shrink-strip" => Action::ShrinkStrip,
            "promote-pin" => Action::PromotePin,
            "unpin" => Action::Unpin,
            // `summon-pin-N` / `pin-N` → SummonPin(N) (1..=9).
            other => {
                let n = other
                    .strip_prefix("summon-pin-")
                    .or_else(|| other.strip_prefix("pin-"))
                    .and_then(|s| s.parse::<u8>().ok())
                    .filter(|n| (1..=9).contains(n))?;
                Action::SummonPin(n)
            }
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
    /// Per-program host-action overlay: `program → [(single chord, Action)]`.
    /// Single-chord only (consulted before the mode matcher, so it stays
    /// stateless and can't desync the sequence matchers). A small Vec rather
    /// than a map because `Key` (termwiz `Modifiers`) is not `Hash`.
    program_overlays: std::collections::HashMap<String, Vec<(Key, Action)>>,
    /// Per-program key-injection remaps: `program → [(single chord, target keys)]`.
    /// Applied only when a chord is not claimed as a host action.
    program_remaps: std::collections::HashMap<String, Vec<(Key, Vec<Key>)>>,
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
            program_overlays: std::collections::HashMap::new(),
            program_remaps: std::collections::HashMap::new(),
        }
    }

    /// The host action a focused `program` binds to a single `key`, if any
    /// (`[program_keybinds.<program>]`). Consulted before the mode matcher.
    pub fn program_action(&self, program: &str, key: &Key) -> Option<Action> {
        self.program_overlays
            .get(program)?
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, a)| a.clone())
    }

    /// The keys a focused `program` remaps a single `key` to, if any
    /// (`[program_remap.<program>]`). The caller injects these into the pane.
    pub fn program_remap(&self, program: &str, key: &Key) -> Option<&[Key]> {
        self.program_remaps
            .get(program)?
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_slice())
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
            self.insert_keys(mode, keys.clone(), action.clone());
        }
        Ok(())
    }

    pub fn remove(&mut self, mode: Mode, action: &Action) {
        if let Some(m) = self.modes.get_mut(&mode) {
            m.remove_action(action);
        }
    }

    pub fn remove_all(&mut self, action: &Action) {
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

    /// Next-key candidates for the pending prefix in `mode` (drives which-key).
    pub fn pending_continuations(&self, mode: Mode) -> Vec<(Key, Action)> {
        self.modes
            .get(&mode)
            .map(|m| m.pending_continuations())
            .unwrap_or_default()
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
    // Palette lives on Ctrl+Space (Ctrl+k is a focus move; see below).
    map.insert_all("Ctrl Space", Action::OpenPalette).unwrap();
    // The keybind lock: Ctrl+g suspends every compositor chord (the loop
    // checks the lock before this map) so panes get Ctrl keys back.
    map.insert_all("Ctrl g", Action::ToggleKeyLock).unwrap();
    map.insert_all("Ctrl Alt s", Action::ToggleSidebar).unwrap();
    map.insert_all("Ctrl Alt p", Action::TogglePanel).unwrap();
    map.insert_all("Ctrl Alt f", Action::ToggleDrawer).unwrap();
    map.insert_all("Alt s", Action::FocusSidebar).unwrap();
    map.insert_all("Alt .", Action::FocusPanel).unwrap();
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
    map.insert_all("Alt p", Action::NewPane).unwrap();
    map.insert_all("Ctrl Alt z", Action::ToggleZoom).unwrap();
    map.insert_all("Ctrl Alt t", Action::CycleTheme).unwrap();
    map.insert_all("Alt f", Action::SwitchFont).unwrap();
    map.insert_all("Alt F", Action::SwitchFont).unwrap();
    map.insert_all("Alt x", Action::CloseTab).unwrap();
    map.insert_all("Alt X", Action::CloseWorktree).unwrap();
    map.insert_all("Alt o", Action::SwitchWorkspace).unwrap();
    map.insert_all("Alt d", Action::Dashboard).unwrap();
    map.insert_all("Alt n", Action::SplitDown).unwrap();
    map.insert_all("Alt N", Action::SplitRight).unwrap();
    map.insert_all("Alt g", Action::Lazygit).unwrap();
    map.insert_all("Alt y", Action::Yazi).unwrap();
    map.insert_all("Alt e", Action::Editor).unwrap();
    map.insert_all("Alt /", Action::Diff).unwrap();
    map.insert_all("Alt s", Action::FocusSidebar).unwrap();

    // Ctrl owns focus: one spatial graph across sidebar ← panes → panel.
    // Arrows always work; h/j/k/l mirror them (kitty-protocol terminals
    // disambiguate; on legacy terminals those keys pass through and the
    // arrows carry the feature). Ctrl+g suspends all of these.
    map.insert_all("Ctrl Left", Action::FocusLeft).unwrap();
    map.insert_all("Ctrl Down", Action::FocusDown).unwrap();
    map.insert_all("Ctrl Up", Action::FocusUp).unwrap();
    map.insert_all("Ctrl Right", Action::FocusRight).unwrap();
    map.insert_all("Ctrl h", Action::FocusLeft).unwrap();
    map.insert_all("Ctrl j", Action::FocusDown).unwrap();
    map.insert_all("Ctrl k", Action::FocusUp).unwrap();
    map.insert_all("Ctrl l", Action::FocusRight).unwrap();
    // Alt owns tabs/worktrees: ←/→ cycles tabs WITHIN the active worktree,
    // ↑/↓ moves between worktrees (each restores its own active tab).
    map.insert_all("Alt Left", Action::PrevTab).unwrap();
    map.insert_all("Alt Right", Action::NextTab).unwrap();
    map.insert_all("Alt Up", Action::PrevWorktree).unwrap();
    map.insert_all("Alt Down", Action::NextWorktree).unwrap();

    map.insert_all("Shift PageUp", Action::ScrollUp).unwrap();
    map.insert_all("Shift PageDown", Action::ScrollDown)
        .unwrap();

    // Search: "/" for focused-pane history, "Ctrl /" for worktree-wide scope.
    map.insert_all("/", Action::SearchPane).unwrap();
    map.insert_all("Ctrl /", Action::SearchGlobal).unwrap();

    // Pins: Alt-1..9 launch-or-focus the configured pin in registration order;
    // strip visibility/sizing and promote/unpin hang off Ctrl-Alt chords.
    for n in 1u8..=9 {
        map.insert_all(&format!("Alt {n}"), Action::SummonPin(n))
            .unwrap();
    }
    map.insert_all("Ctrl Alt b", Action::ToggleStrip).unwrap();
    map.insert_all("Ctrl Alt ]", Action::GrowStrip).unwrap();
    map.insert_all("Ctrl Alt [", Action::ShrinkStrip).unwrap();
    map.insert_all("Ctrl Alt P", Action::PromotePin).unwrap();
    map.insert_all("Ctrl Alt U", Action::Unpin).unwrap();

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
    map.insert(Mode::VimNormal, "Space x", Action::CloseTab)
        .unwrap();
    map.insert(Mode::VimNormal, "Space X", Action::CloseWorktree)
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
    // (Ctrl+g is the global keybind lock; mode-switching stays on Ctrl+Alt+n.)
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

#[allow(dead_code)] // retained for tests + as the no-context convenience wrapper
pub fn default_keymap_with_config(cfg: &superzej_core::config::Config) -> KeyMap {
    default_keymap_for(cfg, None, None)
}

/// Build the host keymap for a focused context: the built-in defaults, custom
/// `[[actions]]`, then each keybind layer from [`Config::effective_keybinds`]
/// applied lowest-precedence-first (profile → global → workspace → repo-root).
/// `repo_root`/`slug` are `None` outside a workspace (e.g. the home tab).
pub fn default_keymap_for(
    cfg: &superzej_core::config::Config,
    repo_root: Option<&std::path::Path>,
    slug: Option<&str>,
) -> KeyMap {
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

    for layer in cfg.effective_keybinds(repo_root, slug) {
        apply_keybind_layer(&mut map, &layer);
    }

    // Per-program host-action overlays: `[program_keybinds.<prog>] action = "Chord"`.
    for (program, binds) in &cfg.program_keybinds {
        for (id, chord) in binds.iter() {
            let Some(action) = Action::from_key(id) else {
                superzej_core::msg::warn(&format!(
                    "program_keybinds.{program}: unknown action {id:?}; ignored"
                ));
                continue;
            };
            match single_key(chord) {
                Ok(key) => {
                    map.program_overlays
                        .entry(program.clone())
                        .or_default()
                        .push((key, action));
                }
                Err(e) => superzej_core::msg::warn(&format!(
                    "program_keybinds.{program}: {id}: {e}; ignored"
                )),
            }
        }
    }

    // Per-program key-injection remaps: `[program_remap.<prog>] "Chord" = "Chord"`.
    for (program, remaps) in &cfg.program_remap {
        for (from, to) in remaps {
            match (single_key(from), parse_chord(to)) {
                (Ok(src), Ok(dst)) => {
                    map.program_remaps
                        .entry(program.clone())
                        .or_default()
                        .push((src, dst));
                }
                (Err(e), _) | (_, Err(e)) => superzej_core::msg::warn(&format!(
                    "program_remap.{program}: {from:?} -> {to:?}: {e}; ignored"
                )),
            }
        }
    }

    map
}

/// Parse a chord that must be a single key (no multi-key sequence). Used by the
/// per-program overlay/remap tables, which are stateless single-chord maps.
fn single_key(chord: &str) -> Result<Key, String> {
    let keys = parse_chord(chord)?;
    match keys.len() {
        1 => Ok(keys.into_iter().next().unwrap()),
        n => Err(format!("expected a single key, got a {n}-key sequence")),
    }
}

/// Apply one keybind layer (a [`KeybindConfig`]) onto `map`: the flat table
/// rebinds across all modes, the nested tables rebind their named mode only.
fn apply_keybind_layer(map: &mut KeyMap, layer: &superzej_core::config::KeybindConfig) {
    for (id, chord) in layer.iter() {
        apply_override(map, None, id, chord, None);
    }
    for (id, chord) in &layer.vim_normal {
        apply_override(map, Some(Mode::VimNormal), id, chord, None);
    }
    for (id, chord) in &layer.vim_insert {
        apply_override(map, Some(Mode::VimInsert), id, chord, None);
    }
    for (id, chord) in &layer.emacs {
        apply_override(map, Some(Mode::Emacs), id, chord, None);
    }
}

/// The native-host mode a config implies on startup: the active profile's
/// `default_mode`, else `Normal`. Built-in `vim`/`emacs` profile names map to
/// their mode even without an explicit `default_mode`.
pub fn startup_mode(cfg: &superzej_core::config::Config) -> Mode {
    if let Some(p) = cfg.active_profile()
        && let Some(m) = parse_mode(&p.default_mode)
    {
        return m;
    }
    match cfg.profile.as_str() {
        "vim" => Mode::VimNormal,
        "emacs" => Mode::Emacs,
        _ => Mode::Normal,
    }
}

fn parse_mode(s: &str) -> Option<Mode> {
    match s.trim().to_ascii_lowercase().replace('_', "-").as_str() {
        "" => None,
        "normal" => Some(Mode::Normal),
        "vim" | "vim-normal" => Some(Mode::VimNormal),
        "vim-insert" => Some(Mode::VimInsert),
        "emacs" => Some(Mode::Emacs),
        other => {
            superzej_core::msg::warn(&format!("profile: unknown default_mode {other:?}; ignored"));
            None
        }
    }
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
    } else if let Some(a) = Action::from_key(id) {
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
        map.remove(mode, &action);
        map.insert_keys(mode, parsed, action);
    } else {
        map.remove_all(&action);
        for mode in ALL_MODES {
            map.insert_keys(mode, parsed.clone(), action.clone());
        }
    }
}

#[allow(dead_code)]
pub fn map_key(key: &KeyCode, mods: Modifiers) -> Option<Action> {
    match default_keymap().dispatch(Mode::Normal, Key::modified(*key, mods)) {
        MatchResult::Matched(a) => Some(a),
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
        assert_eq!(k(' ', Modifiers::CTRL), Some(Action::OpenPalette));
        assert_eq!(k('g', Modifiers::CTRL), Some(Action::ToggleKeyLock));
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
        // Ctrl owns focus: arrows and hjkl both move spatially.
        assert_eq!(k('h', Modifiers::CTRL), Some(Action::FocusLeft));
        assert_eq!(k('j', Modifiers::CTRL), Some(Action::FocusDown));
        assert_eq!(k('k', Modifiers::CTRL), Some(Action::FocusUp));
        assert_eq!(k('l', Modifiers::CTRL), Some(Action::FocusRight));
        assert_eq!(
            map_key(&KeyCode::LeftArrow, Modifiers::CTRL),
            Some(Action::FocusLeft)
        );
        assert_eq!(
            map_key(&KeyCode::UpArrow, Modifiers::CTRL),
            Some(Action::FocusUp)
        );
        // Alt owns tabs (within the worktree) and worktrees (vertical).
        assert_eq!(
            map_key(&KeyCode::LeftArrow, Modifiers::ALT),
            Some(Action::PrevTab)
        );
        assert_eq!(
            map_key(&KeyCode::RightArrow, Modifiers::ALT),
            Some(Action::NextTab)
        );
        assert_eq!(
            map_key(&KeyCode::UpArrow, Modifiers::ALT),
            Some(Action::PrevWorktree)
        );
        assert_eq!(
            map_key(&KeyCode::DownArrow, Modifiers::ALT),
            Some(Action::NextWorktree)
        );
        // Alt+hjkl no longer claims focus moves (forwards to the pane).
        assert_eq!(k('h', Modifiers::ALT), None);
        assert_eq!(k('l', Modifiers::ALT), None);
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
        assert_eq!(Action::SwitchFont.key(), "switch-font");
    }

    #[test]
    fn switch_font_is_registered_and_bound() {
        assert_eq!(Action::from_key("switch-font"), Some(Action::SwitchFont));
        assert_eq!(Action::from_key("font"), Some(Action::SwitchFont));
        assert_eq!(
            action_spec("switch-font").unwrap().default_chords,
            &["Alt f", "Alt F"]
        );
        assert_eq!(k('f', Modifiers::ALT), Some(Action::SwitchFont));
        assert_eq!(k('F', Modifiers::ALT), Some(Action::SwitchFont));
    }

    #[test]
    fn close_tab_is_lowercase_alt_x_and_close_worktree_is_shift_alt_x() {
        assert_eq!(action_spec("close-tab").unwrap().default_chords, &["Alt x"]);
        assert_eq!(
            action_spec("close-worktree").unwrap().default_chords,
            &["Alt X"]
        );
        assert_eq!(k('x', Modifiers::ALT), Some(Action::CloseTab));
        assert_eq!(k('X', Modifiers::ALT), Some(Action::CloseWorktree));
    }

    #[test]
    fn action_registry_default_chords_are_unique() {
        let mut seen = std::collections::BTreeMap::<&str, &str>::new();
        for spec in action_specs() {
            for chord in spec.default_chords {
                if let Some(prev) = seen.insert(chord, spec.id) {
                    panic!(
                        "default chord {chord} is registered to both {prev} and {}",
                        spec.id
                    );
                }
            }
        }
    }

    #[test]
    fn action_registry_ids_resolve_to_actions() {
        for spec in action_specs() {
            assert!(
                Action::from_key(spec.id).is_some(),
                "registered palette action {} must dispatch",
                spec.id
            );
        }
    }

    #[test]
    fn mode_actions_have_stable_keys() {
        assert_eq!(Action::SwitchMode(Mode::Normal).key(), "mode-normal");
        assert_eq!(Action::SwitchMode(Mode::VimNormal).key(), "mode-vim-normal");
        assert_eq!(Action::SwitchMode(Mode::VimInsert).key(), "mode-vim-insert");
        assert_eq!(Action::SwitchMode(Mode::Emacs).key(), "mode-emacs");
    }

    #[test]
    fn pin_actions_round_trip_through_keys() {
        assert_eq!(Action::ToggleStrip.key(), "toggle-strip");
        assert_eq!(Action::from_key("toggle-strip"), Some(Action::ToggleStrip));
        assert_eq!(Action::from_key("grow-strip"), Some(Action::GrowStrip));
        assert_eq!(Action::from_key("shrink-strip"), Some(Action::ShrinkStrip));
        assert_eq!(Action::from_key("promote-pin"), Some(Action::PromotePin));
        assert_eq!(Action::from_key("unpin"), Some(Action::Unpin));
        // SummonPin parses both `summon-pin-N` and `pin-N`, 1..=9 only.
        assert_eq!(Action::from_key("summon-pin-3"), Some(Action::SummonPin(3)));
        assert_eq!(Action::from_key("pin-1"), Some(Action::SummonPin(1)));
        assert_eq!(Action::from_key("pin-9"), Some(Action::SummonPin(9)));
        assert_eq!(Action::from_key("pin-0"), None);
        assert_eq!(Action::from_key("pin-99"), None);
    }

    #[test]
    fn default_keymap_binds_alt_digits_to_summon_pin() {
        let mut map = default_keymap();
        assert_eq!(
            map.dispatch(
                Mode::Normal,
                Key::modified(KeyCode::Char('1'), Modifiers::ALT)
            ),
            MatchResult::Matched(Action::SummonPin(1))
        );
        assert_eq!(
            map.dispatch(
                Mode::Normal,
                Key::modified(KeyCode::Char('9'), Modifiers::ALT)
            ),
            MatchResult::Matched(Action::SummonPin(9))
        );
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
    fn startup_mode_follows_profile() {
        let mut cfg = superzej_core::config::Config::default();
        assert_eq!(startup_mode(&cfg), Mode::Normal);
        cfg.profile = "vim".into();
        assert_eq!(startup_mode(&cfg), Mode::VimNormal);
        cfg.profile = "emacs".into();
        assert_eq!(startup_mode(&cfg), Mode::Emacs);
    }

    #[test]
    fn profile_default_mode_overrides_name_heuristic() {
        let mut profiles = std::collections::BTreeMap::new();
        profiles.insert(
            "custom".to_string(),
            superzej_core::config::ProfileConfig {
                default_mode: "emacs".into(),
                keybinds: Default::default(),
            },
        );
        let cfg = superzej_core::config::Config {
            profile: "custom".into(),
            profiles,
            ..Default::default()
        };
        assert_eq!(startup_mode(&cfg), Mode::Emacs);
    }

    #[test]
    fn global_keybind_beats_profile_layer() {
        // profile binds focus-down → j; global rebinds it → Ctrl j. Global wins.
        let mut prof = superzej_core::config::ProfileConfig::default();
        prof.keybinds.insert("focus-down".into(), "j".into());
        let mut profiles = std::collections::BTreeMap::new();
        profiles.insert("vim".to_string(), prof);
        let mut cfg = superzej_core::config::Config {
            profile: "vim".into(),
            profiles,
            ..Default::default()
        };
        cfg.keybinds.insert("focus-down".into(), "Ctrl j".into());

        let mut map = default_keymap_for(&cfg, None, None);
        assert_eq!(
            map.dispatch(Mode::Normal, Key::ctrl('j')),
            MatchResult::Matched(Action::FocusDown)
        );
        // The profile's plain `j` lost to the global override.
        assert_eq!(
            map.dispatch(Mode::Normal, Key::char('j')),
            MatchResult::None
        );
    }

    #[test]
    fn workspace_layer_beats_global() {
        let mut cfg = superzej_core::config::Config::default();
        cfg.keybinds.insert("focus-down".into(), "Ctrl j".into());
        let mut ws = superzej_core::config::WorkspaceConfig::default();
        ws.keybinds.insert("focus-down".into(), "Alt j".into());
        cfg.workspace.insert("myrepo".into(), ws);

        let mut map = default_keymap_for(&cfg, None, Some("myrepo"));
        assert_eq!(
            map.dispatch(
                Mode::Normal,
                Key::modified(KeyCode::Char('j'), Modifiers::ALT)
            ),
            MatchResult::Matched(Action::FocusDown)
        );
        assert_eq!(
            map.dispatch(Mode::Normal, Key::ctrl('j')),
            MatchResult::None
        );
    }

    #[test]
    fn program_overlay_binds_action_for_focused_program() {
        let mut cfg = superzej_core::config::Config::default();
        let mut binds = superzej_core::config::KeybindConfig::default();
        binds.insert("palette".into(), "Ctrl Alt k".into());
        cfg.program_keybinds.insert("lazygit".into(), binds);

        let map = default_keymap_for(&cfg, None, None);
        let key = Key::modified(KeyCode::Char('k'), Modifiers::CTRL | Modifiers::ALT);
        assert_eq!(
            map.program_action("lazygit", &key),
            Some(Action::OpenPalette)
        );
        // A different focused program does not see lazygit's overlay.
        assert_eq!(map.program_action("yazi", &key), None);
        assert_eq!(map.program_action("", &key), None);
    }

    #[test]
    fn program_remap_rewrites_unclaimed_key() {
        let mut cfg = superzej_core::config::Config::default();
        let mut remap = std::collections::BTreeMap::new();
        remap.insert("Ctrl j".into(), "Enter".into());
        cfg.program_remap.insert("lazygit".into(), remap);

        let map = default_keymap_for(&cfg, None, None);
        let src = Key::ctrl('j');
        let dst = map.program_remap("lazygit", &src).expect("remap present");
        assert_eq!(dst, &[Key::from_code(KeyCode::Enter)]);
        assert!(map.program_remap("yazi", &src).is_none());
    }

    #[test]
    fn program_overlay_rejects_multi_key_sequence() {
        // Per-program overlays are single-chord only; a sequence is dropped.
        let mut cfg = superzej_core::config::Config::default();
        let mut binds = superzej_core::config::KeybindConfig::default();
        binds.insert("palette".into(), "g g".into());
        cfg.program_keybinds.insert("lazygit".into(), binds);

        let map = default_keymap_for(&cfg, None, None);
        assert_eq!(map.program_action("lazygit", &Key::char('g')), None);
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
}
