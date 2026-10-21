//! Final launch admission, including bare host/remote outcomes after fallback.

use thegn_core::capabilities::IsolationClass;
use thegn_core::config::{IsolationFloor, SandboxConfig};
use thegn_core::sandbox_floor::{
    FloorDecision, FloorEnforcement, FloorMiss, decide, floor_enforcement, floor_in_force,
    miss_policy_in_force,
};

/// Only this module mints an admission. Holding one proves the launch passed
/// the isolation-floor admission (THE-794 slice c).
#[derive(Debug, Clone)]
pub(crate) struct Admission {
    class: IsolationClass,
    enforcement: FloorEnforcement,
}

impl Admission {
    pub(crate) fn class(&self) -> IsolationClass {
        self.class
    }

    pub(crate) fn enforcement(&self) -> FloorEnforcement {
        self.enforcement
    }

    #[cfg(test)]
    pub(crate) fn for_test(class: IsolationClass) -> Self {
        Self {
            class,
            enforcement: FloorEnforcement::NoFloor,
        }
    }
}

pub(crate) enum CandidateAdmission {
    Admitted(Admission),
    Miss(FloorMiss),
}

/// Check what the launch will actually enter. Candidate selection can remember
/// rejected weaker runtimes, but that does not prove the eventual host fallback
/// meets the floor (all runtimes may have been absent or failed preflight).
pub(crate) fn admit(
    cfg: &SandboxConfig,
    actual: IsolationClass,
    worktree: &str,
    host_pinned: bool,
    warnings: &mut Vec<String>,
) -> anyhow::Result<Admission> {
    let floor = floor_in_force(cfg, host_pinned);
    match admission_for_decision(
        decide(floor, miss_policy_in_force(cfg), actual, actual),
        floor,
        actual,
        worktree,
        warnings,
    ) {
        Ok(admission) => Ok(admission),
        Err(miss) => anyhow::bail!("{} for {worktree}", miss.refusal()),
    }
}

/// Admit a candidate while leaving the resolver in control of explicit-pick
/// refusal versus trying the next candidate.
pub(crate) fn admit_candidate(
    cfg: &SandboxConfig,
    floor: IsolationFloor,
    actual: IsolationClass,
    worktree: &str,
    warnings: &mut Vec<String>,
) -> CandidateAdmission {
    match admission_for_decision(
        decide(floor, miss_policy_in_force(cfg), actual, actual),
        floor,
        actual,
        worktree,
        warnings,
    ) {
        Ok(admission) => CandidateAdmission::Admitted(admission),
        Err(miss) => CandidateAdmission::Miss(miss),
    }
}

fn admission_for_decision(
    decision: FloorDecision,
    floor: IsolationFloor,
    actual: IsolationClass,
    worktree: &str,
    warnings: &mut Vec<String>,
) -> Result<Admission, FloorMiss> {
    let enforcement = floor_enforcement(floor, &decision);
    match decision {
        FloorDecision::Ok => Ok(Admission {
            class: actual,
            enforcement,
        }),
        FloorDecision::BypassProvider => Ok(Admission {
            class: actual,
            enforcement,
        }),
        FloorDecision::Degrade(miss) => {
            let message = miss.message();
            thegn_core::msg::warn(&format!("{message} for {worktree}"));
            warnings.push(message);
            Ok(Admission {
                class: actual,
                enforcement,
            })
        }
        FloorDecision::Fail(miss) => Err(miss),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::config::OnFloorMiss;

    fn cfg(floor: IsolationFloor, on_floor_miss: OnFloorMiss) -> SandboxConfig {
        SandboxConfig {
            isolation_floor: Some(floor),
            on_floor_miss,
            ..SandboxConfig::default()
        }
    }

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
        assert!(error.to_string().contains("backend = \"none\""));
        assert!(error.to_string().contains("run on host"));
        assert!(error.to_string().contains("on_floor_miss = \"degrade\""));
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

    #[test]
    fn admission_records_class_and_enforcement() {
        let mut warnings = Vec::new();
        let no_floor = admit(
            &cfg(IsolationFloor::Off, OnFloorMiss::Fail),
            IsolationClass::HostProcess,
            "wt",
            false,
            &mut warnings,
        )
        .unwrap();
        assert_eq!(no_floor.class(), IsolationClass::HostProcess);
        assert_eq!(no_floor.enforcement(), FloorEnforcement::NoFloor);

        let met = admit(
            &cfg(IsolationFloor::SharedKernel, OnFloorMiss::Fail),
            IsolationClass::SharedKernel,
            "wt",
            false,
            &mut warnings,
        )
        .unwrap();
        assert_eq!(met.class(), IsolationClass::SharedKernel);
        assert_eq!(met.enforcement(), FloorEnforcement::Met);

        let provider = admit(
            &cfg(IsolationFloor::SharedKernel, OnFloorMiss::Fail),
            IsolationClass::ProviderManaged,
            "wt",
            false,
            &mut warnings,
        )
        .unwrap();
        assert_eq!(provider.class(), IsolationClass::ProviderManaged);
        assert_eq!(provider.enforcement(), FloorEnforcement::ProviderUnverified);

        let degrade = admit(
            &cfg(IsolationFloor::SharedKernel, OnFloorMiss::Degrade),
            IsolationClass::HostProcess,
            "wt",
            false,
            &mut warnings,
        )
        .unwrap();
        assert_eq!(degrade.class(), IsolationClass::HostProcess);
        assert_eq!(degrade.enforcement(), FloorEnforcement::MissedDegrade);
        assert_eq!(warnings.len(), 1);
    }

    #[test]
    fn candidate_fail_floor_is_a_miss_without_warning() {
        let mut warnings = Vec::new();
        let result = admit_candidate(
            &cfg(IsolationFloor::SharedKernel, OnFloorMiss::Fail),
            IsolationFloor::SharedKernel,
            IsolationClass::HostProcess,
            "wt",
            &mut warnings,
        );
        assert!(
            matches!(result, CandidateAdmission::Miss(miss) if miss.refusal().contains("isolation_floor = \"off\""))
        );
        assert!(warnings.is_empty());
    }

    #[test]
    fn candidate_degrade_warns_and_admits() {
        let mut warnings = Vec::new();
        let result = admit_candidate(
            &cfg(IsolationFloor::SharedKernel, OnFloorMiss::Degrade),
            IsolationFloor::SharedKernel,
            IsolationClass::HostProcess,
            "wt",
            &mut warnings,
        );
        assert!(
            matches!(result, CandidateAdmission::Admitted(admission) if admission.enforcement() == FloorEnforcement::MissedDegrade)
        );
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("isolation floor `shared-kernel` not met"));
    }

    #[test]
    fn candidate_off_floor_admits_silently() {
        let mut warnings = Vec::new();
        let result = admit_candidate(
            &cfg(IsolationFloor::Off, OnFloorMiss::Fail),
            IsolationFloor::Off,
            IsolationClass::HostProcess,
            "wt",
            &mut warnings,
        );
        assert!(
            matches!(result, CandidateAdmission::Admitted(admission) if admission.enforcement() == FloorEnforcement::NoFloor)
        );
        assert!(warnings.is_empty());
    }
}
