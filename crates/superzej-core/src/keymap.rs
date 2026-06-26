//! The keybinding registry — the single source of truth for superzej's keys.
//!
//! Every action is declared once in [`BUILTINS`] with its default chord(s), how
//! it's invoked (a `superzej` subcommand, a plugin pipe, or a native zellij
//! action), its menu label, and its short hint. From that one table we:
//!   * generate the `keybinds {}` block spliced into `~/.superzej/zellij.kdl`
//!     (a marked, regenerated region — user edits elsewhere in the file survive);
//!   * drive the Cmd+K menu (labels + chords) and the `superzej keys` surface;
//!   * feed the statusbar's hints (`superzej keys hints`).
//!
//! Users rebind by id (`[keybinds] new-worktree = "Ctrl w"`) and add their own
//! actions (`[[actions]]`) in `config.toml`. Bad chords warn and keep the
//! default; the strict check is `superzej keys validate`.

use crate::config::Config;

/// Markers delimiting the generated keybind block inside the managed
/// `zellij.kdl`. Everything outside them (theme, options, user edits) is left
/// untouched on regeneration.
pub const BEGIN: &str = "// >>> superzej:keybinds (generated — edits here are overwritten) <<<";
pub const END: &str = "// >>> end superzej:keybinds <<<";

/// Which plugin a `MessagePlugin` binding targets (the wasm path is derived so
/// it always matches the layout's `file:~/.local/share/...` references).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plugin {
    Statusbar,
    Tabbar,
    Sidebar,
    Panel,
}

impl Plugin {
    fn wasm(self) -> &'static str {
        match self {
            Plugin::Statusbar => "statusbar.wasm",
            Plugin::Tabbar => "tabbar.wasm",
            Plugin::Sidebar => "sidebar.wasm",
            Plugin::Panel => "panel.wasm",
        }
    }
    fn url(self) -> String {
        format!("file:~/.local/share/superzej/{}", self.wasm())
    }
}

/// How a bound key acts.
#[derive(Debug, Clone)]
pub enum Invocation {
    /// `Run "superzej" <args…>` in a (optionally floating) command pane.
    Run {
        args: &'static [&'static str],
        floating: bool,
        close_on_exit: bool,
        direction: Option<&'static str>,
    },
    /// Pipe a named message to a plugin (no command pane flashes).
    Pipe { plugin: Plugin, name: &'static str },
    /// Send a message directly to a plugin instance via zellij.
    MessagePlugin {
        target: Plugin,
        name: &'static str,
        payload: &'static str,
    },
    /// A raw vanilla-zellij action body, e.g. `MoveFocus "Left";`.
    Native { body: &'static str },
    /// A user-defined shell command (`config.toml [[actions]]`).
    Shell {
        run: String,
        floating: bool,
        close_on_exit: bool,
    },
    /// A user-defined keybind bound to a built-in composite operation
    /// (`config.toml [[actions]] action = "…"`). The host keymap parses
    /// `action`/`params` into a typed composite and dispatches it in-process.
    Builtin {
        action: String,
        params: std::collections::BTreeMap<String, String>,
    },
}

/// Where the binding lives + where its hint shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Scope {
    /// `shared_except "locked"` — active in every non-locked mode.
    Shared,
    /// zellij `tab` mode (the `n` override).
    Tab,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Context {
    Global,
    Center,
    Left,
    Right,
    Top,
    Bottom,
    TopAndBottom,
}

/// A registry entry (static builtin form).
pub struct Action {
    pub id: &'static str,
    pub chords: &'static [&'static str],
    pub menu_label: &'static str,
    pub hint: &'static str,
    pub invocation: Invocation,
    pub scope: Scope,
    pub contexts: &'static [Context],
    pub priority: u32,
    /// Appears in the Cmd+K palette.
    pub menu: bool,
}

/// The owned, fully-resolved form (builtins + user overrides + custom actions).
#[derive(Debug, Clone)]
pub struct Resolved {
    pub id: String,
    pub chords: Vec<Chord>,
    pub menu_label: String,
    pub hint: String,
    pub invocation: Invocation,
    pub scope: Scope,
    pub contexts: Vec<Context>,
    pub priority: u32,
    pub menu: bool,
    pub custom: bool,
}

// ─── Chord ────────────────────────────────────────────────────────────────

/// A validated key chord, stored in canonical zellij form (e.g. `"Ctrl Alt s"`,
/// `"Super Alt Left"`, `"Alt X"`). Shift on a letter is encoded as the uppercase
/// letter (matching zellij + the existing config), not an explicit `Shift`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chord(String);

