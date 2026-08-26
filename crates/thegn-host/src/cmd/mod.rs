//! Non-interactive CLI verbs folded into the single `thegn`(=`thegn`) binary.
//!
//! These are the user-facing commands that used to live in the standalone
//! `thegn-cli` crate and had no zellij coupling — `pr`, `issue`, `diff`,
//! `list`, `repos`, `recent`, `config`. The plugin-bridge commands (status/
//! stats/theme/hints/workspaces/worktrees/snapshot/activity) were deleted with
//! the zellij substrate: the native host computes all of that in-process.
//!
//! Each verb is a thin shell over `thegn-core`; `run.rs` (the compositor) is
//! the default when no subcommand is given.

pub mod agent;
pub mod api;
pub mod attach;
pub mod bundle;
pub mod ci;
pub mod config;
pub mod daemon;
pub mod debug;
pub mod diff;
pub mod disk;
pub mod dispatch;
pub mod doctor;
pub mod env;
pub mod env_image;
pub mod forward;
pub mod host;
pub mod integrate;
pub mod issue;
pub mod kaneo;
pub mod keys;
pub mod land;
pub mod list;
pub mod logs;
pub mod map;
pub mod mcp;
pub mod mcp_proxy_cmd;
pub mod merge;
pub mod notify;
pub mod open;
pub mod pair;
pub mod placement;
pub mod plugin;
pub mod pr;
pub mod pr_queue;
pub mod project;
pub mod proxy;
pub mod repos;
pub mod sandbox;
pub mod search;
pub mod secret;
pub mod session;
pub mod share;
pub mod target;
pub mod theme;
pub mod wt;
pub mod zone;

use std::path::{Path, PathBuf};
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
    thegn_core::outln!("{}", serde_json::to_string(value)?);
    Ok(())
}

/// Resolve the worktree a command targets: explicit arg, else `$THEGN_WORKTREE`,
/// else the git toplevel of the cwd, else the cwd.
///
/// `$THEGN_WORKTREE` is the **host-canonical** path the parent thegn injects. In
/// a local sandbox the worktree is bind-mounted at that same real path, so it
/// exists and is used as-is. On a **remote sprite** the worktree is mounted at a
/// *different* local path (e.g. `/home/sprite/workspace`), so the host path does
/// not exist here — trusting it blindly breaks every worktree-scoped command
/// (`merge add`, `land`, `wt`, `disk`, …). Guard on existence so a stale host
/// path falls through to the git toplevel of the cwd; remote sprites then work
/// out of the box.
pub fn resolve_worktree(arg: Option<String>) -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let env_wt = std::env::var("THEGN_WORKTREE").ok();
    resolve_worktree_from(arg, env_wt, &cwd)
}

/// Pure core of [`resolve_worktree`] (env/cwd read out) so the fallthrough is
/// unit-testable. A `$THEGN_WORKTREE` that doesn't exist locally (a remote
/// sprite's host path) is skipped in favor of the cwd's git toplevel.
fn resolve_worktree_from(arg: Option<String>, env_wt: Option<String>, cwd: &Path) -> PathBuf {
    arg.map(PathBuf::from)
        .or_else(|| env_wt.map(PathBuf::from).filter(|p| p.exists()))
        .or_else(|| thegn_core::repo::toplevel(cwd))
        .unwrap_or_else(|| cwd.to_path_buf())
}

/// Yes/no confirmation (gum if present, else a y/N stdin prompt).
#[allow(clippy::disallowed_macros)] // a raw interactive prompt, not a log line
pub fn confirm(message: &str) -> bool {
    if thegn_core::util::have("gum") {
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

#[cfg(test)]
mod resolve_worktree_tests {
    use super::resolve_worktree_from;
    use std::path::{Path, PathBuf};

    #[test]
    fn explicit_arg_wins() {
        let got = resolve_worktree_from(
            Some("/some/arg".into()),
            Some("/env".into()),
            Path::new("/cwd"),
        );
        assert_eq!(got, PathBuf::from("/some/arg"));
    }

    #[test]
    fn existing_env_worktree_is_used() {
        // A $THEGN_WORKTREE that exists (a local sandbox's bind-mounted path).
        let dir = std::env::temp_dir();
        let got = resolve_worktree_from(
            None,
            Some(dir.to_string_lossy().into()),
            Path::new("/nonexistent/cwd"),
        );
        assert_eq!(got, dir);
    }

    #[test]
    fn nonexistent_env_worktree_falls_through() {
        // The remote-sprite case: the host path isn't mounted here, so it must be
        // ignored rather than trusted. The cwd (a temp dir, not a git repo) has no
        // toplevel, so resolution lands on the cwd itself — never the dead path.
        let cwd = std::env::temp_dir();
        let dead = "/definitely/not/here/thegn-xyz";
        let got = resolve_worktree_from(None, Some(dead.into()), &cwd);
        assert_ne!(got, PathBuf::from(dead));
        assert_eq!(got, cwd);
    }
}
