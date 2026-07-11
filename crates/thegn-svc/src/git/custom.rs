//! User-defined git commands (`[[git_commands]]`): the host collects prompt
//! responses, thegn-core's `custom_cmd::expand` renders the template
//! against the current selection, and this runs the result in the worktree.
//! `terminal` output mode is executed by the host's floating-pane machinery
//! instead — this seam only covers capture (`popup`) and fire-and-forget.

use super::GitBackend;
use anyhow::{Context, Result, bail};
use thegn_core::remote::GitLoc;

pub trait CustomOps: GitBackend {
    /// Run an expanded command line, capturing combined output for the
    /// popup. Non-zero exit becomes an error carrying the tail of stderr.
    fn run_custom(&self, loc: &GitLoc, command: &str) -> Result<String> {
        let out = loc
            .sh_command(command)
            .stdin(std::process::Stdio::null())
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .with_context(|| format!("run custom command: {command}"))?;
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            let tail: Vec<&str> = err.trim().lines().rev().take(4).collect();
            let tail: Vec<&str> = tail.into_iter().rev().collect();
            bail!("command failed: {}", tail.join(" · "));
        }
        Ok(stdout)
    }
}

impl<T: GitBackend + ?Sized> CustomOps for T {}