impl Chord {
    pub fn parse(s: &str) -> Result<Chord, String> {
        let toks: Vec<&str> = s.split_whitespace().collect();
        let Some((key, mods)) = toks.split_last() else {
            return Err("empty key chord".into());
        };
        let (mut ctrl, mut alt, mut sup, mut shift) = (false, false, false, false);
        for m in mods {
            match m.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => ctrl = true,
                "alt" | "opt" | "option" => alt = true,
                "super" | "cmd" | "mod" | "win" => sup = true,
                "shift" => shift = true,
                other => return Err(format!("unknown modifier {other:?} in {s:?}")),
            }
        }
        if key.is_empty() {
            return Err(format!("missing key in {s:?}"));
        }
        // A single letter + Shift → the uppercase letter (zellij convention).
        let key =
            if shift && key.chars().count() == 1 && key.chars().all(|c| c.is_ascii_alphabetic()) {
                shift = false;
                key.to_ascii_uppercase()
            } else {
                key.to_string()
            };
        // Canonical modifier order matches the existing zellij config:
        // Ctrl, Super, Alt, Shift (e.g. "Ctrl Alt s", "Super Alt Left").
        let mut out = String::new();
        for (on, name) in [
            (ctrl, "Ctrl"),
            (sup, "Super"),
            (alt, "Alt"),
            (shift, "Shift"),
        ] {
            if on {
                out.push_str(name);
                out.push(' ');
            }
        }
        out.push_str(&key);
        Ok(Chord(out))
    }

    /// Parse a chord that may be a **multi-key sequence** (e.g. `"Space w"`,
    /// `"Ctrl x Ctrl c"`). Each key-group (optional modifiers + one key) is
    /// canonicalized via [`Chord::parse`] and the groups are space-joined. A
    /// single-group value is identical to [`Chord::parse`]. Used by `[keybinds]`
    /// / `[[actions]]` so user-defined leader sequences are first-class in the
    /// cheatsheet (the native host routes them via its own sequence matcher).
    pub fn parse_loose(s: &str) -> Result<Chord, String> {
        const MODS: &[&str] = &[
            "ctrl", "control", "alt", "opt", "option", "super", "cmd", "mod", "win", "shift",
        ];
        // A token that legitimately ends a key-group: a single character, the
        // leader/space alias, or a recognized named key. Anything else (e.g. a
        // typo'd modifier like "Wat") is NOT a group boundary, so the value
        // falls back to strict single-chord parsing, which reports the error.
        fn is_key_token(tok: &str) -> bool {
            if tok.chars().count() == 1 {
                return true;
            }
            matches!(
                tok.to_ascii_lowercase().as_str(),
                "space"
                    | "leader"
                    | "enter"
                    | "return"
                    | "escape"
                    | "esc"
                    | "backspace"
                    | "bs"
                    | "tab"
                    | "left"
                    | "leftarrow"
                    | "right"
                    | "rightarrow"
                    | "up"
                    | "uparrow"
                    | "down"
                    | "downarrow"
                    | "delete"
                    | "del"
                    | "pageup"
                    | "pgup"
                    | "pagedown"
                    | "pgdn"
                    | "home"
                    | "end"
            )
        }
        let toks: Vec<&str> = s.split_whitespace().collect();
        if toks.is_empty() {
            return Err("empty key chord".into());
        }
        // Split into key-groups, closing each on a recognized key token. An
        // unclassifiable token bails to strict parsing (preserving typo errors).
        let mut groups: Vec<String> = Vec::new();
        let mut cur: Vec<&str> = Vec::new();
        for tok in &toks {
            let lower = tok.to_ascii_lowercase();
            if MODS.contains(&lower.as_str()) {
                cur.push(tok);
            } else if is_key_token(tok) {
                cur.push(tok);
                groups.push(cur.join(" "));
                cur.clear();
            } else {
                // Not a modifier and not a recognizable key → let the strict
                // single-chord parser classify (and reject) the whole value.
                return Chord::parse(s);
            }
        }
        if !cur.is_empty() {
            return Err(format!("dangling modifier(s) in {s:?}"));
        }
        if groups.len() == 1 {
            return Chord::parse(&groups[0]);
        }
        // Multi-key sequence: canonicalize each group and space-join.
        let canon: Result<Vec<String>, String> = groups
            .iter()
            .map(|g| Chord::parse(g).map(|c| c.0))
            .collect();
        Ok(Chord(canon?.join(" ")))
    }

    /// The canonical zellij chord string (for KDL + `keys get`).
    pub fn to_kdl(&self) -> &str {
        &self.0
    }

    /// A short hint form (e.g. `Alt-w`, `Ctrl-Alt-s`) for the menu / status.
    pub fn to_hint(&self) -> String {
        self.0.replace(' ', "-")
    }
}

// ─── The builtin action table ───────────────────────────────────────────────

macro_rules! run {
    ($($a:literal),+) => {
        Invocation::Run { args: &[$($a),+], floating: false, close_on_exit: false, direction: None }
    };
}
macro_rules! run_float {
    ($($a:literal),+) => {
        Invocation::Run { args: &[$($a),+], floating: true, close_on_exit: true, direction: None }
    };
}

