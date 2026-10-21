//! The **isolation floor** — a demandable minimum on the honest boundary a
//! launch may enter, and its miss policy. Pure comparison logic over the honest
//! [`IsolationClass`]: the resolver decides
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
//! container compares as `guest-kernel`. A [`IsolationClass::ProviderManaged`]
//! placement is
//! **outside** the order — the user chose to trust the provider — so it bypasses
//! the floor and is reported as `provider-managed`, never counted as a tier.

use crate::capabilities::IsolationClass;
use crate::config::{FileAccess, IsolationFloor, OnFloorMiss, SandboxBackend, SandboxConfig};
use crate::sandbox::{Backend, BackendFamily};

/// Default minimum for enabled sandbox launches that have not opted into a
/// deliberate host execution choice.
pub const DEFAULT_ISOLATION_FLOOR: IsolationFloor = IsolationFloor::SharedKernel;

/// Resolve the floor that governs this launch. An explicit configured floor
/// always wins, including over deliberate host choices.
pub fn floor_in_force(sb: &SandboxConfig, host_pinned: bool) -> IsolationFloor {
    if let Some(floor) = sb.isolation_floor {
        return floor;
    }
    if !sb.enabled || sb.backend == SandboxBackend::None || host_pinned {
        IsolationFloor::Off
    } else {
        DEFAULT_ISOLATION_FLOOR
    }
}

/// Default-floor misses always refuse; only an explicitly configured floor
/// may opt into its configured miss policy.
pub fn miss_policy_in_force(sb: &SandboxConfig) -> OnFloorMiss {
    if sb.isolation_floor.is_some() {
        sb.on_floor_miss
    } else {
        OnFloorMiss::Fail
    }
}

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

    /// An actionable refusal that names the supported floor opt-outs.
    /// `message()` remains the concise warning used by degrade paths.
    pub fn refusal(&self) -> String {
        format!(
            "{}; to launch on the host set [sandbox] isolation_floor = \"off\"; with no explicit floor, enabled = false, backend = \"none\", or \"run on host\" also opts out; for an explicit floor, on_floor_miss = \"degrade\" permits a warned launch",
            self.message()
        )
    }
}

/// Whether the backend enforces the requested filesystem scope. Host-root
/// execution and Windows process scoping do not hide narrower file scopes.
pub fn file_access_enforced(backend: Backend, file_access: FileAccess) -> bool {
    if matches!(file_access, FileAccess::All | FileAccess::Host) {
        return true;
    }
    !matches!(
        backend.profile().family,
        BackendFamily::Systemd
            | BackendFamily::Host
            | BackendFamily::WinJobObject
            | BackendFamily::WinAppContainer
    )
}

/// What the enabled launch actually says about its containment boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Containment {
    Disabled,
    Uncontained,
    Contained(IsolationClass),
    ProviderManaged,
}

impl Containment {
    /// Stable machine-readable spelling, including the honest class when
    /// contained.
    pub fn as_str(self) -> String {
        match self {
            Self::Disabled => "disabled".to_string(),
            Self::Uncontained => "not-contained".to_string(),
            Self::Contained(class) => format!("contained:{}", class.as_str()),
            Self::ProviderManaged => "provider-managed".to_string(),
        }
    }
}

/// Summarize whether the selected class is a genuine boundary.
pub fn containment_summary(enabled: bool, actual: IsolationClass) -> Containment {
    if !enabled {
        return Containment::Disabled;
    }
    match actual {
        IsolationClass::HostProcess => Containment::Uncontained,
        IsolationClass::ProviderManaged => Containment::ProviderManaged,
        class => Containment::Contained(class),
    }
}

/// Whether `actual` satisfies `floor`. A [`IsolationClass::ProviderManaged`]
/// class is unranked
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

