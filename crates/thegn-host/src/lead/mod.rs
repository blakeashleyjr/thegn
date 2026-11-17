//! The Lead (THE-2171): a reserved terminal (`name = "lead"`, `kind = "lead"`)
//! rendered at the top of the sidebar, whose tabs are separate agent
//! conversations driving thegn through a scoped MCP tool surface.
//!
//! Stub created by the b152 spine (lane 1); b152 lane 2 owns this file after it.

pub(crate) mod group;
pub(crate) mod launch;
pub(crate) mod mcp;
pub(crate) mod skill;

/// Whether the Lead is configured and available in this build. Stub: never.
#[allow(dead_code, reason = "filled by b152 lane 2")]
pub(crate) fn enabled(_cfg: &thegn_core::config::Config) -> bool {
    false
}