pub const BUILTINS: &[Action] = &[
    Action {
        id: "new-worktree",
        chords: &["Alt w"],
        menu_label: "New worktree — branch off the base",
        hint: "worktree",
        invocation: run_float!("new-worktree"),
        scope: Scope::Shared,
        contexts: &[Context::Global],
        priority: 100,
        menu: true,
    },
    Action {
        id: "close-worktree",
        chords: &["Alt x"],
        menu_label: "Close worktree (+ its tab)",
        hint: "close",
        invocation: run_float!("close-worktree"),
        scope: Scope::Shared,
        contexts: &[Context::Global],
        priority: 100,
        menu: true,
    },
    Action {
        id: "new-tab",
        chords: &["Alt t"],
        menu_label: "New tab — same worktree",
        hint: "tab",
        invocation: Invocation::Pipe {
            plugin: Plugin::Tabbar,
            name: "superzej_new_tab",
        },
        scope: Scope::Shared,
        contexts: &[Context::Global],
        priority: 100,
        menu: true,
    },
    Action {
        id: "new-workspace",
        chords: &["Alt W"],
        menu_label: "New workspace — open a repo",
        hint: "new repo",
        invocation: run_float!("new-workspace"),
        scope: Scope::Shared,
        contexts: &[Context::Global],
        priority: 100,
        menu: true,
    },
    Action {
        id: "menu",
        chords: &["Ctrl Space"],
        menu_label: "Command palette",
        hint: "menu",
        // Super+K routes through the statusbar so it's a real toggle: a bare
        // `Run` only ever *spawns* a pane (a second press can't close the open
        // palette, and rapid presses race a flurry of floating panes). The
        // statusbar tracks the palette's id and spawns `superzej menu` itself.
        invocation: Invocation::Pipe {
            plugin: Plugin::Statusbar,
            name: "superzej_toggle_palette",
        },
        scope: Scope::Shared,
        contexts: &[Context::Global],
        priority: 1000, // Very important
        menu: false,
    },
    Action {
        id: "files",
        chords: &["Ctrl Alt f"],
        menu_label: "File drawer (yazi)",
        hint: "drawer",
        // Spawn/close a bottom-anchored floating yazi rooted at the focused
        // worktree; `superzej files` self-toggles (closes via the statusbar's
        // `superzej_close_files` pipe when already open).
        invocation: run_float!("files"),
        scope: Scope::Shared,
        contexts: &[Context::Global],
        priority: 100,
        menu: true,
    },
    Action {
        id: "new-panel-native",
        chords: &["Alt n"],
        menu_label: "New panel — split pane",
        hint: "split↓",
        invocation: Invocation::Native {
            body: "NewPane \"Down\";",
        },
        scope: Scope::Shared,
        contexts: &[Context::Center],
        priority: 500,
        menu: true,
    },
    Action {
        id: "new-panel",
        chords: &["Alt N"],
        menu_label: "New panel — split right (scoped)",
        hint: "split→",
        invocation: Invocation::Run {
            args: &["new-panel", "--in-place"],
            floating: false,
            close_on_exit: false,
            direction: Some("Right"),
        },
        scope: Scope::Shared,
        contexts: &[Context::Center],
        priority: 500,
        menu: false,
    },
    Action {
        id: "switch-repo",
        chords: &["Alt o"],
        menu_label: "Switch repo — recents picker",
        hint: "switch repo",
        invocation: run_float!("launch"),
        scope: Scope::Shared,
        contexts: &[Context::Global],
        priority: 10,
        menu: true,
    },
    Action {
        id: "dashboard",
        chords: &["Alt d"],
        menu_label: "Worktree dashboard",
        hint: "dashboard",
        invocation: run!("dashboard"),
        scope: Scope::Shared,
        contexts: &[Context::Global],
        priority: 10,
        menu: true,
    },
    Action {
        id: "toggle-sidebar",
        chords: &["Ctrl Alt s"],
        menu_label: "Toggle sidebar",
        hint: "sidebar",
        invocation: Invocation::Pipe {
            plugin: Plugin::Statusbar,
            name: "superzej_toggle_sidebar",
        },
        scope: Scope::Shared,
        contexts: &[Context::Global],
        priority: 50,
        menu: true,
    },
    Action {
        id: "toggle-panel",
        chords: &["Ctrl Alt p"],
        menu_label: "Toggle diff / PR panel",
        hint: "panel",
        invocation: Invocation::Pipe {
            plugin: Plugin::Statusbar,
            name: "superzej_toggle_panel",
        },
        scope: Scope::Shared,
        contexts: &[Context::Global],
        priority: 50,
        menu: true,
    },
    Action {
        id: "tool-lazygit",
        chords: &["Alt g"],
        menu_label: "lazygit",
        hint: "lazygit",
        invocation: run_float!("tool", "lazygit"),
        scope: Scope::Shared,
        contexts: &[Context::Global],
        priority: 10,
        menu: true,
    },
    Action {
        id: "tool-yazi",
        chords: &["Alt y"],
        menu_label: "yazi — file manager",
        hint: "files",
        invocation: run_float!("tool", "yazi"),
        scope: Scope::Shared,
        contexts: &[Context::Global],
        priority: 10,
        menu: true,
    },
    Action {
        id: "tool-editor",
        chords: &["Alt e"],
        menu_label: "editor",
        hint: "edit",
        invocation: run_float!("tool", "editor"),
        scope: Scope::Shared,
        contexts: &[Context::Global],
        priority: 10,
        menu: true,
    },
    Action {
        id: "tool-diff",
        chords: &["Alt /"],
        menu_label: "git diff",
        hint: "diff",
        invocation: run_float!("tool", "diff"),
        scope: Scope::Shared,
        contexts: &[Context::Global],
        priority: 10,
        menu: true,
    },
    Action {
        id: "prev-tab",
        chords: &["Alt Left"],
        menu_label: "Previous tab (within worktree)",
        hint: "tabs",
        invocation: Invocation::Native {
            body: "GoToPreviousTab;",
        },
        scope: Scope::Shared,
        contexts: &[Context::Global],
        priority: 400,
        menu: false,
    },
    Action {
        id: "next-tab",
        chords: &["Alt Right"],
        menu_label: "Next tab (within worktree)",
        hint: "tabs",
        invocation: Invocation::Native {
            body: "GoToNextTab;",
        },
        scope: Scope::Shared,
        contexts: &[Context::Global],
        priority: 400,
        menu: false,
    },
    Action {
        id: "prev-worktree",
        chords: &["Alt Up"],
        menu_label: "Previous worktree",
        hint: "worktrees",
        invocation: Invocation::Native {
            body: "GoToPreviousTab;",
        },
        scope: Scope::Shared,
        contexts: &[Context::Global],
        priority: 400,
        menu: false,
    },
    Action {
        id: "next-worktree",
        chords: &["Alt Down"],
        menu_label: "Next worktree",
        hint: "worktrees",
        invocation: Invocation::Native {
            body: "GoToNextTab;",
        },
        scope: Scope::Shared,
        contexts: &[Context::Global],
        priority: 400,
        menu: false,
    },
    Action {
        id: "close-tab",
        chords: &["Alt X"],
        menu_label: "Close tab",
        hint: "tabs",
        invocation: Invocation::Native { body: "CloseTab;" },
        scope: Scope::Shared,
        contexts: &[Context::Global],
        priority: 400,
        menu: false,
    },
    Action {
        id: "toggle-key-lock",
        chords: &["Ctrl g"],
        menu_label: "Lock/unlock keybinds (pass keys to pane)",
        hint: "lock",
        invocation: Invocation::Native { body: ";" },
        scope: Scope::Shared,
        contexts: &[Context::Global],
        priority: 1000,
        menu: false,
    },
    Action {
        id: "focus-left",
        chords: &["Ctrl Left", "Ctrl h"],
        menu_label: "Focus left (pane → sidebar)",
        hint: "←↓↑→",
        invocation: Invocation::Native {
            body: "MoveFocus \"Left\";",
        },
        scope: Scope::Shared,
        contexts: &[Context::Global],
        priority: 600,
        menu: false,
    },
    Action {
        id: "focus-down",
        chords: &["Ctrl Down", "Ctrl j"],
        menu_label: "Focus down (pane / row / widget)",
        hint: "←↓↑→",
        invocation: Invocation::Native {
            body: "MoveFocus \"Down\";",
        },
        scope: Scope::Shared,
        contexts: &[Context::Global],
        priority: 600,
        menu: false,
    },
    Action {
        id: "focus-up",
        chords: &["Ctrl Up", "Ctrl k"],
        menu_label: "Focus up (pane / row / widget)",
        hint: "←↓↑→",
        invocation: Invocation::Native {
            body: "MoveFocus \"Up\";",
        },
        scope: Scope::Shared,
        contexts: &[Context::Global],
        priority: 600,
        menu: false,
    },
    Action {
        id: "focus-right",
        chords: &["Ctrl Right", "Ctrl l"],
        menu_label: "Focus right (pane → panel)",
        hint: "←↓↑→",
        invocation: Invocation::Native {
            body: "MoveFocus \"Right\";",
        },
        scope: Scope::Shared,
        contexts: &[Context::Global],
        priority: 600,
        menu: false,
    },
    Action {
        id: "select-bottombar",
        chords: &["Super Alt Down", "Super Alt j"],
        menu_label: "Focus the bottom status bar",
        hint: "nav",
        // The statusbar owns bottom-bar selection (highlight + key routing).
        invocation: Invocation::Pipe {
            plugin: Plugin::Statusbar,
            name: "superzej_select_bottombar",
        },
        scope: Scope::Shared,
        contexts: &[Context::Global],
        priority: 700,
        menu: false,
    },
    Action {
        id: "select-topbar",
        chords: &["Super Alt Up", "Super Alt k"],
        menu_label: "Focus the top stats bar",
        hint: "nav",
        // The tabbar owns top-bar stat selection (CPU/MEM/GPU → embed monitor).
        invocation: Invocation::Pipe {
            plugin: Plugin::Tabbar,
            name: "superzej_select_topbar",
        },
        scope: Scope::Shared,
        contexts: &[Context::Global],
        priority: 700,
        menu: false,
    },
    // Menu-only actions (no default chord).
    Action {
        id: "focus-sidebar",
        chords: &["Alt s"],
        menu_label: "Focus workspace sidebar",
        hint: "focus-sidebar",
        invocation: Invocation::MessagePlugin {
            target: Plugin::Sidebar,
            name: "superzej_focus_sidebar",
            payload: "",
        },
        scope: Scope::Shared,
        contexts: &[Context::Center, Context::Right],
        priority: 500,
        menu: true,
    },
    Action {
        id: "focus-panel",
        chords: &["Alt ."],
        menu_label: "Focus diff / PR panel",
        hint: "focus-panel",
        invocation: Invocation::MessagePlugin {
            target: Plugin::Panel,
            name: "superzej_focus_panel",
            payload: "",
        },
        scope: Scope::Shared,
        contexts: &[Context::Center, Context::Left],
        priority: 500,
        menu: true,
    },
    Action {
        id: "pr-open",
        chords: &[],
        menu_label: "PR — open in browser",
        hint: "pr",
        invocation: run!("pr", "open"),
        scope: Scope::Shared,
        contexts: &[Context::Global],
        priority: 5,
        menu: true,
    },
    Action {
        id: "pr-create",
        chords: &[],
        menu_label: "PR — create (web)",
        hint: "pr",
        invocation: run!("pr", "create", "--web"),
        scope: Scope::Shared,
        contexts: &[Context::Global],
        priority: 5,
        menu: true,
    },
    // ── Pinned programs — Alt-1..9 launch-or-focus (config `[[pins]]`) ─────────
    // Pipe to the tabbar (no command-pane flash, like `new-tab`): it maps the
    // index to the configured pin name and runs `superzej pin open <name>`.
    pin_action("pin-1", &["Alt 1"], "superzej_pin_1"),
    pin_action("pin-2", &["Alt 2"], "superzej_pin_2"),
    pin_action("pin-3", &["Alt 3"], "superzej_pin_3"),
    pin_action("pin-4", &["Alt 4"], "superzej_pin_4"),
    pin_action("pin-5", &["Alt 5"], "superzej_pin_5"),
    pin_action("pin-6", &["Alt 6"], "superzej_pin_6"),
    pin_action("pin-7", &["Alt 7"], "superzej_pin_7"),
    pin_action("pin-8", &["Alt 8"], "superzej_pin_8"),
    pin_action("pin-9", &["Alt 9"], "superzej_pin_9"),
];

