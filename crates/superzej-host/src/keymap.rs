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
    DeleteWorkspace,
    NewTerminal,
    NewTab,
    /// Zellij-style smart split: along the focused pane's longer dimension.
    NewPane,
    /// Fullscreen the focused zone (pane / sidebar / panel); toggles off.
    ToggleZoom,
    /// Broadcast typed input to every pane in the focused tab (item 96); toggles off.
    ToggleSyncPanes,
    /// Save the focused tab's pane layout as a named snapshot (item 115).
    SaveLayout,
    /// Apply a saved named layout to the focused tab (item 115).
    ApplyLayout,
    /// Export the focused tab's layout to a JSON file (item 99).
    ExportLayout,
    /// Import a layout from a JSON file into the focused tab (item 99).
    ImportLayout,
    /// Create a worktree from a saved `[[worktree_templates]]` preset (item 54).
    NewWorktreeFromTemplate,
    /// Cycle through the named theme presets (storm → light → abyss → …).
    CycleTheme,
    /// Pick a font family from fontconfig and patch the live alacritty profile.
    SwitchFont,
    /// Close the active tab within the worktree. The final tab is kept; use
    /// CloseWorktree for the explicit worktree-removal action.
    CloseTab,
    CloseWorktree,
    SwitchWorkspace,
    /// Open the coding-agent account switcher for the focused worktree (item 656).
    SwitchAccount,
    NextTab,
    PrevTab,
    /// Switch to the next worktree (Alt+Down), wrapping WITHIN the active
    /// worktree's workspace only; restores its active tab.
    NextWorktree,
    /// Switch to the previous worktree (Alt+Up), wrapping within the workspace.
    PrevWorktree,
    /// Switch to the next workspace (Shift+Alt+Down) — a real context switch.
    NextWorkspace,
    /// Switch to the previous workspace (Shift+Alt+Up).
    PrevWorkspace,
    /// Reorder the selected item up (Ctrl+Alt+Up): the cursor workspace if the
    /// sidebar is focused on one, else the active worktree within its workspace.
    MoveItemUp,
    /// Reorder the selected item down (Ctrl+Alt+Down).
    MoveItemDown,
    SplitDown,
    SplitRight,
    CloseSplitPane,
    FocusLeft,
    FocusRight,
    FocusUp,
    FocusDown,
    ToggleSidebar,
    TogglePanel,
    ToggleRecorder,
    ToggleDrawer,
    /// Move keyboard focus into the sidebar tree (shows it if hidden).
    FocusSidebar,
    /// Move keyboard focus into the right panel (shows it if hidden).
    FocusPanel,
    /// Open the right panel to the System ▸ Notifications section and focus it;
    /// pressing it again while already there returns focus to the center.
    ToggleNotifications,
    /// Open the right panel to the Work ▸ CI section and focus it (AV group).
    OpenCi,
    OpenPalette,
    Lazygit,
    Yazi,
    Editor,
    Diff,
    /// Push the current branch to its upstream — fast-path, no branches-panel
    /// navigation (item 605). Offers `push -u` when there is no upstream.
    Push,
    /// Pull the current branch from its upstream (item 605).
    Pull,
    /// Fetch all remotes with prune (item 605).
    Fetch,
    /// Open the rollback/discard window for the active worktree (item 604).
    Rollback,
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
        id: "close-pane",
        label: "Close pane",
        hint: "close pane",
        default_chords: &["Ctrl w"],
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
        id: "sync-panes",
        label: "Toggle sync-panes (broadcast input)",
        hint: "sync",
        default_chords: &["Ctrl Alt y"],
        palette: true,
    },
    ActionSpec {
        id: "save-layout",
        label: "Save layout as…",
        hint: "save layout",
        default_chords: &[],
        palette: true,
    },
    ActionSpec {
        id: "apply-layout",
        label: "Apply saved layout…",
        hint: "apply layout",
        default_chords: &[],
        palette: true,
    },
    ActionSpec {
        id: "export-layout",
        label: "Export layout to file…",
        hint: "export layout",
        default_chords: &[],
        palette: true,
    },
    ActionSpec {
        id: "import-layout",
        label: "Import layout from file…",
        hint: "import layout",
        default_chords: &[],
        palette: true,
    },
    ActionSpec {
        id: "new-worktree-from-template",
        label: "New worktree from template…",
        hint: "template",
        default_chords: &[],
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
        id: "switch-account",
        label: "Switch agent account",
        hint: "account",
        default_chords: &["Ctrl Alt a"],
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
        id: "prev-workspace",
        label: "Previous workspace",
        hint: "prev ws",
        default_chords: &["Shift Alt Up"],
        palette: true,
    },
    ActionSpec {
        id: "next-workspace",
        label: "Next workspace",
        hint: "next ws",
        default_chords: &["Shift Alt Down"],
        palette: true,
    },
    ActionSpec {
        id: "move-item-up",
        label: "Move up (workspace/worktree)",
        hint: "move↑",
        default_chords: &["Ctrl Alt Up"],
        palette: true,
    },
    ActionSpec {
        id: "move-item-down",
        label: "Move down (workspace/worktree)",
        hint: "move↓",
        default_chords: &["Ctrl Alt Down"],
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
        id: "toggle-notifications",
        label: "Toggle Notifications panel",
        hint: "notifications",
        default_chords: &["Alt i"],
        palette: true,
    },
    ActionSpec {
        id: "open-ci",
        label: "Open CI/CD runs panel",
        hint: "ci",
        default_chords: &[],
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
        id: "git-push",
        label: "Git push (current branch)",
        hint: "push",
        default_chords: &[],
        palette: true,
    },
    ActionSpec {
        id: "git-pull",
        label: "Git pull (current branch)",
        hint: "pull",
        default_chords: &[],
        palette: true,
    },
    ActionSpec {
        id: "git-fetch",
        label: "Git fetch (all remotes, prune)",
        hint: "fetch",
        default_chords: &[],
        palette: true,
    },
    ActionSpec {
        id: "rollback",
        label: "Rollback / discard changes…",
        hint: "rollback",
        default_chords: &[],
        palette: true,
    },
    ActionSpec {
        id: "scroll-up",
        label: "Scroll pane up",
        hint: "scroll↑",
        default_chords: &["Shift PageUp", "PageUp"],
        palette: true,
    },
    ActionSpec {
        id: "scroll-down",
        label: "Scroll pane down",
        hint: "scroll↓",
        default_chords: &["Shift PageDown", "PageDown"],
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

/// The scope in which a keybinding applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[allow(dead_code)]
pub enum BindingScope {
    /// Fires in any focus zone.
    Global,
    /// Fires only while the right panel has keyboard focus.
    PanelFocused,
    /// Fires only while the workspace sidebar has keyboard focus.
    SidebarFocused,
}

/// A keybinding registered at a specific scope. Used for conflict detection
/// and for documenting intentional global→scoped overrides.
#[derive(Debug, Clone, Copy)]
pub struct ScopedBinding {
    /// The chord string in the same format as `ActionSpec::default_chords`
    /// (e.g. `"Alt 1"`).
    pub chord: &'static str,
    pub scope: BindingScope,
    /// Human description for diagnostics ("panel tab → git").
    pub label: &'static str,
}

/// Panel-scoped bindings that intentionally shadow global defaults. Documented
/// here so conflict detection can distinguish deliberate overrides from bugs,
/// and so future plugin keybind registration can validate against them.
pub const PANEL_SCOPE_BINDINGS: &[ScopedBinding] = &[
    ScopedBinding {
        chord: "Alt 1",
        scope: BindingScope::PanelFocused,
        label: "panel tab → git",
    },
    ScopedBinding {
        chord: "Alt 2",
        scope: BindingScope::PanelFocused,
        label: "panel tab → work",
    },
    ScopedBinding {
        chord: "Alt 3",
        scope: BindingScope::PanelFocused,
        label: "panel tab → system",
    },
];

/// Check for keybinding conflicts.
///
/// **True conflict** (returned as `Err` strings): two bindings at the *same*
/// scope claim the same chord — the result would be ambiguous.
///
/// **Intentional override** (logged at debug, not returned): a `PanelFocused`
/// binding shadows a `Global` one. This is expected and by design; the narrower
/// scope wins. Callers can pass `PANEL_SCOPE_BINDINGS` + `ACTION_SPECS` to
/// surface these as diagnostics without treating them as errors.
pub fn check_binding_conflicts(
    scoped: &[ScopedBinding],
    global_specs: &[ActionSpec],
) -> Vec<String> {
    let mut errors: Vec<String> = Vec::new();

    // Within-scope duplicates are always errors.
    let mut seen: std::collections::HashMap<(&str, BindingScope), &str> =
        std::collections::HashMap::new();
    for b in scoped {
        if let Some(prev) = seen.insert((b.chord, b.scope), b.label) {
            errors.push(format!(
                "chord {:?} is bound twice within scope {:?}: {:?} vs {:?}",
                b.chord, b.scope, prev, b.label
            ));
        }
    }

    // Cross-scope (Global vs PanelFocused) overlaps are intentional overrides.
    // Log them at debug so they're visible during development but not noisy.
    for b in scoped.iter().filter(|b| b.scope != BindingScope::Global) {
        for spec in global_specs {
            if spec.default_chords.contains(&b.chord) {
                tracing::debug!(
                    chord = b.chord,
                    panel_binding = b.label,
                    global_action = spec.id,
                    "panel binding intentionally shadows global action"
                );
            }
        }
    }

    errors
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
            Action::DeleteWorkspace => "delete-workspace",
            Action::NewTerminal => "new-terminal",
            Action::NewTab => "new-tab",
            Action::NewPane => "new-pane",
            Action::ToggleZoom => "zoom",
            Action::ToggleSyncPanes => "sync-panes",
            Action::SaveLayout => "save-layout",
            Action::ApplyLayout => "apply-layout",
            Action::ExportLayout => "export-layout",
            Action::ImportLayout => "import-layout",
            Action::NewWorktreeFromTemplate => "new-worktree-from-template",
            Action::CycleTheme => "cycle-theme",
            Action::SwitchFont => "switch-font",
            Action::CloseTab => "close-tab",
            Action::CloseWorktree => "close-worktree",
            Action::SwitchWorkspace => "switch-workspace",
            Action::SwitchAccount => "switch-account",
            Action::NextTab => "next-tab",
            Action::PrevTab => "prev-tab",
            Action::NextWorktree => "next-worktree",
            Action::PrevWorktree => "prev-worktree",
            Action::NextWorkspace => "next-workspace",
            Action::PrevWorkspace => "prev-workspace",
            Action::MoveItemUp => "move-item-up",
            Action::MoveItemDown => "move-item-down",
            Action::SplitDown => "split-down",
            Action::SplitRight => "split-right",
            Action::CloseSplitPane => "close-pane",
            Action::FocusLeft => "focus-left",
            Action::FocusRight => "focus-right",
            Action::FocusUp => "focus-up",
            Action::FocusDown => "focus-down",
            Action::ToggleSidebar => "toggle-sidebar",
            Action::TogglePanel => "toggle-panel",
            Action::ToggleRecorder => "toggle-recorder",
            Action::ToggleDrawer => "files-drawer",
            Action::FocusSidebar => "focus-sidebar",
            Action::FocusPanel => "focus-panel",
            Action::OpenCi => "open-ci",
            Action::ToggleNotifications => "toggle-notifications",
            Action::OpenPalette => "palette",
            Action::Lazygit => "lazygit",
            Action::Yazi => "yazi",
            Action::Editor => "editor",
            Action::Diff => "show-diff",
            Action::Push => "git-push",
            Action::Pull => "git-pull",
            Action::Fetch => "git-fetch",
            Action::Rollback => "rollback",
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
            "delete-workspace" => Action::DeleteWorkspace,
            "new-terminal" => Action::NewTerminal,
            "new-tab" => Action::NewTab,
            "new-pane" => Action::NewPane,
            "zoom" | "toggle-zoom" | "fullscreen" => Action::ToggleZoom,
            "sync-panes" | "toggle-sync-panes" | "broadcast" => Action::ToggleSyncPanes,
            "save-layout" => Action::SaveLayout,
            "apply-layout" => Action::ApplyLayout,
            "export-layout" => Action::ExportLayout,
            "import-layout" => Action::ImportLayout,
            "new-worktree-from-template" | "worktree-template" => Action::NewWorktreeFromTemplate,
            "cycle-theme" | "theme" => Action::CycleTheme,
            "switch-font" | "font" => Action::SwitchFont,
            "close-tab" => Action::CloseTab,
            "close-worktree" => Action::CloseWorktree,
            "switch-workspace" | "switch-repo" => Action::SwitchWorkspace,
            "switch-account" => Action::SwitchAccount,
            "next-tab" => Action::NextTab,
            "prev-tab" => Action::PrevTab,
            "next-worktree" => Action::NextWorktree,
            "prev-worktree" => Action::PrevWorktree,
            "next-workspace" => Action::NextWorkspace,
            "prev-workspace" => Action::PrevWorkspace,
            "move-item-up" | "move-worktree-up" => Action::MoveItemUp,
            "move-item-down" | "move-worktree-down" => Action::MoveItemDown,
            "split-down" | "new-panel-native" => Action::SplitDown,
            "split-right" | "new-panel" => Action::SplitRight,
            "close-pane" => Action::CloseSplitPane,
            "focus-left" => Action::FocusLeft,
            "focus-right" => Action::FocusRight,
            "focus-up" => Action::FocusUp,
            "focus-down" => Action::FocusDown,
            "toggle-sidebar" => Action::ToggleSidebar,
            "toggle-panel" => Action::TogglePanel,
            "toggle-recorder" => Action::ToggleRecorder,
            "files" | "files-drawer" | "toggle-drawer" => Action::ToggleDrawer,
            "focus-sidebar" => Action::FocusSidebar,
            "focus-panel" => Action::FocusPanel,
            "open-ci" => Action::OpenCi,
            "toggle-notifications" => Action::ToggleNotifications,
            "palette" | "menu" => Action::OpenPalette,
            "lazygit" | "tool-lazygit" => Action::Lazygit,
            "yazi" | "tool-yazi" => Action::Yazi,
            "editor" | "tool-editor" => Action::Editor,
            "show-diff" | "diff" | "tool-diff" => Action::Diff,
            "git-push" | "push" => Action::Push,
            "git-pull" | "pull" => Action::Pull,
            "git-fetch" | "fetch" => Action::Fetch,
            "rollback" | "discard-window" => Action::Rollback,
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

/// How a `[[actions]]` name is resolved when creating a worktree composite.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NameSpec {
    /// Generate a random adjective-noun name (param absent or `"random"`).
    Random,
    /// Use this literal tail (the configured branch prefix is prepended).
    Fixed(String),
}

/// Where a `new-pane` composite splits the focused pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanePlacement {
    /// Split below (`placement = "down"`).
    Down,
    /// Split to the right (`placement = "right"`, the default).
    Right,
}

