//! `thegn env adopt <sprite> --env <S> [--yes] [--json]`: record an existing
//! sprite as this host's, so thegn may later destroy it (env down, env reap,
//! land --release-sandbox, worktree delete and the opt-in lifetime reaper). It
//! asks for confirmation and never touches the sprite itself.
//!
//! b162 spine stub: lane 2 fills in the lookup, the confirmation and the
//! ledger write.

use thegn_core::config::Config;

/// `thegn env adopt` arguments.
#[derive(Debug, Clone, clap::Args)]
pub struct AdoptArgs {
    /// The sprite's name as the Sprites API lists it.
    pub name: String,
    /// The `[env.<name>]` (a sprites env) whose account owns the sprite.
    #[arg(long, value_name = "ENV")]
    pub env: String,
    /// Skip the typed confirmation (required when stdin is not a tty).
    #[arg(long)]
    pub yes: bool,
    /// Emit one JSON object instead of the human summary.
    #[arg(long)]
    pub json: bool,
}

pub fn run(_cfg: &Config, _args: AdoptArgs) -> anyhow::Result<()> {
    anyhow::bail!("env adopt: not implemented in this build")
}
