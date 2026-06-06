pub mod activity;
pub mod attach;
pub mod close_worktree;
pub mod dashboard;
pub mod diff;
pub mod files;
pub mod grant_plugins;
pub mod launch;
pub mod list;
pub mod menu;
pub mod monitor;
pub mod new_panel;
pub mod new_tab;
pub mod new_workspace;
pub mod new_worktree;
pub mod open_worktree;
pub mod panels;
pub mod pick_agent;
pub mod pr;
pub mod recent;
pub mod repos;
pub mod resolve;
pub mod restore;
pub mod snapshot;
pub mod stats;
pub mod status;
pub mod theme;
pub mod tool;
pub mod watch;
pub mod workspaces;
pub mod worktrees;

use crate::{repo, util};
use std::path::PathBuf;
use std::process::Command;

/// Resolve the worktree a command targets: explicit arg, else $SUPERZEJ_WORKTREE,
/// else the git toplevel of the cwd, else the cwd. Mirrors `tool.rs`.
pub fn resolve_worktree(arg: Option<String>) -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    arg.map(PathBuf::from)
        .or_else(|| std::env::var("SUPERZEJ_WORKTREE").ok().map(PathBuf::from))
        .or_else(|| repo::toplevel(&cwd))
        .unwrap_or(cwd)
}

/// Yes/no confirmation (gum if present, else a y/N stdin prompt).
pub fn confirm(message: &str) -> bool {
    if util::have("gum") {
        return Command::new("gum")
            .args(["confirm", message])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
    }
    eprint!("{message} [y/N] ");
    use std::io::{BufRead, Write};
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    if std::io::stdin().lock().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim(), "y" | "Y" | "yes" | "YES")
}
