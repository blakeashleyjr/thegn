//! The static action catalog: every superzej command the palette can run,
//! trailing its keybind so the palette doubles as a live cheatsheet. Configured
//! tools (`cfg.tools`) are folded in so custom tools are reachable too.

use crate::config::Config;
use crate::palette::item::{Action, Row};
use crate::theme;

/// Build the full command catalog as result rows.
pub fn rows(cfg: &Config) -> Vec<Row> {
    let mut rows = vec![
        Row::command(
            "✦",
            theme::TEAL,
            "New workspace — open a repo",
            "Alt-W",
            Action::NewWorkspace,
            "new-workspace",
        ),
        Row::command(
            "⎇",
            theme::TEAL,
            "New worktree — branch off the base",
            "Alt-w",
            Action::NewWorktree,
            "new-worktree",
        ),
        Row::command(
            "▤",
            theme::TEAL,
            "New panel — split pane",
            "Alt-n",
            Action::NewPanel,
            "new-panel",
        ),
        Row::command(
            "▦",
            theme::TEAL,
            "New tab — same worktree",
            "Alt-t",
            Action::NewTab,
            "new-tab",
        ),
        Row::command(
            "⇄",
            theme::PURPLE,
            "Switch repo — recents picker",
            "Alt-o",
            Action::SwitchRepo,
            "switch-repo",
        ),
        Row::command(
            "▦",
            theme::AMBER,
            "Worktree dashboard",
            "Alt-d",
            Action::Dashboard,
            "dashboard",
        ),
        Row::command(
            "◧",
            theme::BLUE,
            "Toggle sidebar",
            "Ctrl-Alt-s",
            Action::ToggleSidebar,
            "toggle-sidebar",
        ),
        Row::command(
            "◨",
            theme::BLUE,
            "Toggle diff / PR panel",
            "Ctrl-Alt-p",
            Action::TogglePanel,
            "toggle-panel",
        ),
        Row::command(
            "✕",
            theme::RED,
            "Close worktree (+ its tab)",
            "Alt-X",
            Action::CloseWorktree,
            "close-worktree",
        ),
        Row::command(
            "⬡",
            theme::GREEN,
            "PR — open in browser",
            "",
            Action::PrOpen,
            "pr-open",
        ),
        Row::command(
            "⬡",
            theme::GREEN,
            "PR — create (web)",
            "",
            Action::PrCreate,
            "pr-create",
        ),
        Row::command(
            "⬡",
            theme::GREEN,
            "PR — status / checks",
            "",
            Action::PrStatus,
            "pr-status",
        ),
        Row::command(
            "⬡",
            theme::MAGENTA,
            "PR — approve",
            "",
            Action::PrApprove,
            "pr-approve",
        ),
        Row::command(
            "⬡",
            theme::MAGENTA,
            "PR — merge (squash)",
            "",
            Action::PrMerge,
            "pr-merge",
        ),
        Row::command(
            "⬡",
            theme::MAGENTA,
            "PR — re-run failed checks",
            "",
            Action::PrRerun,
            "pr-rerun",
        ),
        Row::command(
            "🎨",
            theme::TEAL,
            "Theme: Preview accents",
            "",
            Action::ThemePreview,
            "theme-preview",
        ),
    ];

    // Configured tools (lazygit/yazi/editor/diff by default), keyed by name so
    // their identity hue/glyph match the rest of superzej.
    for t in &cfg.tools {
        let hue = theme::agent_hue(&t.name);
        let glyph = theme::agent_glyph(&t.name);
        let keybind = match t.name.as_str() {
            "lazygit" => "Alt-g",
            "yazi" => "Alt-y",
            "editor" => "Alt-e",
            "diff" => "Alt-/",
            _ => "",
        };
        rows.push(Row::command(
            &glyph,
            hue,
            &t.name,
            keybind,
            Action::Tool(t.name.clone()),
            &format!("tool:{}", t.name),
        ));
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::NamedCommand;
    use std::collections::HashSet;

    #[test]
    fn catalog_is_non_empty_with_unique_keys() {
        let rows = rows(&Config::default());
        assert!(rows.len() >= 15, "expected the full action catalog");
        let keys: HashSet<_> = rows.iter().filter_map(|r| r.frecency_key.clone()).collect();
        assert_eq!(keys.len(), rows.len(), "frecency keys must be unique");
        // A representative command is present and labelled as a cheatsheet entry.
        let toggle = rows
            .iter()
            .find(|r| r.label.contains("Toggle sidebar"))
            .unwrap();
        assert_eq!(toggle.detail, "Ctrl-Alt-s");
    }

    #[test]
    fn configured_tools_become_tool_rows() {
        let mk = |n: &str| NamedCommand {
            name: n.into(),
            command: n.into(),
            hints: vec![],
        };
        // All four known tools (each has a distinct keybind) plus an unknown one
        // (no keybind) — covers every keybind arm.
        let cfg = Config {
            tools: vec![
                mk("lazygit"),
                mk("yazi"),
                mk("editor"),
                mk("diff"),
                mk("custom"),
            ],
            ..Config::default()
        };
        let rows = rows(&cfg);
        let by = |label: &str| rows.iter().find(|r| r.label == label).unwrap();
        assert_eq!(by("lazygit").detail, "Alt-g");
        assert_eq!(by("yazi").detail, "Alt-y");
        assert_eq!(by("editor").detail, "Alt-e");
        assert_eq!(by("diff").detail, "Alt-/");
        assert_eq!(by("custom").detail, ""); // unknown tools get no keybind hint
        let lg = by("lazygit");
        assert!(matches!(lg.action, Action::Tool(ref n) if n == "lazygit"));
        assert_eq!(lg.frecency_key.as_deref(), Some("tool:lazygit"));
    }
}
