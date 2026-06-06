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
    Sidebar,
    Statusbar,
    Tabbar,
}

impl Plugin {
    fn wasm(self) -> &'static str {
        match self {
            Plugin::Sidebar => "sidebar.wasm",
            Plugin::Statusbar => "statusbar.wasm",
            Plugin::Tabbar => "tabbar.wasm",
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
    /// A raw vanilla-zellij action body, e.g. `MoveFocus "Left";`.
    Native { body: &'static str },
    /// A user-defined shell command (`config.toml [[actions]]`).
    Shell {
        run: String,
        floating: bool,
        close_on_exit: bool,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Context {
    Always,
    WorktreeOnly,
    NonWorktree,
}

/// A registry entry (static builtin form).
pub struct Action {
    pub id: &'static str,
    pub chords: &'static [&'static str],
    pub menu_label: &'static str,
    pub hint: &'static str,
    pub invocation: Invocation,
    pub scope: Scope,
    pub context: Context,
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
    pub context: Context,
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
        context: Context::Always,
        menu: true,
    },
    Action {
        id: "close-worktree",
        chords: &["Alt X"],
        menu_label: "Close worktree (+ its tab)",
        hint: "close",
        invocation: run_float!("close-worktree"),
        scope: Scope::Shared,
        context: Context::WorktreeOnly,
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
        context: Context::Always,
        menu: true,
    },
    Action {
        id: "new-workspace",
        chords: &["Alt W"],
        menu_label: "New workspace — open a repo",
        hint: "new repo",
        invocation: run_float!("new-workspace"),
        scope: Scope::Shared,
        context: Context::NonWorktree,
        menu: true,
    },
    Action {
        id: "menu",
        chords: &["Super k"],
        menu_label: "Command palette",
        hint: "menu",
        invocation: run_float!("menu"),
        scope: Scope::Shared,
        context: Context::Always,
        menu: false,
    },
    Action {
        id: "new-panel-native",
        chords: &["Alt n"],
        menu_label: "New panel — split pane",
        hint: "split",
        invocation: Invocation::Native {
            body: "NewPane \"Down\";",
        },
        scope: Scope::Shared,
        context: Context::Always,
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
        context: Context::Always,
        menu: false,
    },
    Action {
        id: "switch-repo",
        chords: &["Alt o"],
        menu_label: "Switch repo — recents picker",
        hint: "switch repo",
        invocation: run_float!("launch"),
        scope: Scope::Shared,
        context: Context::NonWorktree,
        menu: true,
    },
    Action {
        id: "dashboard",
        chords: &["Alt d"],
        menu_label: "Worktree dashboard",
        hint: "dashboard",
        invocation: run!("dashboard"),
        scope: Scope::Shared,
        context: Context::NonWorktree,
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
        context: Context::Always,
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
        context: Context::Always,
        menu: true,
    },
    Action {
        id: "tool-lazygit",
        chords: &["Alt g"],
        menu_label: "lazygit",
        hint: "lazygit",
        invocation: run!("tool", "lazygit"),
        scope: Scope::Shared,
        context: Context::WorktreeOnly,
        menu: true,
    },
    Action {
        id: "tool-yazi",
        chords: &["Alt y"],
        menu_label: "yazi — file manager",
        hint: "files",
        invocation: run!("tool", "yazi"),
        scope: Scope::Shared,
        context: Context::WorktreeOnly,
        menu: true,
    },
    Action {
        id: "tool-editor",
        chords: &["Alt e"],
        menu_label: "editor",
        hint: "edit",
        invocation: run!("tool", "editor"),
        scope: Scope::Shared,
        context: Context::WorktreeOnly,
        menu: true,
    },
    Action {
        id: "tool-diff",
        chords: &["Alt /"],
        menu_label: "git diff",
        hint: "diff",
        invocation: run!("tool", "diff"),
        scope: Scope::Shared,
        context: Context::WorktreeOnly,
        menu: true,
    },
    Action {
        id: "prev-tab",
        chords: &["Alt Left"],
        menu_label: "Previous tab",
        hint: "tabs",
        invocation: Invocation::Native {
            body: "GoToPreviousTab;",
        },
        scope: Scope::Shared,
        context: Context::Always,
        menu: false,
    },
    Action {
        id: "next-tab",
        chords: &["Alt Right"],
        menu_label: "Next tab",
        hint: "tabs",
        invocation: Invocation::Native {
            body: "GoToNextTab;",
        },
        scope: Scope::Shared,
        context: Context::Always,
        menu: false,
    },
    Action {
        id: "focus-left",
        chords: &["Alt h", "Super Alt Left", "Super Alt h"],
        menu_label: "Focus pane left",
        hint: "panes",
        invocation: Invocation::Native {
            body: "MoveFocus \"Left\";",
        },
        scope: Scope::Shared,
        context: Context::Always,
        menu: false,
    },
    Action {
        id: "focus-down",
        chords: &["Alt j"],
        menu_label: "Focus pane down",
        hint: "panes",
        invocation: Invocation::Native {
            body: "MoveFocus \"Down\";",
        },
        scope: Scope::Shared,
        context: Context::Always,
        menu: false,
    },
    Action {
        id: "focus-up",
        chords: &["Alt k"],
        menu_label: "Focus pane up",
        hint: "panes",
        invocation: Invocation::Native {
            body: "MoveFocus \"Up\";",
        },
        scope: Scope::Shared,
        context: Context::Always,
        menu: false,
    },
    Action {
        id: "focus-right",
        chords: &["Alt l", "Super Alt Right", "Super Alt l"],
        menu_label: "Focus pane right",
        hint: "panes",
        invocation: Invocation::Native {
            body: "MoveFocus \"Right\";",
        },
        scope: Scope::Shared,
        context: Context::Always,
        menu: false,
    },
    Action {
        id: "nav-down",
        chords: &["Super Alt Down", "Super Alt j"],
        menu_label: "Sidebar selection down",
        hint: "nav",
        invocation: Invocation::Pipe {
            plugin: Plugin::Sidebar,
            name: "superzej_nav_down",
        },
        scope: Scope::Shared,
        context: Context::Always,
        menu: false,
    },
    Action {
        id: "nav-up",
        chords: &["Super Alt Up", "Super Alt k"],
        menu_label: "Sidebar selection up",
        hint: "nav",
        invocation: Invocation::Pipe {
            plugin: Plugin::Sidebar,
            name: "superzej_nav_up",
        },
        scope: Scope::Shared,
        context: Context::Always,
        menu: false,
    },
    // Menu-only actions (no default chord).
    Action {
        id: "pr-open",
        chords: &[],
        menu_label: "PR — open in browser",
        hint: "pr",
        invocation: run!("pr", "open"),
        scope: Scope::Shared,
        context: Context::WorktreeOnly,
        menu: true,
    },
    Action {
        id: "pr-create",
        chords: &[],
        menu_label: "PR — create (web)",
        hint: "pr",
        invocation: run!("pr", "create", "--web"),
        scope: Scope::Shared,
        context: Context::WorktreeOnly,
        menu: true,
    },
];

/// Vanilla-zellij chords superzej deliberately keeps; a user override landing on
/// one is flagged by `keys validate`.
const RESERVED: &[&str] = &[
    "Alt [", "Alt ]", "Ctrl p", "Ctrl t", "Ctrl n", "Ctrl s", "Ctrl o", "Ctrl h", "Ctrl g",
    "Ctrl b", "Ctrl q",
];

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
            context: a.context,
            menu: a.menu,
            custom: false,
        })
        .collect();

    // [keybinds] — rebind a builtin by id (whole chord set replaced).
    for (id, chord) in &cfg.keybinds {
        match out.iter_mut().find(|r| &r.id == id) {
            Some(r) => match Chord::parse(chord) {
                Ok(c) => r.chords = vec![c],
                Err(e) => crate::msg::warn(&format!("[keybinds] {id}: {e}; keeping default")),
            },
            None => crate::msg::warn(&format!("[keybinds] unknown action {id:?}; ignored")),
        }
    }

    // [[actions]] — user-defined shell actions (rendered as `Run sh -c`).
    for a in &cfg.actions {
        let chords = match Chord::parse(&a.key) {
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
            invocation: Invocation::Shell {
                run: a.run.clone(),
                floating: a.floating,
                close_on_exit: a.close_on_exit,
            },
            scope: Scope::Shared,
            context: Context::Always,
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

fn render_invocation(inv: &Invocation) -> String {
    match inv {
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
            let mut opts = Vec::new();
            if *floating {
                opts.push("floating true".to_string());
            }
            if *close_on_exit {
                opts.push("close_on_exit true".to_string());
            }
            if let Some(d) = direction {
                opts.push(format!("direction \"{d}\""));
            }
            if opts.is_empty() {
                format!("Run \"superzej\" {argv};")
            } else {
                format!("Run \"superzej\" {argv} {{ {} }}", opts.join("; "))
            }
        }
        Invocation::Shell {
            run,
            floating,
            close_on_exit,
        } => {
            let mut opts = Vec::new();
            if *floating {
                opts.push("floating true".to_string());
            }
            if *close_on_exit {
                opts.push("close_on_exit true".to_string());
            }
            let esc = run.replace('\\', "\\\\").replace('"', "\\\"");
            if opts.is_empty() {
                format!("Run \"sh\" \"-c\" \"{esc}\";")
            } else {
                format!("Run \"sh\" \"-c\" \"{esc}\" {{ {} }}", opts.join("; "))
            }
        }
        Invocation::Pipe { plugin, name } => {
            format!("MessagePlugin \"{}\" {{ name \"{name}\"; }}", plugin.url())
        }
        Invocation::Native { body } => body.to_string(),
    }
}

/// Render just the `keybinds {}` block (between the markers) from the effective
/// registry. Byte-stable for a given registry, so it diffs cleanly.
pub fn render_keybinds_kdl(actions: &[Resolved]) -> String {
    let mut shared = String::new();
    let mut tab = String::new();
    for a in actions {
        for c in &a.chords {
            let line = format!(
                "        bind \"{}\" {{ {} }}\n",
                c.to_kdl(),
                render_invocation(&a.invocation)
            );
            match a.scope {
                Scope::Shared => shared.push_str(&line),
                Scope::Tab => tab.push_str(&line),
            }
        }
    }
    // The tab-mode `n` override always repoints new-tab (+ returns to Normal).
    let new_tab_pipe = actions
        .iter()
        .find(|a| a.id == "new-tab")
        .map(|a| match &a.invocation {
            Invocation::Pipe { plugin, name } => {
                format!("MessagePlugin \"{}\" {{ name \"{name}\"; }}", plugin.url())
            }
            _ => "MessagePlugin \"file:~/.local/share/superzej/tabbar.wasm\" { name \"superzej_new_tab\"; }".into(),
        })
        .unwrap_or_default();

    let mut out = String::new();
    out.push_str(BEGIN);
    out.push('\n');
    out.push_str("keybinds {\n");
    out.push_str("    shared_except \"locked\" {\n");
    out.push_str(&shared);
    out.push_str("    }\n");
    out.push_str("    tab {\n");
    out.push_str(&format!(
        "        bind \"n\" {{ {new_tab_pipe} SwitchToMode \"Normal\"; }}\n"
    ));
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
    if let Some(start) = existing.find("keybinds") {
        if let Some(open) = existing[start..].find('{') {
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
        assert!(kdl.contains("bind \"Alt w\" { Run \"superzej\" \"new-worktree\""));
        assert!(kdl.contains("bind \"Ctrl Alt s\" { MessagePlugin \"file:~/.local/share/superzej/statusbar.wasm\" { name \"superzej_toggle_sidebar\"; } }"));
        assert!(kdl.contains("bind \"Alt h\" { MoveFocus \"Left\"; }"));
        // tab-mode override present
        assert!(kdl.contains("tab {"));
        assert!(kdl.contains("SwitchToMode \"Normal\""));
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
            run: "just deploy".into(),
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
        cfg.keybinds.insert("switch-repo".into(), "Alt [".into()); // reserved
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
            run: "echo \"hi\"".into(),
            menu: false,
            hint: Some("dep".into()),
            floating: true,
            close_on_exit: true,
        });
        let kdl = render_keybinds_kdl(&effective(&cfg));
        assert!(kdl.contains("bind \"Alt D\" { Run \"sh\" \"-c\""));
        assert!(kdl.contains("echo \\\"hi\\\""), "quotes escaped: {kdl}");
        // the scoped panel binding carries its direction option.
        assert!(kdl.contains("\"new-panel\" \"--in-place\" { direction \"Right\" }"));
    }

    #[test]
    fn bad_custom_chord_is_skipped() {
        let mut cfg = Config::default();
        cfg.actions.push(crate::config::CustomAction {
            name: "broke".into(),
            key: "Wat x".into(), // unknown modifier
            run: "true".into(),
            menu: true,
            hint: None,
            floating: false,
            close_on_exit: false,
        });
        let acts = effective(&cfg);
        assert!(acts.iter().all(|a| a.id != "broke"));
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
}