/// The `$HOME` confidentiality gate (THE-215). A sealed profile promises the host
/// `$HOME` is not readable; a launch whose resolved spec would expose it — a
/// backend with no filesystem boundary (host process, Windows), `file_access =
/// "all"`/`"host"`, or a `[sandbox] mounts` entry covering `$HOME` — must be
/// **refused**, never run with a quietly weaker `sealed`. Returns the miss
/// message, or `None` when the spec is not sealed or hides `$HOME`.
pub fn home_gate(spec: &crate::sandbox::SandboxSpec) -> Option<String> {
    use crate::sandbox::BackendFamily;
    use crate::sandbox_mounts::{HomeView, home_view};
    let home = spec.seal_home.as_deref()?;
    // An unset/empty/relative/unresolvable `$HOME` cannot be hidden or checked:
    // refuse rather than silently disable sealing.
    let home_path = std::path::Path::new(home);
    if home.is_empty() || !home_path.is_absolute() || home_path.canonicalize().is_err() {
        return Some(
            "sealed profile requires a resolvable absolute $HOME to hide, but $HOME is \
             unset, empty, relative or does not exist"
                .to_string(),
        );
    }
    // systemd takes mounts as `BindPaths=host:dest` property strings with no
    // quoting: a path containing whitespace or `:` would be parsed as extra
    // fields (or a different destination). Refuse the sealed launch instead.
    if spec.backend.profile().family == BackendFamily::Systemd {
        let bad = crate::sandbox_mounts::mount_path_unsafe;
        if let Some(m) = spec
            .mounts
            .iter()
            .find(|m| home_path_under(&m.dest, home_path) && (bad(&m.host) || bad(&m.dest)))
        {
            return Some(format!(
                "sealed systemd launch refused: mount `{}` contains whitespace, a quote, a \
                 backslash or ':' which systemd BindPaths cannot express safely",
                m.dest
            ));
        }
    }
    // A mount whose host path is (or contains) a protected path — the worktree
    // being `$HOME` itself, a `~` mount — would expose the secrets inside it.
    if let Some(m) = spec
        .mounts
        .iter()
        .find(|m| crate::sandbox_mounts::sealed_mount_covers_deny(m, home_path))
    {
        return Some(format!(
            "sealed profile refused: mount `{}` contains protected $HOME paths (credentials, \
             keys, thegn state); mount something narrower or use a non-sealed profile",
            m.host
        ));
    }
    let view = home_view(spec, home_path);
    (view != HomeView::Hidden).then(|| {
        format!(
            "sealed profile requires a hidden $HOME, but the `{}` launch would leave it {} — \
             use a bwrap/systemd/OCI backend, `file_access = \"worktree\"`, and no `[sandbox] \
             mounts` entry covering $HOME (see `thegn doctor`)",
            spec.backend.label(),
            view.as_str(),
        )
    })
}

