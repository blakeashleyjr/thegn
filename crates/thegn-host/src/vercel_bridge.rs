//! `thegn vercel-exec <id> -- cmd…`: the Vercel Sandbox exec self-bridge (the
//! `sprite-exec` role for a sandbox with no vendor CLI and no ssh). Capture
//! mode (non-tty stdin) runs the command and relays its output without ever
//! resuming a stopped sandbox; tty mode resumes and streams the
//! `/interactive` PTY, falling back to stream mode. The token is resolved
//! in-process, never passed in argv or the environment.

use thegn_core::config::Config;

/// Run the bridge. Spine stub, filled by lane 6.
pub fn run(_cfg: &Config, _id: &str, _cmd: &[String]) -> anyhow::Result<()> {
    anyhow::bail!("vercel-exec: not built")
}
