//! `thegn lead login [--mcp <server>]`: log the Lead's agent, or one of its
//! MCP servers, in once on the host (operator only, interactive).
//!
//! Stub: filled by b161 lane 3.
#![allow(dead_code, reason = "filled by b161 lane 3")]

use thegn_core::config::Config;

/// Run the login. Stub: refuses.
pub(crate) fn run(_cfg: &Config, _mcp: Option<&str>) -> anyhow::Result<()> {
    anyhow::bail!("not implemented")
}
