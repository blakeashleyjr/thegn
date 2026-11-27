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

/// One-line explanation shown on the sidebar when Lead cannot be launched.
/// Stable's explicit preview marker is the documented exception to the
/// channel registry's dev-only gate.
pub(crate) fn disabled_reason(cfg: &thegn_core::config::Config) -> Option<String> {
    let channel = crate::channel_state::current();
    let preview = cfg.lead.as_ref().is_some_and(|lead| lead.preview);
    if !thegn_core::channel::Feature::Lead.allowed_in(channel)
        && !(channel == thegn_core::channel::Channel::Stable && preview)
    {
        return Some("disabled on stable; press Alt i to set up the preview".into());
    }
    if !thegn_core::lead_sandbox::bwrap_available() {
        return Some("disabled: install bubblewrap, then press Alt i".into());
    }
    if !thegn_core::lead_net::pasta_available() {
        return Some("disabled: install pasta, then press Alt i".into());
    }
    if !enabled(cfg) {
        return None;
    }
    None
}
