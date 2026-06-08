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

pub fn run(cfg: &Config, tab: Option<String>) -> Result<()> {
    // The Super+K statusbar toggle spawns the palette from a plugin, so the new
    // pane's cwd is NOT the focused worktree (a plugin-spawned pane can't inherit
    // it the way the old `Run` keybind did). `resolve_worktree` is cwd-based and
    // the file/grep sources walk cwd, so chdir into the focused tab's worktree
    // first — resolved from the DB by tab name, exactly as the panel does.
    if let Some(tab) = tab {
        let session = crate::db::session();
        if let Some(path) = crate::commands::resolve::resolve_tab_worktree(&session, &tab) {
            let _ = std::env::set_current_dir(&path);
        }
    }
    crate::palette::run(cfg)
}

/// The legacy external-picker palette, kept as a fallback until the native
/// palette covers every path. Unused while `run` delegates to the iocraft UI.
/// Entries and dispatch are derived from the keymap registry, so it never drifts
/// from the actual bindings.
#[allow(dead_code)]
fn run_external(cfg: &Config) -> Result<()> {
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
#[allow(dead_code)]
fn label(a: &Resolved) -> String {
    match a.chords.first() {
        Some(c) => format!("{:<38} {}", a.menu_label, c.to_hint()),
        None => a.menu_label.clone(),
    }
}

#[allow(dead_code)]
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
        "files" => commands::files::run(cfg, None, None, None, None, false, false),
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
            "files",
            "tool-lazygit",
            "tool-yazi",
            "tool-editor",
            "tool-diff",
            "close-worktree",
            "pr-open",
            "pr-create",
            "focus-sidebar",
            "focus-panel",
            "select-bottombar",
            "select-topbar",
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