/// A pinned-program builtin: `Alt-N` pipes `superzej_pin_N` to the tabbar, which
/// resolves the index to a pin and launch-or-focuses it. `const fn` so the
/// entries stay in the `BUILTINS` const slice.
const fn pin_action(
    id: &'static str,
    chords: &'static [&'static str],
    pipe: &'static str,
) -> Action {
    Action {
        id,
        chords,
        menu_label: "Pinned program",
        hint: "pin",
        invocation: Invocation::Pipe {
            plugin: Plugin::Tabbar,
            name: pipe,
        },
        scope: Scope::Shared,
        contexts: &[Context::Global],
        priority: 50,
        menu: false,
    }
}

/// Terminal-critical chords: claiming one steals interrupt/EOF/suspend from
/// every program in every pane, so a user override landing on one is flagged
/// by `keys validate`. (The zellij-era modal chords are gone — the native host
/// owns input, and Ctrl+g/Ctrl+hjkl are deliberate superzej bindings with the
/// Ctrl+g lock as the escape hatch.)
const RESERVED: &[&str] = &["Ctrl c", "Ctrl d", "Ctrl z"];

// ─── Resolution: builtins + overrides + custom ──────────────────────────────

fn parse_chords(strs: &[&str], id: &str) -> Vec<Chord> {
    strs.iter()
        .filter_map(|s| match Chord::parse(s) {
            Ok(c) => Some(c),
            Err(e) => {
                // A builtin with a bad chord is a programmer bug (tests catch it).
                crate::msg::warn(&format!("keymap: {id}: {e}"));
                None
            }
        })
        .collect()
}

/// The effective registry: builtins, with `[keybinds]` rebinds and `[[actions]]`
/// custom entries applied. Bad user chords warn and keep the default.
pub fn effective(cfg: &Config) -> Vec<Resolved> {
    let mut out: Vec<Resolved> = BUILTINS
        .iter()
        .map(|a| Resolved {
            id: a.id.to_string(),
            chords: parse_chords(a.chords, a.id),
            menu_label: a.menu_label.to_string(),
            hint: a.hint.to_string(),
            invocation: a.invocation.clone(),
            scope: a.scope,
            contexts: a.contexts.to_vec(),
            priority: a.priority,
            menu: a.menu,
            custom: false,
        })
        .collect();

    // [keybinds] — rebind a builtin by id (whole chord set replaced).
    for (id, chord) in &cfg.keybinds {
        match out.iter_mut().find(|r| &r.id == id) {
            Some(r) => match Chord::parse_loose(chord) {
                Ok(c) => r.chords = vec![c],
                Err(e) => crate::msg::warn(&format!("[keybinds] {id}: {e}; keeping default")),
            },
            None => crate::msg::warn(&format!("[keybinds] unknown action {id:?}; ignored")),
        }
    }

    // [[actions]] — user-defined shell actions (`Shell`) or built-in composite
    // actions (`Builtin`). Exactly one of `run` / `action` must be set.
    for a in &cfg.actions {
        let invocation = match (&a.run, &a.action) {
            (Some(run), None) => Invocation::Shell {
                run: run.clone(),
                floating: a.floating,
                close_on_exit: a.close_on_exit,
            },
            (None, Some(action)) => Invocation::Builtin {
                action: action.clone(),
                params: a.params.clone(),
            },
            (Some(_), Some(_)) => {
                crate::msg::warn(&format!(
                    "[[actions]] {}: set exactly one of `run` or `action`; skipped",
                    a.name
                ));
                continue;
            }
            (None, None) => {
                crate::msg::warn(&format!(
                    "[[actions]] {}: needs a `run` command or an `action`; skipped",
                    a.name
                ));
                continue;
            }
        };
        let chords = match Chord::parse_loose(&a.key) {
            Ok(c) => vec![c],
            Err(e) => {
                crate::msg::warn(&format!("[[actions]] {}: {e}; skipped", a.name));
                continue;
            }
        };
        out.push(Resolved {
            id: a.name.clone(),
            chords,
            menu_label: a.name.clone(),
            hint: a.hint.clone().unwrap_or_else(|| a.name.clone()),
            invocation,
            scope: Scope::Shared,
            contexts: vec![Context::Global],
            priority: 50,
            menu: a.menu,
            custom: true,
        });
    }
    out
}

#[derive(Debug, PartialEq, Eq)]
pub enum Collision {
    Duplicate { chord: String, ids: Vec<String> },
    Reserved { chord: String, id: String },
}