fn home_path_under(dest: &str, home: &std::path::Path) -> bool {
    std::path::Path::new(dest).starts_with(home)
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

/// Gate an opted-in queue agent against both its queue floor and the `[sandbox]`
/// floor in force. The stricter decision wins (ties keep the queue decision), so
/// queue policy can raise the boundary but cannot weaken the sandbox default.
/// Disabling the sandbox, selecting backend `none`, or explicitly setting the
/// sandbox floor to `off` opts out of the inherited floor.
pub fn queue_agent_gate(
    sb: &SandboxConfig,
    floor: IsolationFloor,
    on_miss: OnFloorMiss,
    resolved: Option<IsolationClass>,
    best_available: IsolationClass,
) -> AgentGate {
    let queue_gate = agent_task_gate(true, floor, on_miss, resolved, best_available);
    let sandbox_gate = agent_task_gate(
        true,
        floor_in_force(sb, false),
        miss_policy_in_force(sb),
        resolved,
        best_available,
    );
    let severity = |gate: &AgentGate| match gate {
        AgentGate::Run => 0,
        AgentGate::RunDegraded(_) => 1,
        AgentGate::InfraHold(_) => 2,
    };
    if severity(&sandbox_gate) > severity(&queue_gate) {
        match sandbox_gate {
            AgentGate::InfraHold(reason) => AgentGate::InfraHold(format!(
                "{reason}; inherited from the [sandbox] isolation floor in force — set \
                 [sandbox] isolation_floor = \"off\" to run queue agents uncontained"
            )),
            other => other,
        }
    } else {
        queue_gate
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_access_enforced_table_covers_each_backend_and_scope() {
        let scopes = [
            FileAccess::Worktree,
            FileAccess::WorktreePlusCaches,
            FileAccess::Custom,
            FileAccess::All,
            FileAccess::Host,
            FileAccess::None,
        ];
        for backend in Backend::ALL {
            for scope in scopes {
                let expected = match scope {
                    FileAccess::All | FileAccess::Host => true,
                    FileAccess::Worktree
                    | FileAccess::WorktreePlusCaches
                    | FileAccess::Custom
                    | FileAccess::None => !matches!(
                        backend,
                        Backend::Systemd
                            | Backend::None
                            | Backend::WinJobObject
                            | Backend::WinAppContainer
                    ),
                };
                assert_eq!(
                    file_access_enforced(backend, scope),
                    expected,
                    "{backend:?} / {scope:?}"
                );
            }
        }
        assert!(!file_access_enforced(
            Backend::Systemd,
            FileAccess::Worktree
        ));
        assert!(file_access_enforced(Backend::Bwrap, FileAccess::Worktree));
        assert!(!file_access_enforced(Backend::None, FileAccess::Worktree));
        assert!(file_access_enforced(Backend::Podman, FileAccess::None));
        assert!(!file_access_enforced(
            Backend::WinJobObject,
            FileAccess::Worktree
        ));
    }

    #[test]
    fn containment_summary_cases() {
        assert_eq!(
            containment_summary(false, IsolationClass::SharedKernel),
            Containment::Disabled
        );
        assert_eq!(Containment::Disabled.as_str(), "disabled");
        assert_eq!(
            containment_summary(true, IsolationClass::HostProcess),
            Containment::Uncontained
        );
        assert_eq!(Containment::Uncontained.as_str(), "not-contained");
        assert_eq!(
            containment_summary(true, IsolationClass::SharedKernel),
            Containment::Contained(IsolationClass::SharedKernel)
        );
        assert_eq!(
            containment_summary(true, IsolationClass::ProviderManaged),
            Containment::ProviderManaged
        );
        assert_eq!(Containment::ProviderManaged.as_str(), "provider-managed");
        assert_eq!(
            Containment::Contained(IsolationClass::GuestKernel).as_str(),
            "contained:guest-kernel"
        );
    }

    #[test]
    fn refusal_names_opt_outs_and_explicit_floor_degrade_and_preserves_message() {
        let FloorDecision::Fail(miss) = decide(
            IsolationFloor::SharedKernel,
            OnFloorMiss::Fail,
            IsolationClass::HostProcess,
            IsolationClass::HostProcess,
        ) else {
            panic!("expected floor miss");
        };
        let refusal = miss.refusal();
        let message = miss.message();
        assert!(refusal.contains(message.as_str()));
        assert!(refusal.contains("isolation_floor = \"off\""));
        assert!(refusal.contains("enabled = false"));
        assert!(refusal.contains("backend = \"none\""));
        assert!(refusal.contains("run on host"));
        assert!(refusal.contains("on_floor_miss = \"degrade\""));
    }

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

    #[test]
    fn default_floor_applies_only_to_enabled_non_host_launches() {
        use crate::config::{SandboxBackend, SandboxConfig};

        let base = SandboxConfig::default();
        assert_eq!(floor_in_force(&base, false), IsolationFloor::SharedKernel);
        assert_eq!(miss_policy_in_force(&base), OnFloorMiss::Fail);
        let default_with_degrade = SandboxConfig {
            on_floor_miss: OnFloorMiss::Degrade,
            ..base.clone()
        };
        assert_eq!(
            miss_policy_in_force(&default_with_degrade),
            OnFloorMiss::Fail
        );

        let disabled = SandboxConfig {
            enabled: false,
            ..base.clone()
        };
        assert_eq!(floor_in_force(&disabled, false), IsolationFloor::Off);

        let no_backend = SandboxConfig {
            backend: SandboxBackend::None,
            ..base.clone()
        };
        assert_eq!(floor_in_force(&no_backend, false), IsolationFloor::Off);
        assert_eq!(floor_in_force(&base, true), IsolationFloor::Off);

        let explicit = SandboxConfig {
            enabled: false,
            isolation_floor: Some(IsolationFloor::GuestKernel),
            on_floor_miss: OnFloorMiss::Degrade,
            ..base.clone()
        };
        assert_eq!(floor_in_force(&explicit, true), IsolationFloor::GuestKernel);
        assert_eq!(miss_policy_in_force(&explicit), OnFloorMiss::Degrade);
        let explicit_off = SandboxConfig {
            isolation_floor: Some(IsolationFloor::Off),
            ..base
        };
        assert_eq!(floor_in_force(&explicit_off, false), IsolationFloor::Off);
    }

    #[test]
    fn queue_agent_inherits_default_floor_when_sandbox_unresolvable() {
        let sb = SandboxConfig::default();
        match queue_agent_gate(
            &sb,
            IsolationFloor::Off,
            OnFloorMiss::Degrade,
            None,
            IsolationClass::HostProcess,
        ) {
            AgentGate::InfraHold(reason) => {
                assert!(reason.to_lowercase().contains("infrastructure"));
                assert!(reason.contains("isolation_floor"));
            }
            other => panic!("expected infra hold, got {other:?}"),
        }
    }

    #[test]
    fn queue_agent_inherits_default_floor_on_host_process_class() {
        let sb = SandboxConfig::default();
        assert!(matches!(
            queue_agent_gate(
                &sb,
                IsolationFloor::Off,
                OnFloorMiss::Degrade,
                Some(IsolationClass::HostProcess),
                IsolationClass::HostProcess,
            ),
            AgentGate::InfraHold(_)
        ));
    }

    #[test]
    fn queue_agent_degrade_cannot_weaken_default_floor() {
        let sb = SandboxConfig::default();
        assert!(matches!(
            queue_agent_gate(
                &sb,
                IsolationFloor::SharedKernel,
                OnFloorMiss::Degrade,
                None,
                IsolationClass::HostProcess,
            ),
            AgentGate::InfraHold(_)
        ));
    }

    #[test]
    fn queue_agent_stricter_queue_floor_wins() {
        let sb = SandboxConfig::default();
        match queue_agent_gate(
            &sb,
            IsolationFloor::GuestKernel,
            OnFloorMiss::Fail,
            Some(IsolationClass::SharedKernel),
            IsolationClass::SharedKernel,
        ) {
            AgentGate::InfraHold(reason) => assert!(reason.contains("guest-kernel")),
            other => panic!("expected guest-kernel hold, got {other:?}"),
        }
    }

    #[test]
    fn queue_agent_explicit_sandbox_off_and_disabled_opt_out() {
        let base = SandboxConfig::default();
        let explicit_off = SandboxConfig {
            isolation_floor: Some(IsolationFloor::Off),
            ..base.clone()
        };
        assert_eq!(
            queue_agent_gate(
                &explicit_off,
                IsolationFloor::Off,
                OnFloorMiss::Degrade,
                None,
                IsolationClass::HostProcess,
            ),
            AgentGate::Run
        );
        let disabled = SandboxConfig {
            enabled: false,
            ..base
        };
        assert_eq!(
            queue_agent_gate(
                &disabled,
                IsolationFloor::Off,
                OnFloorMiss::Degrade,
                None,
                IsolationClass::HostProcess,
            ),
            AgentGate::Run
        );
    }

    #[test]
    fn queue_agent_explicit_sandbox_degrade_degrades() {
        let sb = SandboxConfig {
            isolation_floor: Some(IsolationFloor::SharedKernel),
            on_floor_miss: OnFloorMiss::Degrade,
            ..SandboxConfig::default()
        };
        assert!(matches!(
            queue_agent_gate(
                &sb,
                IsolationFloor::Off,
                OnFloorMiss::Degrade,
                None,
                IsolationClass::HostProcess,
            ),
            AgentGate::RunDegraded(_)
        ));
    }

    #[test]
    fn queue_agent_met_floor_runs() {
        let sb = SandboxConfig::default();
        assert_eq!(
            queue_agent_gate(
                &sb,
                IsolationFloor::Off,
                OnFloorMiss::Degrade,
                Some(IsolationClass::SharedKernel),
                IsolationClass::SharedKernel,
            ),
            AgentGate::Run
        );
    }
}
