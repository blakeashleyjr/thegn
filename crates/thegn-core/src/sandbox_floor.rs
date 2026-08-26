//! The **isolation floor** — a demandable minimum on the honest boundary a
//! launch may enter, and its miss policy. Pure comparison logic over the honest
//! [`IsolationClass`](crate::capabilities::IsolationClass): the resolver decides
//! *what* a launch enters, this decides *whether that is allowed to run*.
//!
//! Two doctrines meet here (see the change's Security section):
//! - **Fail-safe** (`degrade`) is right when the user is present and a missed
//!   promise is availability-adjacent — the default, matching every other
//!   interactive degrade (it warns, flags, and proceeds).
//! - **Fail-closed** (`fail`) is right when the security boundary itself is the
//!   promise and nobody is watching — the VPN `on_error = "fail"` precedent. A
//!   fail-closed miss MUST abort before any process spawns on the host.
//!
//! The comparison is over the class of what the launch *actually* enters (after
//! backend-chain selection and any runtime degrade), so a `krun` that fell back
//! to the daemon default compares as `shared-kernel`, and a macOS local OCI
//! container compares as `guest-kernel`. A [`ProviderManaged`] placement is
//! **outside** the order — the user chose to trust the provider — so it bypasses
//! the floor and is reported as `provider-managed`, never counted as a tier.

use crate::capabilities::IsolationClass;
use crate::config::{IsolationFloor, OnFloorMiss};

/// The outcome of comparing a resolved launch's honest class against the floor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FloorDecision {
    /// No floor set, or the launch meets it — proceed unchanged.
    Ok,
    /// A provider placement: the floor is out of scope (trusted, unranked). Not a
    /// pass and not a miss — reported as `provider-managed`.
    BypassProvider,
    /// The floor is missed and policy is `degrade`: launch, but flag it and warn.
    Degrade(FloorMiss),
    /// The floor is missed and policy is `fail`: refuse before any host spawn.
    Fail(FloorMiss),
}

/// The specifics of a floor miss, for the warning / error / notification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FloorMiss {
    /// The demanded floor.
    pub floor: IsolationFloor,
    /// The class the launch actually provides.
    pub actual: IsolationClass,
    /// The best class actually available on this host (the resolver's honest
    /// ceiling), so the remedy can be concrete. Equals `actual` when the caller
    /// has nothing better to offer.
    pub best_available: IsolationClass,
}

impl FloorMiss {
    /// A one-line, actionable message: the floor, what was actually available,
    /// and how to satisfy it.
    pub fn message(&self) -> String {
        format!(
            "isolation floor `{}` not met — this launch is `{}` (best available here: `{}`); {}",
            self.floor.as_str(),
            self.actual.as_str(),
            self.best_available.as_str(),
            self.floor.remedy(),
        )
    }
}

/// Whether `actual` satisfies `floor`. A [`ProviderManaged`] class is unranked
/// and never "satisfies" a floor (it bypasses the check entirely — callers use
/// [`decide`], which distinguishes bypass from a genuine pass/miss). An unset
/// floor is met by everything.
pub fn class_meets(actual: IsolationClass, floor: IsolationFloor) -> bool {
    let Some(required) = floor.required_rank() else {
        return true; // Off: no floor.
    };
    match actual.rank() {
        Some(have) => have >= required,
        None => false, // ProviderManaged — unranked; see `decide` for the bypass.
    }
}

/// The floor decision for a resolved launch. `best_available` is the strongest
/// honest class the host could offer (for the remedy); pass `actual` when there
/// is nothing better to name.
pub fn decide(
    floor: IsolationFloor,
    on_miss: OnFloorMiss,
    actual: IsolationClass,
    best_available: IsolationClass,
) -> FloorDecision {
    // No floor: today's behavior, unchanged.
    if floor.required_rank().is_none() {
        return FloorDecision::Ok;
    }
    // A provider placement is outside the order — trusted, not ranked. The floor
    // is out of scope; we neither pass nor fail it.
    if actual == IsolationClass::ProviderManaged {
        return FloorDecision::BypassProvider;
    }
    if class_meets(actual, floor) {
        return FloorDecision::Ok;
    }
    let miss = FloorMiss {
        floor,
        actual,
        best_available,
    };
    match on_miss {
        OnFloorMiss::Degrade => FloorDecision::Degrade(miss),
        OnFloorMiss::Fail => FloorDecision::Fail(miss),
    }
}

