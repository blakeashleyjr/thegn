//! `thegn lead pause|resume|status` (b155 6f): the Lead kill switch.
//!
//! `pause` = `automations.pause`, then `automations.halt` (unless
//! `--keep-sessions`); `resume` = `automations.resume` (Admin; the Lead
//! cannot); `status` prints the flag, budget usage, pending approvals and
//! live automation lanes.
//!
//! Spine stub: b155 lane 7 fills it.

use anyhow::Result;
use thegn_core::config::Config;

#[derive(Debug, Clone, clap::Subcommand)]
pub enum Action {
    /// Pause every automation rule and kill automation-started sessions.
    Pause {
        /// Pause only; leave running automation and Lead-opened sessions alone.
        #[arg(long)]
        keep_sessions: bool,
    },
    /// Resume paused automations (operator only).
    Resume,
    /// Show the pause flag, budget usage, pending approvals and live lanes.
    Status {
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
}

#[allow(dead_code, reason = "filled by b155 lane 7")]
pub fn run(_cfg: &Config, _a: Action) -> Result<()> {
    anyhow::bail!("not implemented")
}
