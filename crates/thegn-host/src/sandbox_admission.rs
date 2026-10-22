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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RouteStatus {
    Handle,
    AdmittedNoHandle,
    Partial,
    OwnGate,
    HostCapped,
    HostTrust,
}

impl RouteStatus {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Handle => "handle",
            Self::AdmittedNoHandle => "admitted-no-handle",
            Self::Partial => "partial",
            Self::OwnGate => "own-gate",
            Self::HostCapped => "host-capped",
            Self::HostTrust => "host-trust",
        }
    }
}

pub(crate) struct LaunchRoute {
    pub(crate) id: &'static str,
    pub(crate) route: &'static str,
    pub(crate) status: RouteStatus,
    pub(crate) path: &'static str,
    #[cfg_attr(not(test), allow(dead_code, reason = "read by the route anchor tests"))]
    anchor: Option<(&'static str, &'static str)>,
}

pub(crate) const LAUNCH_ROUTES: &[LaunchRoute] = &[
    LaunchRoute {
        id: "worktree-panes",
        route: "Worktree shells, configured agents/tools, daemon opens",
        status: RouteStatus::Handle,
        path: "agent::prepare_sandbox_env returns a SandboxOutcome that must carry the admission",
        anchor: Some((
            "agent.rs",
            "pub(crate) admission: crate::sandbox_admission::Admission,",
        )),
    },
    LaunchRoute {
        id: "terminal-tabs",
        route: "Standalone terminal tabs",
        status: RouteStatus::AdmittedNoHandle,
        path: "panes::terminal_launch_spec admits wrapped, degraded and fallback shells; the handle is not carried",
        anchor: Some((
            "panes.rs",
            "crate::sandbox_admission::admit(sb, actual, \"terminal\", false, warnings)",
        )),
    },
    LaunchRoute {
        id: "queue-agents",
        route: "Queue/CI/review/merge agents",
        status: RouteStatus::OwnGate,
        path: "agent_run::agent_floor_gate (sandbox_floor::queue_agent_gate), outside the handle",
        anchor: Some((
            "agent_run.rs",
            "queue_agent_gate(&effective, floor, on_miss, resolved, best)",
        )),
    },
    LaunchRoute {
        id: "autopilot",
        route: "Autopilot",
        status: RouteStatus::OwnGate,
        path: "autopilot_driver::sealed_autopilot_sandbox forces a sealed, credential-free spec, outside the handle",
        anchor: Some(("autopilot_driver.rs", "fn sealed_autopilot_sandbox(")),
    },
    LaunchRoute {
        id: "tests-gates",
        route: "Tests/discovery/gates/regeneration",
        status: RouteStatus::HostCapped,
        path: "host commands under the shared resource ceiling (integrate_gate)",
        anchor: Some((
            "integrate_gate.rs",
            "thegn_core::sandbox_cpucap::wrap_background_argv(",
        )),
    },
    LaunchRoute {
        id: "host-hooks",
        route: "Host lifecycle hooks/devcontainer initialize",
        status: RouteStatus::HostTrust,
        path: "user-approved host hooks (hook_run), resource-capped",
        anchor: Some((
            "hook_run.rs",
            "thegn_core::sandbox_cpucap::wrap_background_argv(vec![",
        )),
    },
    LaunchRoute {
        id: "devcontainer",
        route: "Container devcontainer lifecycle",
        status: RouteStatus::Handle,
        path: "admitted before devcontainer bring-up in agent::prepare_sandbox_env",
        anchor: Some(("agent.rs", "Admit before bring-up so a")),
    },
    LaunchRoute {
        id: "provider-native",
        route: "Provider-native",
        status: RouteStatus::Partial,
        path: "pane opens carry the handle (SandboxOutcome); provider exec and transport calls do not",
        anchor: None,
    },
    LaunchRoute {
        id: "host-git",
        route: "Host Git reads/mutations",
        status: RouteStatus::HostTrust,
        path: "local git helpers on the host",
        anchor: None,
    },
    LaunchRoute {
        id: "language-services",
        route: "Language services",
        status: RouteStatus::HostCapped,
        path: "host language servers under the resource ceiling (lsp)",
        anchor: Some((
            "lsp.rs",
            "let argv = thegn_core::sandbox_cpucap::wrap_background_argv(argv);",
        )),
    },
    LaunchRoute {
        id: "host-extensions",
        route: "MCP/plugins/editors/viewer or arbitrary-command panes",
        status: RouteStatus::HostTrust,
        path: "user-configured host extensions (mcp_proxy upstream, plugins, editors)",
        anchor: Some((
            "mcp_proxy/upstream.rs",
            "let wrapped = thegn_core::sandbox_cpucap::wrap_background_argv(argv.to_vec());",
        )),
    },
];

