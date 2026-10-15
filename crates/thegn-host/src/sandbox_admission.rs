//! Final launch admission, including bare host/remote outcomes after fallback.

use thegn_core::capabilities::IsolationClass;
use thegn_core::config::SandboxConfig;
use thegn_core::sandbox_floor::{FloorDecision, decide, floor_in_force, miss_policy_in_force};

/// Check what the launch will actually enter. Candidate selection can remember
/// rejected weaker runtimes, but that does not prove the eventual host fallback
/// meets the floor (all runtimes may have been absent or failed preflight).
pub(crate) fn admit(
    cfg: &SandboxConfig,
    actual: IsolationClass,
    worktree: &str,
    host_pinned: bool,
    warnings: &mut Vec<String>,
) -> anyhow::Result<()> {
    match decide(
        floor_in_force(cfg, host_pinned),
        miss_policy_in_force(cfg),
        actual,
        actual,
    ) {
        FloorDecision::Ok | FloorDecision::BypassProvider => Ok(()),
        FloorDecision::Degrade(miss) => {
            let message = miss.message();
            thegn_core::msg::warn(&format!("{message} for {worktree}"));
            warnings.push(message);
            Ok(())
        }
        FloorDecision::Fail(miss) => anyhow::bail!("{} for {worktree}", miss.refusal()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::config::{IsolationFloor, OnFloorMiss};

    #[test]
    fn fail_refusal_names_opt_outs() {
        let cfg = SandboxConfig {
            isolation_floor: Some(IsolationFloor::SharedKernel),
            on_floor_miss: OnFloorMiss::Fail,
            ..SandboxConfig::default()
        };
        let mut warnings = Vec::new();
        let error = admit(
            &cfg,
            IsolationClass::HostProcess,
            "wt",
            false,
            &mut warnings,
        )
        .expect_err("host process must miss shared-kernel floor");
        assert!(error.to_string().contains("isolation_floor = \"off\""));
        assert!(error.to_string().contains("enabled = false"));
    }

    #[test]
    fn degrade_keeps_message_without_refusal_opt_outs() {
        let cfg = SandboxConfig {
            isolation_floor: Some(IsolationFloor::SharedKernel),
            on_floor_miss: OnFloorMiss::Degrade,
            ..SandboxConfig::default()
        };
        let mut warnings = Vec::new();
        assert!(
            admit(
                &cfg,
                IsolationClass::HostProcess,
                "wt",
                false,
                &mut warnings
            )
            .is_ok()
        );
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("isolation floor `shared-kernel` not met"));
        assert!(!warnings[0].contains("on_floor_miss = \"degrade\""));
    }

    #[test]
    fn default_floor_refuses_host_unless_pinned() {
        let cfg = SandboxConfig::default();
        let mut warnings = Vec::new();
        assert!(
            admit(
                &cfg,
                IsolationClass::HostProcess,
                "wt",
                false,
                &mut warnings
            )
            .is_err()
        );
        assert!(admit(&cfg, IsolationClass::HostProcess, "wt", true, &mut warnings).is_ok());

        let degrade = SandboxConfig {
            on_floor_miss: OnFloorMiss::Degrade,
            ..cfg
        };
        assert!(
            admit(
                &degrade,
                IsolationClass::HostProcess,
                "wt",
                false,
                &mut warnings
            )
            .is_err()
        );
    }
}