/// The working directory a `new-pane` composite spawns in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneCwd {
    /// The active worktree's directory.
    Worktree,
    /// The active pane's cwd (the default).
    Active,
}

/// A built-in composite operation bound to a custom keybind
/// (`[[actions]] action = "…"`). Parsed + validated at keymap build time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompositeAction {
    /// Create a worktree non-interactively (the wizard, headless).
    NewWorktree {
        name: NameSpec,
        /// Sandbox backend key (`bwrap`, `auto`, …); `None` = configured default.
        sandbox: Option<String>,
        /// Agent/tool choice (`shell`, an agent name, …); `None` = `shell`.
        agent: Option<String>,
        /// Source ref to branch from; `None` = configured/auto base.
        base: Option<String>,
    },
    /// Spawn a pane in the active tab, optionally running a command.
    NewPane {
        /// Command line run via the host shell; `None` = an interactive shell.
        run: Option<String>,
        placement: PanePlacement,
        cwd: PaneCwd,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostCustomAction {
    /// A shell command (`[[actions]] run = "…"`).
    Shell {
        name: String,
        run: String,
        floating: bool,
        close_on_exit: bool,
    },
    /// A built-in composite operation (`[[actions]] action = "…"`).
    Composite {
        name: String,
        action: CompositeAction,
    },
}

impl HostCustomAction {
    /// The action's stable id / menu label.
    pub fn name(&self) -> &str {
        match self {
            HostCustomAction::Shell { name, .. } | HostCustomAction::Composite { name, .. } => name,
        }
    }
}

/// Parse + validate a `[[actions]] action`/`params` entry into a typed
/// composite. Warns and returns `None` (the action is skipped) on an unknown
/// action name or an invalid parameter value.
fn parse_composite(
    cfg: &superzej_core::config::Config,
    name: &str,
    action: &str,
    params: &std::collections::BTreeMap<String, String>,
) -> Option<CompositeAction> {
    let warn = |msg: &str| superzej_core::msg::warn(&format!("[[actions]] {name}: {msg}; skipped"));
    let warn_unknown = |allowed: &[&str]| {
        for key in params.keys() {
            if !allowed.contains(&key.as_str()) {
                superzej_core::msg::warn(&format!(
                    "[[actions]] {name}: unknown param {key:?} for action {action:?}; ignored"
                ));
            }
        }
    };
    match action {
        "new-worktree" => {
            warn_unknown(&["name", "sandbox", "agent", "base"]);
            let name_spec = match params.get("name").map(String::as_str) {
                None | Some("") | Some("random") => NameSpec::Random,
                Some(tail) => NameSpec::Fixed(tail.to_string()),
            };
            let sandbox = match params.get("sandbox") {
                Some(s) => {
                    let valid: Vec<String> = crate::palette::build_sandbox_palette(cfg)
                        .into_iter()
                        .filter_map(|i| i.key.strip_prefix("sandbox:").map(|k| k.to_string()))
                        .collect();
                    if !valid.iter().any(|v| v == s) {
                        warn(&format!(
                            "unknown sandbox {s:?} (expected one of {})",
                            valid.join(", ")
                        ));
                        return None;
                    }
                    Some(s.clone())
                }
                None => None,
            };
            let agent = match params.get("agent") {
                Some(a) => {
                    let valid = crate::agent::choices(cfg);
                    if !valid.iter().any(|v| v == a) {
                        warn(&format!(
                            "unknown agent {a:?} (expected one of {})",
                            valid.join(", ")
                        ));
                        return None;
                    }
                    Some(a.clone())
                }
                None => None,
            };
            let base = params.get("base").filter(|b| !b.is_empty()).cloned();
            Some(CompositeAction::NewWorktree {
                name: name_spec,
                sandbox,
                agent,
                base,
            })
        }
        "new-pane" => {
            warn_unknown(&["run", "placement", "cwd"]);
            let run = params.get("run").filter(|r| !r.is_empty()).cloned();
            let placement = match params.get("placement").map(String::as_str) {
                None | Some("right") => PanePlacement::Right,
                Some("down") => PanePlacement::Down,
                Some(other) => {
                    warn(&format!(
                        "unknown placement {other:?} (expected down|right)"
                    ));
                    return None;
                }
            };
            let cwd = match params.get("cwd").map(String::as_str) {
                None | Some("active") => PaneCwd::Active,
                Some("worktree") => PaneCwd::Worktree,
                Some(other) => {
                    warn(&format!("unknown cwd {other:?} (expected worktree|active)"));
                    return None;
                }
            };
            Some(CompositeAction::NewPane {
                run,
                placement,
                cwd,
            })
        }
        other => {
            warn(&format!(
                "unknown action {other:?} (expected new-worktree|new-pane)"
            ));
            None
        }
    }
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
    map.insert_all("Ctrl Alt r", Action::ToggleRecorder)
        .unwrap();
    map.insert_all("Ctrl Alt f", Action::ToggleDrawer).unwrap();
    map.insert_all("Alt s", Action::FocusSidebar).unwrap();
    map.insert_all("Alt .", Action::FocusPanel).unwrap();
    map.insert_all("Alt i", Action::ToggleNotifications)
        .unwrap();
    map.insert_all("Ctrl Alt c", Action::CopyPane).unwrap();
    map.insert_all("Ctrl Shift c", Action::CopyPane).unwrap();
    map.insert_all("Ctrl Alt n", Action::SwitchMode(Mode::Normal))
        .unwrap();
    map.insert_all("Ctrl Alt v", Action::SwitchMode(Mode::VimNormal))
        .unwrap();
    map.insert_all("Ctrl Alt e", Action::SwitchMode(Mode::Emacs))
        .unwrap();

    map.insert_all("Alt w", Action::NewWorktree).unwrap();
    map.insert_all("Alt W", Action::NewWorkspace).unwrap();
    map.insert_all("Alt Shift X", Action::DeleteWorkspace)
        .unwrap();
    map.insert_all("Alt T", Action::NewTerminal).unwrap();
    map.insert_all("Alt t", Action::NewTab).unwrap();
    map.insert_all("Alt p", Action::NewPane).unwrap();
    map.insert_all("Ctrl Alt z", Action::ToggleZoom).unwrap();
    map.insert_all("Ctrl Alt y", Action::ToggleSyncPanes)
        .unwrap();
    map.insert_all("Ctrl Alt t", Action::CycleTheme).unwrap();
    map.insert_all("Alt f", Action::SwitchFont).unwrap();
    map.insert_all("Alt F", Action::SwitchFont).unwrap();
    map.insert_all("Alt x", Action::CloseTab).unwrap();
    map.insert_all("Alt X", Action::CloseWorktree).unwrap();
    map.insert_all("Alt o", Action::SwitchWorkspace).unwrap();
    map.insert_all("Alt n", Action::SplitDown).unwrap();
    map.insert_all("Alt N", Action::SplitRight).unwrap();
    map.insert_all("Ctrl w", Action::CloseSplitPane).unwrap();
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
    // ↑/↓ navigates worktrees, wrapping WITHIN the current workspace only.
    map.insert_all("Alt Left", Action::PrevTab).unwrap();
    map.insert_all("Alt Right", Action::NextTab).unwrap();
    map.insert_all("Alt Up", Action::PrevWorktree).unwrap();
    map.insert_all("Alt Down", Action::NextWorktree).unwrap();
    // Shift+Alt+↑/↓ switches workspace (a real context switch).
    map.insert_all("Shift Alt Up", Action::PrevWorkspace)
        .unwrap();
    map.insert_all("Shift Alt Down", Action::NextWorkspace)
        .unwrap();
    // Ctrl+Alt+↑/↓ reorders the selected item: the workspace under the sidebar
    // cursor if the sidebar is focused, else the active worktree.
    map.insert_all("Ctrl Alt Up", Action::MoveItemUp).unwrap();
    map.insert_all("Ctrl Alt Down", Action::MoveItemDown)
        .unwrap();

    map.insert_all("Shift PageUp", Action::ScrollUp).unwrap();
    map.insert_all("PageUp", Action::ScrollUp).unwrap();
    map.insert_all("Shift PageDown", Action::ScrollDown)
        .unwrap();
    map.insert_all("PageDown", Action::ScrollDown).unwrap();

    // Single key keybinds are prevented by rule. We shouldn't use "/" for SearchPane.
    map.insert_all("Ctrl Alt /", Action::SearchPane).unwrap();
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
    map.insert(Mode::VimNormal, "Space X", Action::DeleteWorkspace)
        .unwrap();
    map.insert(Mode::VimNormal, "Space T", Action::NewTerminal)
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

/// Overlay an IDE keymap preset (item 621) onto `map`: a small set of familiar
/// chords mapped to existing host actions, applied after the built-in defaults
/// and before user `[keybinds]` so user overrides still win. Chords are chosen
/// to avoid the core nav defaults (`Ctrl h/j/k/l`, `Ctrl w`). `"default"` (and
/// the Vim/Emacs modes, which live in `default_keymap`) are a no-op. A bad chord
/// is skipped, never fatal.
pub fn apply_keymap_preset(map: &mut KeyMap, preset: &str) {
    let binds: &[(&str, Action)] = match preset.trim().to_ascii_lowercase().as_str() {
        "vscode" | "vs-code" | "code" => &[
            ("Ctrl p", Action::SearchGlobal),       // quick open / go to file
            ("Ctrl Shift p", Action::OpenPalette),  // command palette
            ("Ctrl b", Action::ToggleSidebar),      // toggle side bar
            ("Ctrl Shift e", Action::FocusSidebar), // explorer focus
            ("Ctrl Shift g", Action::FocusPanel),   // source-control panel
        ],
        "jetbrains" | "intellij" | "idea" => &[
            ("Ctrl Shift a", Action::OpenPalette),  // Find Action
            ("Ctrl e", Action::SearchGlobal),       // Recent Files
            ("Ctrl Shift f", Action::SearchGlobal), // Find in Path
            ("Alt 1", Action::ToggleSidebar),       // Project tool window
        ],
        _ => return,
    };
    for (chord, action) in binds {
        if let Err(e) = map.insert_all(chord, action.clone()) {
            superzej_core::msg::warn(&format!(
                "keymap_preset {preset}: chord {chord:?}: {e}; skipped"
            ));
        }
    }
}

/// Build the host keymap for a focused context: the built-in defaults, custom
/// `[[actions]]`, then each keybind layer from
/// [`Config::effective_keybinds`](superzej_core::config::Config::effective_keybinds)
/// applied lowest-precedence-first (profile → global → workspace → repo-root).
/// `repo_root`/`slug` are `None` outside a workspace (e.g. the home tab).
pub fn default_keymap_for(
    cfg: &superzej_core::config::Config,
    repo_root: Option<&std::path::Path>,
    slug: Option<&str>,
) -> KeyMap {
    let mut map = default_keymap();
    apply_keymap_preset(&mut map, &cfg.keymap_preset);
    map.config = cfg.clone();

    for action in &cfg.actions {
        // Exactly one of `run` (shell) / `action` (built-in composite); skip
        // (and warn) otherwise. Skipping before the push keeps `custom_actions`
        // indices aligned with the `Action::Custom(idx)` overrides.
        let ca = match (&action.run, &action.action) {
            (Some(run), None) => HostCustomAction::Shell {
                name: action.name.clone(),
                run: run.clone(),
                floating: action.floating,
                close_on_exit: action.close_on_exit,
            },
            (None, Some(act)) => match parse_composite(cfg, &action.name, act, &action.params) {
                Some(composite) => HostCustomAction::Composite {
                    name: action.name.clone(),
                    action: composite,
                },
                None => continue, // parse_composite already warned
            },
            (Some(_), Some(_)) => {
                superzej_core::msg::warn(&format!(
                    "[[actions]] {}: set exactly one of `run` or `action`; skipped",
                    action.name
                ));
                continue;
            }
            (None, None) => {
                superzej_core::msg::warn(&format!(
                    "[[actions]] {}: needs a `run` command or an `action`; skipped",
                    action.name
                ));
                continue;
            }
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

/// Apply one keybind layer (a [`KeybindConfig`](superzej_core::config::KeybindConfig)) onto `map`: the flat table
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
    fn runtime_dispatch_matches_alt_w_in_normal_mode() {
        // The runtime path is keymap.dispatch(mode, Key), NOT map_key. Prove a
        // parsed Alt+w (what termwiz yields for both legacy ESC-w and kitty
        // CSI 119;3u) actually triggers NewWorktree through the real matcher.
        let mut map = default_keymap();
        let key = Key::modified(KeyCode::Char('w'), Modifiers::ALT);
        assert!(
            matches!(
                map.dispatch(Mode::Normal, key),
                crate::sequence::MatchResult::Matched(Action::NewWorktree)
            ),
            "Alt+w must match NewWorktree via the runtime dispatch path"
        );
    }

    #[test]
    fn alt_chords_map_to_lifecycle_actions() {
        assert_eq!(k('w', Modifiers::ALT), Some(Action::NewWorktree));
        assert_eq!(k('W', Modifiers::ALT), Some(Action::NewWorkspace));
        assert_eq!(k('o', Modifiers::ALT), Some(Action::SwitchWorkspace));
        assert_eq!(k('t', Modifiers::ALT), Some(Action::NewTab));
    }

    #[test]
    fn config_defined_multikey_sequence_routes() {
        // A user binds a two-key leader sequence; the host registers it as a
        // real sequence (Pending on the prefix, Matched on completion).
        let mut cfg = superzej_core::config::Config::default();
        cfg.keybinds.insert("new-worktree".into(), "Space w".into());
        let mut map = default_keymap_for(&cfg, None, None);
        let space = Key::char(' ');
        let w = Key::char('w');
        assert!(
            matches!(
                map.dispatch(Mode::Normal, space.clone()),
                MatchResult::Pending
            ),
            "the leader key is a pending prefix"
        );
        assert_eq!(
            map.dispatch(Mode::Normal, w),
            MatchResult::Matched(Action::NewWorktree),
            "completing the sequence fires the action"
        );
        // which-key has a continuation to show after the prefix.
        map.reset();
        let _ = map.dispatch(Mode::Normal, space);
        assert!(
            map.pending_continuations(Mode::Normal)
                .iter()
                .any(|(_, a)| *a == Action::NewWorktree),
            "the which-key popup lists the sequence continuation"
        );
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
        // Sync-panes broadcast (item 96) on Ctrl-Alt-Y, distinct from zoom.
        assert_eq!(
            k('y', Modifiers::CTRL | Modifiers::ALT),
            Some(Action::ToggleSyncPanes)
        );
        assert_eq!(
            k('z', Modifiers::CTRL | Modifiers::ALT),
            Some(Action::ToggleZoom)
        );
        assert_eq!(Action::from_key("broadcast"), Some(Action::ToggleSyncPanes));
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
    fn workspace_nav_and_reorder_chords() {
        // Shift+Alt+↑/↓ switches workspace; Ctrl+Alt+↑/↓ reorders the item.
        let shift_alt = Modifiers::SHIFT | Modifiers::ALT;
        let ctrl_alt = Modifiers::CTRL | Modifiers::ALT;
        assert_eq!(
            map_key(&KeyCode::UpArrow, shift_alt),
            Some(Action::PrevWorkspace)
        );
        assert_eq!(
            map_key(&KeyCode::DownArrow, shift_alt),
            Some(Action::NextWorkspace)
        );
        assert_eq!(
            map_key(&KeyCode::UpArrow, ctrl_alt),
            Some(Action::MoveItemUp)
        );
        assert_eq!(
            map_key(&KeyCode::DownArrow, ctrl_alt),
            Some(Action::MoveItemDown)
        );
    }

    #[test]
    fn move_item_keeps_legacy_worktree_aliases() {
        // Renamed action; existing configs using the old ids must still bind.
        assert_eq!(Action::from_key("move-item-up"), Some(Action::MoveItemUp));
        assert_eq!(
            Action::from_key("move-worktree-up"),
            Some(Action::MoveItemUp)
        );
        assert_eq!(
            Action::from_key("move-worktree-down"),
            Some(Action::MoveItemDown)
        );
        assert_eq!(
            Action::from_key("next-workspace"),
            Some(Action::NextWorkspace)
        );
        assert_eq!(
            Action::from_key("prev-workspace"),
            Some(Action::PrevWorkspace)
        );
    }

    #[test]
    fn shift_pageup_down_scroll() {
        assert_eq!(
            map_key(&KeyCode::PageUp, Modifiers::SHIFT),
            Some(Action::ScrollUp)
        );
        assert_eq!(
            map_key(&KeyCode::PageDown, Modifiers::SHIFT),
            Some(Action::ScrollDown)
        );
        // Plain PageUp now scrolls the pane (user requested).
        assert_eq!(
            map_key(&KeyCode::PageUp, Modifiers::NONE),
            Some(Action::ScrollUp)
        );
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
    fn git_fastpath_actions_round_trip_and_are_in_the_palette() {
        // Item 605: push/pull/fetch reachable from the palette (no default
        // chord — user-bindable), with stable ids + alias parsing.
        for (action, id, alias) in [
            (Action::Push, "git-push", "push"),
            (Action::Pull, "git-pull", "pull"),
            (Action::Fetch, "git-fetch", "fetch"),
        ] {
            assert_eq!(action.key(), id);
            assert_eq!(Action::from_key(id), Some(action.clone()));
            assert_eq!(Action::from_key(alias), Some(action.clone()));
            let spec = action_spec(id).expect("spec registered");
            assert!(spec.palette, "{id} must appear in the command palette");
            assert!(
                spec.default_chords.is_empty(),
                "{id} ships without a default chord"
            );
        }
    }

    #[test]
    fn toggle_notifications_action_round_trips_and_is_in_the_palette() {
        assert_eq!(Action::ToggleNotifications.key(), "toggle-notifications");
        assert_eq!(
            Action::from_key("toggle-notifications"),
            Some(Action::ToggleNotifications)
        );
        let spec = action_spec("toggle-notifications").expect("spec registered");
        assert!(spec.palette, "must appear in the command palette");
        assert_eq!(spec.default_chords, &["Alt i"]);
        // The default keymap must actually BIND Alt+i (default_chords is only
        // metadata; the binding is a separate explicit insert).
        let mut map = default_keymap();
        assert_eq!(
            map.dispatch(
                Mode::Normal,
                Key::modified(KeyCode::Char('i'), Modifiers::ALT)
            ),
            MatchResult::Matched(Action::ToggleNotifications)
        );
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
    fn keymap_preset_default_leaves_ide_chords_unbound() {
        let cfg = superzej_core::config::Config::default(); // "default"
        let mut map = default_keymap_for(&cfg, None, None);
        // Ctrl+p is not a built-in default — without a preset it stays unbound.
        assert_eq!(
            map.dispatch(Mode::Normal, Key::ctrl('p')),
            MatchResult::None
        );
    }

    #[test]
    fn keymap_preset_vscode_binds_ide_chords() {
        let cfg = superzej_core::config::Config {
            keymap_preset: "vscode".into(),
            ..Default::default()
        };
        let mut map = default_keymap_for(&cfg, None, None);
        assert_eq!(
            map.dispatch(Mode::Normal, Key::ctrl('p')),
            MatchResult::Matched(Action::SearchGlobal)
        );
        assert_eq!(
            map.dispatch(Mode::Normal, Key::ctrl('b')),
            MatchResult::Matched(Action::ToggleSidebar)
        );
        // Core nav defaults the preset deliberately avoids still work.
        assert_eq!(
            map.dispatch(Mode::Normal, Key::ctrl('h')),
            MatchResult::Matched(Action::FocusLeft)
        );
    }

    #[test]
    fn keymap_preset_jetbrains_binds_ide_chords() {
        let cfg = superzej_core::config::Config {
            keymap_preset: "jetbrains".into(),
            ..Default::default()
        };
        let mut map = default_keymap_for(&cfg, None, None);
        assert_eq!(
            map.dispatch(Mode::Normal, Key::ctrl('e')),
            MatchResult::Matched(Action::SearchGlobal)
        );
    }

    #[test]
    fn user_keybind_overrides_preset() {
        let mut cfg = superzej_core::config::Config {
            keymap_preset: "vscode".into(),
            ..Default::default()
        };
        // A user rebind of Ctrl+p must win over the preset's SearchGlobal.
        cfg.keybinds.insert("toggle-panel".into(), "Ctrl p".into());
        let mut map = default_keymap_for(&cfg, None, None);
        assert_eq!(
            map.dispatch(Mode::Normal, Key::ctrl('p')),
            MatchResult::Matched(Action::TogglePanel)
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
                sandbox: Default::default(),
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

    /// Build a config with a single composite `[[actions]]` entry.
    fn cfg_with_composite(action: &str, params: &[(&str, &str)]) -> superzej_core::config::Config {
        let mut cfg = superzej_core::config::Config::default();
        cfg.actions.push(superzej_core::config::CustomAction {
            name: "composite".into(),
            key: "Alt N".into(),
            run: None,
            action: Some(action.into()),
            params: params
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            menu: true,
            hint: None,
            floating: true,
            close_on_exit: true,
        });
        cfg
    }

    #[test]
    fn composite_new_worktree_registers_and_routes() {
        let cfg = cfg_with_composite("new-worktree", &[("sandbox", "bwrap"), ("agent", "shell")]);
        let mut map = default_keymap_for(&cfg, None, None);
        assert_eq!(map.custom_actions().len(), 1);
        match &map.custom_actions()[0] {
            HostCustomAction::Composite {
                name,
                action:
                    CompositeAction::NewWorktree {
                        name: NameSpec::Random,
                        sandbox,
                        agent,
                        base,
                    },
            } => {
                assert_eq!(name, "composite");
                assert_eq!(sandbox.as_deref(), Some("bwrap"));
                assert_eq!(agent.as_deref(), Some("shell"));
                assert!(base.is_none());
            }
            other => panic!("expected NewWorktree composite, got {other:?}"),
        }
        // The chord routes to the custom action. `Alt N` parses to Char('N')
        // (case preserved, no Shift modifier), so the press must use 'N' — 'n'
        // is the distinct `Alt n` → SplitDown default.
        assert_eq!(
            map.dispatch(
                Mode::Normal,
                Key::modified(KeyCode::Char('N'), Modifiers::ALT)
            ),
            MatchResult::Matched(Action::Custom(0))
        );
    }

    #[test]
    fn composite_new_worktree_fixed_name() {
        let cfg = cfg_with_composite("new-worktree", &[("name", "my-fix")]);
        let map = default_keymap_for(&cfg, None, None);
        match &map.custom_actions()[0] {
            HostCustomAction::Composite {
                action: CompositeAction::NewWorktree { name, .. },
                ..
            } => assert_eq!(*name, NameSpec::Fixed("my-fix".into())),
            other => panic!("expected NewWorktree composite, got {other:?}"),
        }
    }

    #[test]
    fn composite_new_pane_placement_and_cwd() {
        let cfg = cfg_with_composite(
            "new-pane",
            &[
                ("run", "tail -f x"),
                ("placement", "down"),
                ("cwd", "worktree"),
            ],
        );
        let map = default_keymap_for(&cfg, None, None);
        match &map.custom_actions()[0] {
            HostCustomAction::Composite {
                action:
                    CompositeAction::NewPane {
                        run,
                        placement,
                        cwd,
                    },
                ..
            } => {
                assert_eq!(run.as_deref(), Some("tail -f x"));
                assert_eq!(*placement, PanePlacement::Down);
                assert_eq!(*cwd, PaneCwd::Worktree);
            }
            other => panic!("expected NewPane composite, got {other:?}"),
        }
    }

    #[test]
    fn composite_unknown_action_is_skipped() {
        let cfg = cfg_with_composite("frobnicate", &[]);
        let map = default_keymap_for(&cfg, None, None);
        assert!(map.custom_actions().is_empty());
    }

    #[test]
    fn composite_bad_sandbox_and_placement_are_skipped() {
        let map = default_keymap_for(
            &cfg_with_composite("new-worktree", &[("sandbox", "nope")]),
            None,
            None,
        );
        assert!(
            map.custom_actions().is_empty(),
            "bad sandbox skips the action"
        );
        let map = default_keymap_for(
            &cfg_with_composite("new-pane", &[("placement", "sideways")]),
            None,
            None,
        );
        assert!(
            map.custom_actions().is_empty(),
            "bad placement skips the action"
        );
    }

    #[test]
    fn composite_new_pane_defaults() {
        // No params: an interactive shell pane split to the right of the active pane.
        let map = default_keymap_for(&cfg_with_composite("new-pane", &[]), None, None);
        match &map.custom_actions()[0] {
            HostCustomAction::Composite {
                action:
                    CompositeAction::NewPane {
                        run,
                        placement,
                        cwd,
                    },
                ..
            } => {
                assert!(run.is_none());
                assert_eq!(*placement, PanePlacement::Right);
                assert_eq!(*cwd, PaneCwd::Active);
            }
            other => panic!("expected NewPane composite, got {other:?}"),
        }
    }
}
