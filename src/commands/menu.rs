//! `superzej menu` — the Cmd+K command palette. The entries (label + key hint)
//! are derived from the keybind registry (`src/keymap.rs`), so the palette and
//! the actual bindings can never drift. The chosen action is dispatched to the
//! same command functions the keys use. Bound to Super+K, run in a floating pane.

use crate::cli::PrAction;
use crate::commands;
use crate::config::Config;
use crate::keymap::{self, Invocation, Resolved};
use crate::picker;
use anyhow::Result;
use std::process::Command;

pub fn run(cfg: &Config) -> Result<()> {
    let actions: Vec<Resolved> = keymap::effective(cfg)
        .into_iter()
        .filter(|a| a.menu)
        .collect();
    let labels: Vec<String> = actions.iter().map(label).collect();
    let Some(choice) = picker::pick("superzej ❯ ", &labels, cfg.picker.as_str()) else {
        return Ok(());
    };
    let Some(idx) = labels.iter().position(|l| *l == choice) else {
        return Ok(());
    };
    dispatch(cfg, &actions[idx])
}

/// A right-aligned key hint trailing the label, so the palette doubles as a
/// discoverable cheatsheet.
fn label(a: &Resolved) -> String {
    match a.chords.first() {
        Some(c) => format!("{:<38} {}", a.menu_label, c.to_hint()),
        None => a.menu_label.clone(),
    }
}

fn dispatch(cfg: &Config, a: &Resolved) -> Result<()> {
    // Custom user actions run their shell command directly.
    if let Invocation::Shell { run, .. } = &a.invocation {
        let _ = Command::new(crate::util::shell())
            .arg("-lc")
            .arg(run)
            .status();
        return Ok(());
    }
    // Built-ins dispatch by stable id to the same fns the keybinds call.
    match a.id.as_str() {
        "new-workspace" => commands::new_workspace::run(cfg, None, None, true),
        "new-worktree" => commands::new_worktree::run(cfg, None, None, false, None),
        "new-panel" | "new-panel-native" => commands::new_panel::run(cfg, "right", false),
        "new-tab" => commands::new_tab::run(None),
        "switch-repo" => commands::launch::run(cfg),
        "dashboard" => commands::dashboard::run(cfg, false, false),
        "toggle-sidebar" => commands::panels::sidebar(true),
        "toggle-panel" => commands::panels::panel(true),
        "tool-lazygit" => commands::tool::run(cfg, "lazygit", None, None),
        "tool-yazi" => commands::tool::run(cfg, "yazi", None, None),
        "tool-editor" => commands::tool::run(cfg, "editor", None, None),
        "tool-diff" => commands::tool::run(cfg, "diff", None, None),
        "close-worktree" => commands::close_worktree::run(cfg, false, false),
        "pr-open" => commands::pr::run(cfg, PrAction::Open { worktree: None }),
        "pr-create" => commands::pr::run(
            cfg,
            PrAction::Create {
                worktree: None,
                title: None,
                body: None,
                base: None,
                draft: false,
                web: true,
                fill: false,
            },
        ),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Every menu-visible builtin must have a dispatch arm (else the palette
    // would silently no-op). Custom actions dispatch via Shell, so skip them.
    #[test]
    fn every_menu_builtin_dispatches() {
        let known = [
            "new-workspace",
            "new-worktree",
            "new-panel",
            "new-panel-native",
            "new-tab",
            "switch-repo",
            "dashboard",
            "toggle-sidebar",
            "toggle-panel",
            "tool-lazygit",
            "tool-yazi",
            "tool-editor",
            "tool-diff",
            "close-worktree",
            "pr-open",
            "pr-create",
        ];
        for a in keymap::BUILTINS.iter().filter(|a| a.menu) {
            assert!(
                known.contains(&a.id),
                "menu builtin {:?} has no dispatch arm",
                a.id
            );
        }
    }
}
