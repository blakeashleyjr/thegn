//! Launching a Lead tab (THE-2171): the sandboxed agent argv. The sandbox
//! fails closed: no bwrap means no Lead, never a host shell.
//!
//! Stub created by the b152 spine (lane 1); filled by b152 lane 8.

/// The launch spec for Lead tab `tab` of `group`. Stub: always refuses.
#[allow(dead_code, reason = "filled by b152 lane 8")]
pub(crate) fn launch_spec(
    _cfg: &thegn_core::config::Config,
    _group: &str,
    _tab: usize,
) -> anyhow::Result<crate::agent::LaunchSpec> {
    anyhow::bail!("the Lead is not available in this build")
}
