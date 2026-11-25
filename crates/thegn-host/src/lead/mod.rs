//! The Lead (THE-2171): a reserved terminal (`name = "lead"`, `kind = "lead"`)
//! rendered at the top of the sidebar, whose tabs are separate agent
//! conversations driving thegn through a scoped MCP tool surface.
//!
//! Stub created by the b152 spine (lane 1); b152 lane 2 owns this file after it.

pub(crate) mod group;
pub(crate) mod launch;
pub(crate) mod login;
pub(crate) mod mcp;
pub(crate) mod setup;
pub(crate) mod skill;
pub(crate) mod status;
pub(crate) mod wake;

/// Whether the Lead is configured (`[lead]` present after the channel clamp).
#[allow(dead_code, reason = "consumed by later b152 lanes")]
pub(crate) fn enabled(cfg: &thegn_core::config::Config) -> bool {
    cfg.lead.is_some()
}
