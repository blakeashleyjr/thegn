//! The Lead's MCP wiring (THE-2171): a host-minted, scoped pairing token on a
//! dedicated loopback listener, the `thegn mcp proxy --profile` hub, and
//! `thegn mcp wire --lead`.
//!
//! Stub created by the b152 spine (lane 1); filled by b152 lane 9.

/// What a Lead tab's agent needs for its MCP server: extra agent argv, env,
/// and paths the sandbox must leave readable.
#[allow(dead_code, reason = "filled by b152 lane 9")]
#[derive(Debug, Clone, Default)]
pub(crate) struct LeadMcpPlan {
    pub agent_args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub readable: Vec<std::path::PathBuf>,
}

/// Prepare the MCP plan for one Lead tab. Stub: an empty plan.
#[allow(dead_code, reason = "filled by b152 lane 9")]
pub(crate) fn prepare(
    _cfg: &thegn_core::config::Config,
    _notes_dir: &std::path::Path,
    _caller: &str,
) -> anyhow::Result<LeadMcpPlan> {
    Ok(LeadMcpPlan::default())
}

/// `thegn mcp proxy [--profile <name>]`. Stub: the profile is ignored and the
/// default hub runs.
pub(crate) fn run_proxy(
    cfg: &thegn_core::config::Config,
    _profile: Option<&str>,
) -> anyhow::Result<()> {
    crate::mcp_proxy::run_shim(cfg)
}

/// `thegn mcp wire --lead [--remove]`. Stub: not available yet.
pub(crate) fn wire_cli(_cfg: &thegn_core::config::Config, _remove: bool) -> anyhow::Result<()> {
    anyhow::bail!("thegn mcp wire --lead is not available yet")
}