/// Find chord clashes (two actions on one chord in the same scope) and overrides
/// landing on a reserved vanilla-zellij chord. Drives `keys validate`.
pub fn detect_collisions(actions: &[Resolved]) -> Vec<Collision> {
    use std::collections::BTreeMap;
    let mut by_chord: BTreeMap<(Scope, String), Vec<String>> = BTreeMap::new();
    for a in actions {
        for c in &a.chords {
            by_chord
                .entry((a.scope, c.to_kdl().to_string()))
                .or_default()
                .push(a.id.clone());
        }
    }
    let mut out = Vec::new();
    for ((_, chord), ids) in &by_chord {
        if ids.len() > 1 {
            out.push(Collision::Duplicate {
                chord: chord.clone(),
                ids: ids.clone(),
            });
        }
    }
    for a in actions {
        for c in &a.chords {
            if RESERVED.contains(&c.to_kdl()) {
                out.push(Collision::Reserved {
                    chord: c.to_kdl().to_string(),
                    id: a.id.clone(),
                });
            }
        }
    }
    out
}

// ─── KDL generation ─────────────────────────────────────────────────────────

/// The `floating`/`close_on_exit`/`direction` child-node lines for a `Run`.
fn run_opt_lines(floating: bool, close_on_exit: bool, direction: Option<&str>) -> Vec<String> {
    let mut v = Vec::new();
    if floating {
        v.push("floating true".to_string());
    }
    if close_on_exit {
        v.push("close_on_exit true".to_string());
    }
    if let Some(d) = direction {
        v.push(format!("direction \"{d}\""));
    }
    v
}

/// A `Run`/`MessagePlugin` action with a child block, rendered as the body of a
/// `bind` MULTI-LINE: zellij's KDL parser rejects a nested child block placed on
/// the same line as the bind (`bind "X" { Run … { … } }` fails to deserialize),
/// so the nested block must span lines. `head` is e.g. `Run "superzej" "x"`;
/// `children` are the inner node lines. Indented to sit inside `shared_except`/
/// `tab` (8-space `bind`, 12-space action, 16-space children).
fn render_block_bind(chord: &str, head: &str, children: &[String]) -> String {
    let mut s = format!("        bind \"{chord}\" {{\n            {head} {{\n");
    for c in children {
        s.push_str(&format!("                {c}\n"));
    }
    s.push_str("            }\n        }\n");
    s
}

/// Render a single `bind "<chord>" { … }` block. Actions carrying a nested child
/// block (`Run … { floating … }`, `MessagePlugin … { name … }`) are emitted
/// multi-line (see [`render_block_bind`]); bare actions stay on one line.
fn render_bind(chord: &str, inv: &Invocation) -> String {
    match inv {
        Invocation::Native { body } => format!("        bind \"{chord}\" {{ {body} }}\n"),
        Invocation::Run {
            args,
            floating,
            close_on_exit,
            direction,
        } => {
            let argv = args
                .iter()
                .map(|a| format!("\"{a}\""))
                .collect::<Vec<_>>()
                .join(" ");
            let opts = run_opt_lines(*floating, *close_on_exit, *direction);
            if opts.is_empty() {
                format!("        bind \"{chord}\" {{ Run \"superzej\" {argv}; }}\n")
            } else {
                render_block_bind(chord, &format!("Run \"superzej\" {argv}"), &opts)
            }
        }
        Invocation::Shell {
            run,
            floating,
            close_on_exit,
        } => {
            let esc = run.replace('\\', "\\\\").replace('"', "\\\"");
            let opts = run_opt_lines(*floating, *close_on_exit, None);
            if opts.is_empty() {
                format!("        bind \"{chord}\" {{ Run \"sh\" \"-c\" \"{esc}\"; }}\n")
            } else {
                render_block_bind(chord, &format!("Run \"sh\" \"-c\" \"{esc}\""), &opts)
            }
        }
        Invocation::Pipe { plugin, name } => render_block_bind(
            chord,
            &format!("MessagePlugin \"{}\"", plugin.url()),
            &[format!("name \"{name}\"")],
        ),
        Invocation::MessagePlugin {
            target,
            name,
            payload,
        } => {
            let mut args = vec![format!("name \"{name}\"")];
            if !payload.is_empty() {
                args.push(format!("payload \"{payload}\""));
            }
            render_block_bind(chord, &format!("MessagePlugin \"{}\"", target.url()), &args)
        }
        // Built-in composite actions are dispatched in-process by the host and
        // have no zellij-KDL representation (this renderer is legacy).
        Invocation::Builtin { .. } => String::new(),
    }
}

/// Render just the `keybinds {}` block (between the markers) from the effective
/// registry. Byte-stable for a given registry, so it diffs cleanly.
pub fn render_keybinds_kdl(actions: &[Resolved]) -> String {
    let mut shared = String::new();
    let mut tab = String::new();
    for a in actions {
        for c in &a.chords {
            let line = render_bind(c.to_kdl(), &a.invocation);
            match a.scope {
                Scope::Shared => shared.push_str(&line),
                Scope::Tab => tab.push_str(&line),
            }
        }
    }
    // The tab-mode `n` override always repoints new-tab (+ returns to Normal).
    // Rendered multi-line: a `MessagePlugin … { … }` block can't share the
    // bind's line (zellij KDL), and `SwitchToMode` follows as a sibling node.
    let (nt_url, nt_name) = actions
        .iter()
        .find(|a| a.id == "new-tab")
        .and_then(|a| match &a.invocation {
            Invocation::Pipe { plugin, name } => Some((plugin.url(), name.to_string())),
            _ => None,
        })
        .unwrap_or_else(|| {
            (
                "file:~/.local/share/superzej/tabbar.wasm".to_string(),
                "superzej_new_tab".to_string(),
            )
        });
    let new_tab_bind = format!(
        "        bind \"n\" {{\n            MessagePlugin \"{nt_url}\" {{\n                name \"{nt_name}\"\n            }}\n            SwitchToMode \"Normal\"\n        }}\n"
    );

    let mut out = String::new();
    out.push_str(BEGIN);
    out.push('\n');
    out.push_str("keybinds {\n");
    out.push_str("    shared_except \"locked\" {\n");
    out.push_str(&shared);
    out.push_str("    }\n");
    out.push_str("    tab {\n");
    out.push_str(&new_tab_bind);
    out.push_str(&tab);
    out.push_str("    }\n");
    out.push_str("}\n");
    out.push_str(END);
    out.push('\n');
    out
}