pub(crate) fn route_lines() -> Vec<String> {
    let mut lines = Vec::with_capacity(LAUNCH_ROUTES.len() + 1);
    lines.push(
        "  admission     execution routes (THE-794): handle = carries the sandbox admission handle"
            .to_string(),
    );
    lines.extend(LAUNCH_ROUTES.iter().map(|route| {
        format!(
            "  admission     {}: {} - {}",
            route.id,
            route.status.as_str(),
            route.path
        )
    }));
    lines
}

pub(crate) fn routes_json() -> serde_json::Value {
    serde_json::Value::Array(
        LAUNCH_ROUTES
            .iter()
            .map(|route| {
                serde_json::json!({
                    "id": route.id,
                    "route": route.route,
                    "status": route.status.as_str(),
                    "path": route.path,
                })
            })
            .collect(),
    )
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

    #[test]
    fn launch_route_table_covers_the_audit_rows() {
        const AUDIT_ROWS: [&str; 11] = [
            "Worktree shells, configured agents/tools, daemon opens",
            "Standalone terminal tabs",
            "Queue/CI/review/merge agents",
            "Autopilot",
            "Tests/discovery/gates/regeneration",
            "Host lifecycle hooks/devcontainer initialize",
            "Container devcontainer lifecycle",
            "Provider-native",
            "Host Git reads/mutations",
            "Language services",
            "MCP/plugins/editors/viewer or arbitrary-command panes",
        ];
        assert_eq!(LAUNCH_ROUTES.len(), 11);
        let mut ids = std::collections::HashSet::new();
        for route in LAUNCH_ROUTES {
            assert!(ids.insert(route.id), "duplicate route id {}", route.id);
        }
        assert_eq!(
            LAUNCH_ROUTES
                .iter()
                .map(|route| route.route)
                .collect::<Vec<_>>(),
            AUDIT_ROWS
        );
    }

    fn source(file: &str) -> &'static str {
        match file {
            "agent.rs" => include_str!("agent.rs"),
            "panes.rs" => include_str!("panes.rs"),
            "agent_run.rs" => include_str!("agent_run.rs"),
            "autopilot_driver.rs" => include_str!("autopilot_driver.rs"),
            "integrate_gate.rs" => include_str!("integrate_gate.rs"),
            "hook_run.rs" => include_str!("hook_run.rs"),
            "lsp.rs" => include_str!("lsp.rs"),
            "mcp_proxy/upstream.rs" => include_str!("mcp_proxy/upstream.rs"),
            unknown => panic!("unknown route anchor file {unknown}"),
        }
    }

    #[test]
    fn launch_route_anchors_are_present() {
        for route in LAUNCH_ROUTES {
            if let Some((file, text)) = route.anchor {
                assert!(
                    source(file).contains(text),
                    "route {} anchor disappeared from {file}: {text}",
                    route.id
                );
            }
        }
    }

    #[test]
    fn launch_route_non_handle_files_do_not_admit() {
        for route in LAUNCH_ROUTES {
            if let Some((file, _)) = route.anchor {
                match route.status {
                    RouteStatus::OwnGate | RouteStatus::HostCapped | RouteStatus::HostTrust => {
                        assert!(
                            !source(file).contains("sandbox_admission::"),
                            "route {} now calls sandbox_admission: update LAUNCH_ROUTES",
                            route.id
                        );
                    }
                    RouteStatus::Handle | RouteStatus::AdmittedNoHandle => assert!(
                        source(file).contains("sandbox_admission::"),
                        "route {} should call sandbox_admission",
                        route.id
                    ),
                    RouteStatus::Partial => {}
                }
            }
        }
    }

    #[test]
    fn launch_route_lines_name_every_route() {
        let lines = route_lines();
        assert_eq!(lines.len(), 12);
        for route in LAUNCH_ROUTES {
            let line = lines
                .iter()
                .find(|line| line.contains(route.id))
                .unwrap_or_else(|| panic!("missing route line for {}", route.id));
            assert!(line.contains(route.status.as_str()), "{line}");
        }
    }
}