/// How an opt-in agent/queue task's floor decision maps onto queue-entry state.
/// The load-bearing rule (the merge-guard doctrine): a fail-closed floor miss or
/// a sandbox setup failure is an **infrastructure** failure — the entry is
/// held/retried — and is never recorded as a failure of the branch or the agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentGate {
    /// Run the task under the resolved sandbox (or host, if `sandbox` is off).
    Run,
    /// Run, but the floor was missed under `degrade` — carry the warning.
    RunDegraded(String),
    /// Do not run: an infrastructure failure (fail-closed miss, or the sandbox
    /// could not be set up). Hold the queue entry; NEVER blame the branch.
    InfraHold(String),
}

/// Gate an opt-in agent/queue task. `resolved` is the honest class of the
/// sandbox the task would enter, or `None` when the sandbox could not be
/// resolved / set up at all (a broken boundary). With the sandbox opt-in off the
/// task runs host-side as before (the default posture is unchanged).
pub fn agent_task_gate(
    sandbox_on: bool,
    floor: IsolationFloor,
    on_miss: OnFloorMiss,
    resolved: Option<IsolationClass>,
    best_available: IsolationClass,
) -> AgentGate {
    if !sandbox_on {
        return AgentGate::Run; // host + slice, the unchanged default.
    }
    let Some(actual) = resolved else {
        // The sandbox itself could not be established. Under a demanded floor
        // that is a broken boundary → infra hold; with no floor, it degrades to
        // the host like any other launch (fail-safe).
        return if floor.required_rank().is_some() {
            match on_miss {
                OnFloorMiss::Fail => AgentGate::InfraHold(
                    "sandbox could not be established for the queue task (infrastructure failure); \
                     the branch is not at fault"
                        .to_string(),
                ),
                OnFloorMiss::Degrade => AgentGate::RunDegraded(
                    "sandbox could not be established; running the queue task on the host".to_string(),
                ),
            }
        } else {
            AgentGate::Run
        };
    };
    match decide(floor, on_miss, actual, best_available) {
        FloorDecision::Ok | FloorDecision::BypassProvider => AgentGate::Run,
        FloorDecision::Degrade(m) => AgentGate::RunDegraded(m.message()),
        FloorDecision::Fail(m) => AgentGate::InfraHold(format!(
            "{} — held as an infrastructure failure; the branch is not marked failed",
            m.message()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_floor_is_always_met() {
        for actual in [
            IsolationClass::HostProcess,
            IsolationClass::SharedKernel,
            IsolationClass::ProviderManaged,
        ] {
            assert!(class_meets(actual, IsolationFloor::Off));
            assert_eq!(
                decide(
                    IsolationFloor::Off,
                    OnFloorMiss::Fail,
                    actual,
                    IsolationClass::HostProcess
                ),
                FloorDecision::Ok
            );
        }
    }

    #[test]
    fn floor_met_by_a_stronger_runtime() {
        // guest-kernel floor, launch runs an OCI backend under krun → guest-kernel.
        assert_eq!(
            decide(
                IsolationFloor::GuestKernel,
                OnFloorMiss::Fail,
                IsolationClass::GuestKernel,
                IsolationClass::GuestKernel,
            ),
            FloorDecision::Ok
        );
        // userspace-kernel floor met by runsc.
        assert!(class_meets(
            IsolationClass::UserspaceKernel,
            IsolationFloor::UserspaceKernel
        ));
        // …and a stronger class over-satisfies a weaker floor.
        assert!(class_meets(
            IsolationClass::GuestKernel,
            IsolationFloor::SharedKernel
        ));
    }

    #[test]
    fn degraded_runtime_is_compared_as_what_it_became() {
        // Asked for userspace-kernel (runsc), but runsc was absent so the launch
        // is really shared-kernel. Missed, and `degrade` warns + proceeds.
        let d = decide(
            IsolationFloor::UserspaceKernel,
            OnFloorMiss::Degrade,
            IsolationClass::SharedKernel,
            IsolationClass::SharedKernel,
        );
        match d {
            FloorDecision::Degrade(m) => {
                assert_eq!(m.floor, IsolationFloor::UserspaceKernel);
                assert_eq!(m.actual, IsolationClass::SharedKernel);
                assert!(m.message().contains("userspace-kernel"));
                assert!(m.message().contains("shared-kernel"));
            }
            other => panic!("expected degrade, got {other:?}"),
        }
    }

    #[test]
    fn fail_closed_refuses() {
        let d = decide(
            IsolationFloor::GuestKernel,
            OnFloorMiss::Fail,
            IsolationClass::SharedKernel,
            IsolationClass::SharedKernel,
        );
        match d {
            FloorDecision::Fail(m) => {
                assert!(m.message().contains("guest-kernel"));
                // The remedy must name a way to satisfy the floor.
                assert!(!m.floor.remedy().is_empty());
            }
            other => panic!("expected fail, got {other:?}"),
        }
    }

    #[test]
    fn provider_placement_bypasses_the_floor() {
        // A managed provider is trusted, not ranked: neither counted as satisfying
        // nor as missing — reported as provider-managed, out of scope.
        for on_miss in [OnFloorMiss::Degrade, OnFloorMiss::Fail] {
            assert_eq!(
                decide(
                    IsolationFloor::GuestKernel,
                    on_miss,
                    IsolationClass::ProviderManaged,
                    IsolationClass::ProviderManaged,
                ),
                FloorDecision::BypassProvider
            );
        }
    }

    #[test]
    fn windows_jobobject_satisfies_no_floor_at_shared_kernel_or_above() {
        // A native Windows Job Object is host-process class; a container floor is
        // therefore a miss (governed by on_floor_miss).
        assert!(!class_meets(
            IsolationClass::HostProcess,
            IsolationFloor::SharedKernel
        ));
        assert!(matches!(
            decide(
                IsolationFloor::SharedKernel,
                OnFloorMiss::Fail,
                IsolationClass::HostProcess,
                IsolationClass::HostProcess,
            ),
            FloorDecision::Fail(_)
        ));
        assert!(matches!(
            decide(
                IsolationFloor::SharedKernel,
                OnFloorMiss::Degrade,
                IsolationClass::HostProcess,
                IsolationClass::HostProcess,
            ),
            FloorDecision::Degrade(_)
        ));
    }

    #[test]
    fn agent_gate_off_runs_on_the_host() {
        assert_eq!(
            agent_task_gate(
                false,
                IsolationFloor::GuestKernel,
                OnFloorMiss::Fail,
                None,
                IsolationClass::HostProcess
            ),
            AgentGate::Run
        );
    }

    #[test]
    fn agent_gate_holds_on_fail_closed_miss_and_never_blames_the_branch() {
        // A fail-closed floor the host cannot meet → the entry is held as an
        // infrastructure failure, not a branch/agent failure.
        let g = agent_task_gate(
            true,
            IsolationFloor::GuestKernel,
            OnFloorMiss::Fail,
            Some(IsolationClass::SharedKernel),
            IsolationClass::SharedKernel,
        );
        match g {
            AgentGate::InfraHold(reason) => {
                assert!(reason.contains("guest-kernel"));
                assert!(reason.to_lowercase().contains("infrastructure"));
                assert!(reason.contains("branch is not"));
            }
            other => panic!("expected infra hold, got {other:?}"),
        }
    }

    #[test]
    fn agent_gate_unresolvable_sandbox_is_infra_under_a_fail_floor() {
        // The boundary could not be established at all. With a demanded fail-closed
        // floor that is an infra hold; with degrade it runs on the host, warned;
        // with no floor it just runs.
        assert!(matches!(
            agent_task_gate(
                true,
                IsolationFloor::SharedKernel,
                OnFloorMiss::Fail,
                None,
                IsolationClass::HostProcess
            ),
            AgentGate::InfraHold(_)
        ));
        assert!(matches!(
            agent_task_gate(
                true,
                IsolationFloor::SharedKernel,
                OnFloorMiss::Degrade,
                None,
                IsolationClass::HostProcess
            ),
            AgentGate::RunDegraded(_)
        ));
        assert_eq!(
            agent_task_gate(
                true,
                IsolationFloor::Off,
                OnFloorMiss::Fail,
                None,
                IsolationClass::HostProcess
            ),
            AgentGate::Run
        );
    }

    #[test]
    fn agent_gate_runs_when_floor_is_met() {
        assert_eq!(
            agent_task_gate(
                true,
                IsolationFloor::SharedKernel,
                OnFloorMiss::Fail,
                Some(IsolationClass::SharedKernel),
                IsolationClass::SharedKernel,
            ),
            AgentGate::Run
        );
    }
}