/// Replace the marked keybind region in `existing` with `generated`, preserving
/// everything outside it. A legacy file (no markers) has its top-level
/// `keybinds { … }` block replaced; if there's none, the region is prepended.
pub fn splice_managed_region(existing: &str, generated: &str) -> String {
    if let (Some(b), Some(e)) = (existing.find(BEGIN), existing.find(END)) {
        let end = e + END.len();
        // Drop a single trailing newline after END so we don't accrete blanks.
        let after = existing[end..]
            .strip_prefix('\n')
            .unwrap_or(&existing[end..]);
        return format!("{}{}\n{}", &existing[..b], generated.trim_end(), after);
    }
    // Legacy: replace the first top-level `keybinds { … }` block (brace-matched).
    if let Some(start) = existing.find("keybinds")
        && let Some(open) = existing[start..].find('{')
    {
        let abs_open = start + open;
        if let Some(close) = match_brace(&existing[abs_open..]) {
            let abs_close = abs_open + close;
            return format!(
                "{}{}\n{}",
                &existing[..start],
                generated.trim_end(),
                existing[abs_close + 1..].trim_start_matches('\n')
            );
        }
    }
    // No keybinds block at all: prepend the region.
    format!("{}\n\n{}", generated.trim_end(), existing)
}

/// Index (relative to the opening `{` at position 0) of the matching `}`,
/// ignoring braces inside `"…"` strings and `//` line comments.
fn match_brace(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut depth = 0i32;
    let mut in_str = false;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'"' if !in_str => in_str = true,
            b'"' if in_str => in_str = false,
            b'/' if !in_str && i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            b'{' if !in_str => depth += 1,
            b'}' if !in_str => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chord_parse_and_render() {
        assert_eq!(Chord::parse("Ctrl Alt s").unwrap().to_kdl(), "Ctrl Alt s");
        assert_eq!(Chord::parse("alt w").unwrap().to_kdl(), "Alt w");
        // Shift+letter normalizes to uppercase, modifier order canonicalizes.
        assert_eq!(Chord::parse("shift alt x").unwrap().to_kdl(), "Alt X");
        assert_eq!(Chord::parse("cmd k").unwrap().to_kdl(), "Super k");
        assert_eq!(
            Chord::parse("Super Alt Left").unwrap().to_kdl(),
            "Super Alt Left"
        );
        assert_eq!(Chord::parse("Alt w").unwrap().to_hint(), "Alt-w");
        assert!(Chord::parse("Bogus w").is_err());
        assert!(Chord::parse("").is_err());
    }

    #[test]
    fn parse_loose_accepts_single_chords_and_sequences() {
        // A single chord behaves exactly like Chord::parse (and canonicalizes).
        assert_eq!(Chord::parse_loose("alt w").unwrap().to_kdl(), "Alt w");
        assert_eq!(Chord::parse_loose("shift alt x").unwrap().to_kdl(), "Alt X");

        // Multi-key leader sequences split into canonical key-groups.
        assert_eq!(Chord::parse_loose("Space w").unwrap().to_kdl(), "Space w");
        assert_eq!(
            Chord::parse_loose("ctrl x ctrl c").unwrap().to_kdl(),
            "Ctrl x Ctrl c"
        );
        // The hint dashes the whole sequence.
        assert_eq!(Chord::parse_loose("Space w").unwrap().to_hint(), "Space-w");

        // A dangling trailing modifier is rejected; an unknown key too.
        assert!(Chord::parse_loose("Space ctrl").is_err());
        assert!(Chord::parse_loose("Space boguskey").is_err());
        assert!(Chord::parse_loose("").is_err());
    }

    #[test]
    fn effective_accepts_a_sequence_rebind_without_warning() {
        let mut cfg = Config::default();
        cfg.keybinds.insert("new-worktree".into(), "Space w".into());
        let resolved = effective(&cfg);
        let nw = resolved.iter().find(|r| r.id == "new-worktree").unwrap();
        // The rebind replaced the default chord with the canonical sequence.
        assert_eq!(nw.chords.len(), 1);
        assert_eq!(nw.chords[0].to_kdl(), "Space w");
    }

    #[test]
    fn builtins_all_parse() {
        for a in BUILTINS {
            for c in a.chords {
                assert!(Chord::parse(c).is_ok(), "{}: bad chord {c:?}", a.id);
            }
        }
    }

    #[test]
    fn render_contains_known_bindings() {
        let acts = effective(&Config::default());
        let kdl = render_keybinds_kdl(&acts);
        assert!(kdl.contains(BEGIN) && kdl.contains(END));
        // A floating Run is emitted MULTI-LINE (zellij KDL rejects an inline
        // nested block); check the head line + its child options.
        assert!(kdl.contains(
            "        bind \"Alt w\" {\n            Run \"superzej\" \"new-worktree\" {\n"
        ));
        assert!(kdl.contains("                floating true\n"));
        assert!(kdl.contains("                close_on_exit true\n"));
        // A pipe (MessagePlugin) is multi-line with its `name` child.
        assert!(kdl.contains("        bind \"Ctrl Alt s\" {\n            MessagePlugin \"file:~/.local/share/superzej/statusbar.wasm\" {\n                name \"superzej_toggle_sidebar\"\n            }\n        }\n"));
        // A bare native action stays on one line.
        assert!(kdl.contains("        bind \"Ctrl Left\" { MoveFocus \"Left\"; }\n"));
        // tab-mode override present
        assert!(kdl.contains("    tab {\n"));
        assert!(kdl.contains("SwitchToMode \"Normal\""));
        // Regression guard: no inline nested block (the form zellij rejects) —
        // a single line must never carry both an action head and its child.
        assert!(
            !kdl.lines()
                .any(|l| l.contains("Run \"") && l.contains("floating true")),
            "Run options must not be inlined on the bind line"
        );
        assert!(
            !kdl.lines()
                .any(|l| l.contains("MessagePlugin") && l.contains("name \"")),
            "MessagePlugin name must not be inlined on the bind line"
        );
    }

    #[test]
    fn splice_preserves_surroundings_and_is_idempotent() {
        let theme = "theme \"superzej\"\noptions { x true }\n";
        let gen1 = render_keybinds_kdl(&effective(&Config::default()));
        // Legacy file: a hand-written keybinds block + theme after it.
        let legacy = format!("// header\nkeybinds {{\n  bind \"Alt q\" {{ Quit; }}\n}}\n{theme}");
        let once = splice_managed_region(&legacy, &gen1);
        assert!(once.contains(BEGIN));
        assert!(once.contains("theme \"superzej\""), "theme preserved");
        assert!(!once.contains("Alt q"), "old block replaced");
        // Idempotent: splicing again yields the same thing.
        let twice = splice_managed_region(&once, &gen1);
        assert_eq!(once, twice);
        // A user edit OUTSIDE the region survives.
        let edited = once.replace("options { x true }", "options { x false }");
        let spliced = splice_managed_region(&edited, &gen1);
        assert!(spliced.contains("options { x false }"));
    }

    #[test]
    fn override_and_custom_and_collision() {
        let mut cfg = Config::default();
        cfg.keybinds.insert("new-worktree".into(), "Ctrl w".into());
        cfg.actions.push(crate::config::CustomAction {
            name: "deploy".into(),
            key: "Alt D".into(),
            run: Some("just deploy".into()),
            action: None,
            params: Default::default(),
            menu: true,
            hint: None,
            floating: true,
            close_on_exit: true,
        });
        let acts = effective(&cfg);
        let nw = acts.iter().find(|a| a.id == "new-worktree").unwrap();
        assert_eq!(nw.chords[0].to_kdl(), "Ctrl w");
        let dep = acts.iter().find(|a| a.id == "deploy").unwrap();
        assert!(dep.custom && dep.menu);
        // No collisions in the default-ish set.
        let cols = detect_collisions(&acts);
        assert!(cols.is_empty(), "{cols:?}");

        // Force a duplicate + a reserved hit.
        cfg.keybinds.insert("dashboard".into(), "Alt w".into()); // dup with default new-worktree? new-worktree now Ctrl w, so Alt w free → use Alt g (tool-lazygit)
        cfg.keybinds.insert("dashboard".into(), "Alt g".into());
        cfg.keybinds.insert("switch-repo".into(), "Ctrl c".into()); // reserved
        let cols = detect_collisions(&effective(&cfg));
        assert!(
            cols.iter()
                .any(|c| matches!(c, Collision::Duplicate { .. }))
        );
        assert!(cols.iter().any(|c| matches!(c, Collision::Reserved { .. })));
    }

    #[test]
    fn custom_shell_action_renders_as_sh_c() {
        let mut cfg = Config::default();
        cfg.actions.push(crate::config::CustomAction {
            name: "deploy".into(),
            key: "Alt D".into(),
            run: Some("echo \"hi\"".into()),
            action: None,
            params: Default::default(),
            menu: false,
            hint: Some("dep".into()),
            floating: true,
            close_on_exit: true,
        });
        let kdl = render_keybinds_kdl(&effective(&cfg));
        // Floating shell action -> multi-line `Run "sh" "-c" … { … }`.
        assert!(kdl.contains("        bind \"Alt D\" {\n            Run \"sh\" \"-c\""));
        assert!(kdl.contains("echo \\\"hi\\\""), "quotes escaped: {kdl}");
        // the scoped panel binding carries its direction option (multi-line).
        assert!(kdl.contains("            Run \"superzej\" \"new-panel\" \"--in-place\" {\n                direction \"Right\"\n"));
    }

    #[test]
    fn bad_custom_chord_is_skipped() {
        let mut cfg = Config::default();
        cfg.actions.push(crate::config::CustomAction {
            name: "broke".into(),
            key: "Wat x".into(), // unknown modifier
            run: Some("true".into()),
            action: None,
            params: Default::default(),
            menu: true,
            hint: None,
            floating: false,
            close_on_exit: false,
        });
        let acts = effective(&cfg);
        assert!(acts.iter().all(|a| a.id != "broke"));
    }

    #[test]
    fn builtin_action_yields_builtin_invocation() {
        let mut cfg = Config::default();
        let mut params = std::collections::BTreeMap::new();
        params.insert("sandbox".to_string(), "bwrap".to_string());
        params.insert("agent".to_string(), "shell".to_string());
        cfg.actions.push(crate::config::CustomAction {
            name: "scratch-shell".into(),
            key: "Alt N".into(),
            run: None,
            action: Some("new-worktree".into()),
            params,
            menu: true,
            hint: None,
            floating: true,
            close_on_exit: true,
        });
        let acts = effective(&cfg);
        let a = acts
            .iter()
            .find(|a| a.id == "scratch-shell")
            .expect("custom action present");
        assert!(a.custom && a.menu);
        match &a.invocation {
            Invocation::Builtin { action, params } => {
                assert_eq!(action, "new-worktree");
                assert_eq!(params.get("sandbox").map(String::as_str), Some("bwrap"));
                assert_eq!(params.get("agent").map(String::as_str), Some("shell"));
            }
            other => panic!("expected Builtin, got {other:?}"),
        }
        // A built-in composite still renders nothing in the legacy KDL.
        let kdl = render_keybinds_kdl(&acts);
        assert!(!kdl.contains("scratch-shell"));
    }

    #[test]
    fn run_form_yields_shell_invocation() {
        let mut cfg = Config::default();
        cfg.actions.push(crate::config::CustomAction {
            name: "deploy".into(),
            key: "Alt D".into(),
            run: Some("just deploy".into()),
            action: None,
            params: Default::default(),
            menu: false,
            hint: None,
            floating: true,
            close_on_exit: true,
        });
        let acts = effective(&cfg);
        let a = acts.iter().find(|a| a.id == "deploy").unwrap();
        assert!(matches!(a.invocation, Invocation::Shell { .. }));
    }

    #[test]
    fn action_requires_exactly_one_of_run_or_action() {
        let mut cfg = Config::default();
        // Both set -> skipped.
        cfg.actions.push(crate::config::CustomAction {
            name: "both".into(),
            key: "Alt B".into(),
            run: Some("echo hi".into()),
            action: Some("new-worktree".into()),
            params: Default::default(),
            menu: false,
            hint: None,
            floating: true,
            close_on_exit: true,
        });
        // Neither set -> skipped.
        cfg.actions.push(crate::config::CustomAction {
            name: "neither".into(),
            key: "Alt M".into(),
            run: None,
            action: None,
            params: Default::default(),
            menu: false,
            hint: None,
            floating: true,
            close_on_exit: true,
        });
        let acts = effective(&cfg);
        assert!(acts.iter().all(|a| a.id != "both"));
        assert!(acts.iter().all(|a| a.id != "neither"));
    }

    #[test]
    fn splice_prepends_when_no_keybinds_block() {
        let existing = "theme \"superzej\"\noptions { x true }\n";
        let generated = render_keybinds_kdl(&effective(&Config::default()));
        let out = splice_managed_region(existing, &generated);
        assert!(out.starts_with(BEGIN));
        assert!(out.contains("theme \"superzej\""));
    }

    #[test]
    fn match_brace_ignores_strings_and_comments() {
        let s = "{ bind \"a\" { Run \"x{y}\"; } // a } comment\n }";
        let close = match_brace(s).unwrap();
        assert_eq!(close, s.len() - 1);
        assert!(match_brace("{ unbalanced").is_none());
    }

    #[test]
    fn plugin_wasm_and_url_for_all_variants() {
        for (p, wasm) in [
            (Plugin::Statusbar, "statusbar.wasm"),
            (Plugin::Tabbar, "tabbar.wasm"),
            (Plugin::Sidebar, "sidebar.wasm"),
            (Plugin::Panel, "panel.wasm"),
        ] {
            assert_eq!(p.wasm(), wasm);
            assert_eq!(p.url(), format!("file:~/.local/share/superzej/{wasm}"));
        }
    }

    #[test]
    fn chord_parse_modifier_aliases_and_errors() {
        // Every modifier alias maps to its canonical name.
        assert_eq!(Chord::parse("control x").unwrap().to_kdl(), "Ctrl x");
        assert_eq!(Chord::parse("opt x").unwrap().to_kdl(), "Alt x");
        assert_eq!(Chord::parse("option x").unwrap().to_kdl(), "Alt x");
        assert_eq!(Chord::parse("win x").unwrap().to_kdl(), "Super x");
        assert_eq!(Chord::parse("mod x").unwrap().to_kdl(), "Super x");
        // Shift on a non-letter key keeps an explicit Shift modifier.
        assert_eq!(Chord::parse("shift Left").unwrap().to_kdl(), "Shift Left");
        // Shift on a multi-char key keeps explicit Shift too.
        assert_eq!(Chord::parse("shift Enter").unwrap().to_kdl(), "Shift Enter");
        // Full canonical order: Ctrl Super Alt Shift <key>.
        assert_eq!(
            Chord::parse("shift alt super ctrl Left").unwrap().to_kdl(),
            "Ctrl Super Alt Shift Left"
        );
    }

    #[test]
    fn parse_loose_dangling_modifier_and_single_group() {
        // A trailing modifier with nothing after it → dangling error.
        assert!(Chord::parse_loose("ctrl alt").is_err());
        // A single key-group routes through Chord::parse (canonicalizes).
        assert_eq!(Chord::parse_loose("ctrl k").unwrap().to_kdl(), "Ctrl k");
        // A named key alias as a standalone group.
        assert_eq!(Chord::parse_loose("Enter").unwrap().to_kdl(), "Enter");
    }

    #[test]
    fn effective_warns_and_ignores_unknown_keybind_id() {
        let mut cfg = Config::default();
        cfg.keybinds
            .insert("not-a-real-action".into(), "Alt z".into());
        // Resolution proceeds; the unknown id is simply dropped (warning only).
        let acts = effective(&cfg);
        assert!(acts.iter().all(|a| a.id != "not-a-real-action"));
    }

    #[test]
    fn effective_keeps_default_on_bad_rebind_chord() {
        let mut cfg = Config::default();
        cfg.keybinds.insert("new-worktree".into(), "Bogus q".into());
        let acts = effective(&cfg);
        let nw = acts.iter().find(|a| a.id == "new-worktree").unwrap();
        // The bad rebind is rejected; the builtin default chord is kept.
        assert_eq!(nw.chords[0].to_kdl(), "Alt w");
    }

    #[test]
    fn render_bind_message_plugin_with_payload() {
        // focus-sidebar/focus-panel are MessagePlugin actions with empty payload;
        // add a custom MessagePlugin path via a non-empty payload by constructing
        // a Resolved directly and rendering it.
        let resolved = vec![Resolved {
            id: "x".into(),
            chords: vec![Chord::parse("Alt q").unwrap()],
            menu_label: "x".into(),
            hint: "x".into(),
            invocation: Invocation::MessagePlugin {
                target: Plugin::Panel,
                name: "do_thing",
                payload: "the-payload",
            },
            scope: Scope::Shared,
            contexts: vec![Context::Global],
            priority: 100,
            menu: false,
            custom: false,
        }];
        let kdl = render_keybinds_kdl(&resolved);
        assert!(kdl.contains("name \"do_thing\""), "{kdl}");
        assert!(kdl.contains("payload \"the-payload\""), "{kdl}");
    }

    #[test]
    fn render_bind_bare_run_and_native_and_tab_scope() {
        // A non-floating Run with no options renders single-line.
        // A Tab-scoped binding lands in the `tab {}` block.
        let resolved = vec![
            Resolved {
                id: "bare".into(),
                chords: vec![Chord::parse("Alt b").unwrap()],
                menu_label: "bare".into(),
                hint: "b".into(),
                invocation: Invocation::Run {
                    args: &["dashboard"],
                    floating: false,
                    close_on_exit: false,
                    direction: None,
                },
                scope: Scope::Tab,
                contexts: vec![Context::Global],
                priority: 100,
                menu: false,
                custom: false,
            },
            Resolved {
                id: "n".into(),
                chords: vec![Chord::parse("Alt z").unwrap()],
                menu_label: "n".into(),
                hint: "n".into(),
                invocation: Invocation::Native { body: "Quit;" },
                scope: Scope::Shared,
                contexts: vec![Context::Global],
                priority: 100,
                menu: false,
                custom: false,
            },
        ];
        let kdl = render_keybinds_kdl(&resolved);
        // Bare single-line Run.
        assert!(
            kdl.contains("        bind \"Alt b\" { Run \"superzej\" \"dashboard\"; }\n"),
            "{kdl}"
        );
        // Native single-line.
        assert!(kdl.contains("        bind \"Alt z\" { Quit; }\n"), "{kdl}");
    }

    #[test]
    fn render_falls_back_to_default_new_tab_when_absent() {
        // An action set without `new-tab` still produces the tab-mode `n`
        // override using the default tabbar pipe.
        let resolved = vec![Resolved {
            id: "only".into(),
            chords: vec![Chord::parse("Alt o").unwrap()],
            menu_label: "only".into(),
            hint: "only".into(),
            invocation: Invocation::Native { body: "Quit;" },
            scope: Scope::Shared,
            contexts: vec![Context::Global],
            priority: 100,
            menu: false,
            custom: false,
        }];
        let kdl = render_keybinds_kdl(&resolved);
        assert!(kdl.contains("superzej_new_tab"), "{kdl}");
        assert!(kdl.contains("tabbar.wasm"), "{kdl}");
    }

    #[test]
    fn non_floating_shell_action_renders_single_line() {
        let mut cfg = Config::default();
        cfg.actions.push(crate::config::CustomAction {
            name: "plain".into(),
            key: "Alt P".into(),
            run: Some("ls".into()),
            action: None,
            params: Default::default(),
            menu: false,
            hint: None,
            floating: false,
            close_on_exit: false,
        });
        let kdl = render_keybinds_kdl(&effective(&cfg));
        assert!(
            kdl.contains("        bind \"Alt P\" { Run \"sh\" \"-c\" \"ls\"; }\n"),
            "{kdl}"
        );
    }

    #[test]
    fn splice_replaces_marked_region_and_preserves_tail() {
        let gen1 = render_keybinds_kdl(&effective(&Config::default()));
        // A file that already has the managed markers plus a trailing comment.
        let existing = format!("// top\n{gen1}// bottom comment\n");
        let out = splice_managed_region(&existing, &gen1);
        assert!(out.starts_with("// top\n"));
        assert!(out.contains("// bottom comment"));
        assert!(out.contains(BEGIN) && out.contains(END));
        // Idempotent.
        assert_eq!(splice_managed_region(&out, &gen1), out);
    }
}
