//! `thegn env reap --provider vercel [--apply] [--yes] [--json]`: find Vercel
//! sandboxes this host's controller created (tag proof) that the local ledger
//! no longer records. Dry-run unless `--apply`; never runs automatically.
//!
//! Spine stub (b156 lane 1): the signature is fixed; lane 5 fills the body.

use anyhow::{Result, bail};
use thegn_core::config::Config;

pub fn run(_cfg: &Config, provider: &str, _apply: bool, _yes: bool, _json: bool) -> Result<()> {
    if !provider.trim().eq_ignore_ascii_case("vercel") {
        bail!("env reap supports --provider vercel only");
    }
    bail!("env reap is not available in this build")
}
