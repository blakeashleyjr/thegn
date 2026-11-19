//! Where the merge gate's commands run (THE-2200): `[merge_queue]
//! gate_sandbox = "off"` keeps today's host gate; `"sealed"` runs
//! `gate_setup_command`, `gate_command` and `regenerate_command` through the
//! repo's sealed sandbox.
//!
//! Stub: filled by b161 lane 2. Until then `off` resolves to the host and
//! `sealed` refuses (fails closed: never a host gate for a sealed config).
#![allow(dead_code, reason = "filled by b161 lane 2")]

use std::path::Path;

use thegn_core::config::{Config, GateSandbox, MergeQueueConfig};

/// How a gate command is executed.
pub(crate) enum GateExec {
    /// On the host (the `off` default).
    Host,
    /// Inside this sealed sandbox.
    Sealed(Box<thegn_core::sandbox::SandboxSpec>),
}

/// Resolve the gate execution for `repo_root` from the full config. Stub:
/// the host.
pub(crate) fn resolve(_full: &Config, _repo_root: &Path) -> Result<GateExec, String> {
    Ok(GateExec::Host)
}

/// Resolve the gate execution from the repo's effective `[merge_queue]`.
/// Stub: `off` is the host; `sealed` is refused until lane 2 lands.
pub(crate) fn resolve_for_gate(
    _repo_root: &Path,
    mq: &MergeQueueConfig,
) -> Result<GateExec, String> {
    match mq.gate_sandbox {
        GateSandbox::Off => Ok(GateExec::Host),
        GateSandbox::Sealed => Err("the sealed merge gate is not implemented".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_runs_on_the_host_and_sealed_fails_closed() {
        let root = Path::new("/nonexistent");
        let off = MergeQueueConfig::default();
        assert!(matches!(resolve_for_gate(root, &off), Ok(GateExec::Host)));
        let sealed = MergeQueueConfig {
            gate_sandbox: GateSandbox::Sealed,
            ..MergeQueueConfig::default()
        };
        assert!(resolve_for_gate(root, &sealed).is_err());
    }
}
