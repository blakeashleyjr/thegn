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

pub mod agent;
pub mod ci;
pub mod config;
pub mod debug;
pub mod diff;
pub mod disk;
pub mod doctor;
pub mod env;
pub mod env_image;
pub mod forward;
pub mod host;
pub mod integrate;
pub mod issue;
pub mod list;
pub mod logs;
pub mod mcp;
pub mod notify;
pub mod open;
pub mod placement;
pub mod pr;
pub mod repos;
pub mod share;
pub mod theme;
pub mod wt;
pub mod zone;

use std::path::PathBuf;
use std::process::Command;

/// Exit-code contract for scripting. `anyhow` errors default to [`EXIT_ERROR`];
/// commands opt into the other codes deliberately (retryable via an explicit
/// `std::process::exit`, not-found via the [`NotFound`] error downcast in
/// `main`). Scripts branch on these — treat them as a stable API.
pub const EXIT_OK: i32 = 0;
/// Generic failure.
pub const EXIT_ERROR: i32 = 1;
/// Transient/retryable failure (e.g. a host provision step worth re-running).
pub const EXIT_RETRYABLE: i32 = 2;
/// The named target (repo, worktree, branch, …) does not exist.
pub const EXIT_NOT_FOUND: i32 = 3;

/// Typed "target does not exist" error: `bail!`-compatible via `anyhow`, and
/// downcast in `main()` to map the process exit code to [`EXIT_NOT_FOUND`]
/// while cmd functions stay plain `anyhow::Result`.
#[derive(Debug)]
pub struct NotFound(pub String);

impl std::fmt::Display for NotFound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for NotFound {}

/// Emit one machine-readable JSON document (compact, single line, no ANSI) on
/// stdout. The `--json` convention for list-shaped read commands: exactly one
/// document per invocation, shape treated as a stable API. (`notify list
/// --json` predates this and stays NDJSON; `doctor --json` keeps its object.)
pub fn emit_json<T: serde::Serialize>(value: &T) -> anyhow::Result<()> {
    superzej_core::outln!("{}", serde_json::to_string(value)?);
    Ok(())
}

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
        // CLI path: interactive confirm prompt, no event loop.
        #[expect(clippy::disallowed_methods)]
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
