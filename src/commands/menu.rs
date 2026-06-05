//! `superzej menu` — the Cmd+K command palette: a fuzzy list of superzej
//! actions, the chosen one dispatched to the same command functions the
//! keybinds use. Bound to Super+K and run in a floating pane.

use crate::cli::PrAction;
use crate::commands;
use crate::config::Config;
use crate::picker;
use anyhow::Result;

/// (display label, internal key). The label trails the equivalent keybind so the
/// palette doubles as a discoverable cheatsheet.
const ITEMS: &[(&str, &str)] = &[
    (
        "New workspace — open a repo            Alt-W",
        "new-workspace",
    ),
    (
        "New worktree — branch off the base     Alt-w",
        "new-worktree",
    ),
    ("New panel — split pane                 Alt-n", "new-panel"),
    ("New tab — same worktree                Alt-t", "new-tab"),
    (
        "Switch repo — recents picker           Alt-o",
        "switch-repo",
    ),
    ("Worktree dashboard                     Alt-d", "dashboard"),
    (
        "Toggle sidebar                    Ctrl-Alt-s",
        "toggle-sidebar",
    ),
    (
        "Toggle diff / PR panel            Ctrl-Alt-p",
        "toggle-panel",
    ),
    ("lazygit                                Alt-g", "lazygit"),
    ("yazi — file manager                    Alt-y", "yazi"),
    ("editor                                 Alt-e", "editor"),
    ("git diff                               Alt-/", "diff"),
    (
        "Close worktree (+ its tab)             Alt-X",
        "close-worktree",
    ),
    ("PR — open in browser", "pr-open"),
    ("PR — create (web)", "pr-create"),
];

pub fn run(cfg: &Config) -> Result<()> {
    let labels: Vec<String> = ITEMS.iter().map(|(l, _)| (*l).to_string()).collect();
    let Some(choice) = picker::pick("superzej ❯ ", &labels, &cfg.picker) else {
        return Ok(());
    };
    let key = ITEMS
        .iter()
        .find(|(l, _)| *l == choice)
        .map(|(_, k)| *k)
        .unwrap_or("");
    dispatch(cfg, key)
}

fn dispatch(cfg: &Config, key: &str) -> Result<()> {
    match key {
        "new-workspace" => commands::new_workspace::run(cfg, None, None, true),
        "new-worktree" => commands::new_worktree::run(cfg, None, None, false, None),
        "new-panel" => commands::new_panel::run(cfg, "right", false),
        "new-tab" => commands::new_tab::run(None),
        "switch-repo" => commands::launch::run(cfg),
        "dashboard" => commands::dashboard::run(cfg, false, false),
        "toggle-sidebar" => commands::panels::sidebar(true),
        "toggle-panel" => commands::panels::panel(true),
        "lazygit" => commands::tool::run(cfg, "lazygit", None, None),
        "yazi" => commands::tool::run(cfg, "yazi", None, None),
        "editor" => commands::tool::run(cfg, "editor", None, None),
        "diff" => commands::tool::run(cfg, "diff", None, None),
        "close-worktree" => commands::close_worktree::run(false, false),
        "pr-open" => commands::pr::run(PrAction::Open { worktree: None }),
        "pr-create" => commands::pr::run(PrAction::Create {
            worktree: None,
            title: None,
            body: None,
            base: None,
            draft: false,
            web: true,
            fill: false,
        }),
        _ => Ok(()),
    }
}
