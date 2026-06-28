//! Non-interactive CLI verbs folded into the single `superzej`(=`szhost`) binary.
//!
//! These are the user-facing commands that used to live in the standalone
//! `superzej-cli` crate and had no zellij coupling — `pr`, `issue`, `diff`,
//! `list`, `repos`, `recent`, `config`. The plugin-bridge commands (status/
//! stats/theme/hints/workspaces/worktrees/snapshot/activity) were deleted with
//! the zellij substrate: the native host computes all of that in-process.
//!
//! Each verb is a thin shell over `superzej-core`; `run.rs` (the compositor) is
//! the default when no subcommand is given.

pub mod ci;
pub mod config;
pub mod diff;
pub mod disk;
pub mod doctor;
pub mod env;
pub mod integrate;
pub mod issue;
pub mod list;
pub mod logs;
pub mod notify;
pub mod pr;
pub mod repos;
pub mod share;
pub mod theme;

use std::path::PathBuf;
use std::process::Command;

/// Resolve the worktree a command targets: explicit arg, else `$SUPERZEJ_WORKTREE`,
/// else the git toplevel of the cwd, else the cwd.
pub fn resolve_worktree(arg: Option<String>) -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    arg.map(PathBuf::from)
        .or_else(|| std::env::var("SUPERZEJ_WORKTREE").ok().map(PathBuf::from))
        .or_else(|| superzej_core::repo::toplevel(&cwd))
        .unwrap_or(cwd)
}

/// Yes/no confirmation (gum if present, else a y/N stdin prompt).
#[allow(clippy::disallowed_macros)] // a raw interactive prompt, not a log line
pub fn confirm(message: &str) -> bool {
    if superzej_core::util::have("gum") {
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
