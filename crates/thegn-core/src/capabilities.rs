//! One declared description of what a *resolved* sandbox can do, so the UI and
//! policy engine can degrade gracefully instead of special-casing each backend:
//! grey out a snapshot affordance (only provider kinds whose
//! [`EnvProviderKind::checkpoints`] fact holds can snapshot; no local backend
//! can), show the weaker egress guarantee honestly ([`egress_for`] never claims
//! more than the code enforces), and pick the right projection lifecycle.
//!
//! This module owns **no new policy**. Every field is an *aggregation* of the
//! existing source-of-truth predicates that already live next to the thing they
//! describe:
//! - the isolation engine — the backend's profile table
//!   ([`Backend::profile`](crate::sandbox::Backend::profile): family, events op)
//!   and [`Backend::verified`](crate::sandbox::Backend::verified),
//! - the execution placement — the `Placement` variant,
//! - the provider kind's facts — [`EnvProviderKind`] (`egress_api`,
//!   `checkpoints`, `scale_to_zero`),
//! - the network mode — [`Network`] (`[sandbox] network`),
//! - the tunnel attachment — [`SandboxSpec::vpn`](crate::sandbox::SandboxSpec).
//!
//! Those remain the source of truth; [`Capabilities`] just reads them back as one
//! value. Adding a new backend/placement updates the `match` arms here and the
//! rest of the system asks `spec.capabilities()` instead of re-deriving the same
//! booleans in every call site.

use crate::config::{EnvProviderKind, Network};
use crate::egress::plan::{EgressPath, EgressPlan};
use crate::placement::Placement;
use crate::sandbox::{Backend, BackendFamily, SandboxSpec};
use crate::sandbox_events::EventsCap;

/// How the worktree is made available inside the sandbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectionMode {
    /// Path-preserving host bind mount (local OCI, bwrap, systemd, plain host).
    Bind,
    /// FUSE/sshfs mount of a remote tree (remote placement, mountable POSIX path).
    Sshfs,
    /// Changed-files manifest sync — for backends that expose only file APIs
    /// (managed providers). The active engine lands in the sync phase.
    Sync,
    /// In-environment: the files already live where the env runs (e.g. inside a
    /// k8s pod), nothing to mount or sync from the host.
    InEnv,
}

/// How the single egress policy is realized for this sandbox — the
/// programme's one egress vocabulary (resolutions B11). [`egress_for`] is the
/// only producer, and it under-promises: no code path reports `Enforce` until a
/// thegn egress datapath exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EgressKind {
    /// A thegn egress datapath with a live, authenticated lease is the only
    /// route out, on a path the backend's manifest declares and verifies
    /// (`NetnsBridge`, `SidecarNetns`, `LoopbackPort` on Seatbelt,
    /// `InternalRelay` with engine evidence, `WfpProxyOnly` with a proxy-peer
    /// identity). Unreachable until the backend manifest (WS02-P2-02) and the
    /// egress plan builder (WS04-P2-08) land. DNS filtering and a VPN sidecar
    /// are never reported as `Enforce`.
    Enforce,
    /// The policy is lowered to provider- or VMM-native controls that applied
    /// (and were read back where possible), or a manifest-native egress.
    /// Provider arm today: the placement's kind has
    /// [`EnvProviderKind::egress_api`] (Sprites, whose policy is applied and
    /// verified before handoff).
    Translate,
    /// Proxy env plus a kernel pin to the lease port on any address (a
    /// Landlock-only TCP pin; a WFP proxy-only filter without peer identity).
    /// Not a boundary; a sealed profile refuses it.
    PortPinned,
    /// `network = "none"` on a verified backend with its own network namespace
    /// (or VM): there is no route out. `network = "none"` is never reported as
    /// `Enforce`.
    Offline,
    /// Everything else: `network_allow`/`network_block` lists without
    /// enforcement, `nat`/`host`, a VPN alone, unverified backends, and every
    /// provider without an egress policy API.
    Unmanaged,
}

/// The inputs of [`egress_for`]. Built only through [`EgressInputs::local`]
/// and the builders, so a new input (the provider egress report, WS08-P7-01)
/// never changes `egress_for`'s signature.
#[derive(Debug, Clone, Copy)]
pub struct EgressInputs<'a> {
    backend: Backend,
    placement: &'a Placement,
    network: Network,
    plan: Option<&'a EgressPlan>,
}

impl<'a> EgressInputs<'a> {
    /// The inputs of a launch with no egress plan.
    pub fn local(backend: Backend, placement: &'a Placement, network: Network) -> Self {
        EgressInputs {
            backend,
            placement,
            network,
            plan: None,
        }
    }

    /// The launch's egress plan (`SandboxSpec.egress`), if any.
    #[must_use]
    pub fn with_plan(self, plan: Option<&'a EgressPlan>) -> Self {
        EgressInputs { plan, ..self }
    }
}

/// The honest egress kind of a launch (resolutions B11). Rules, in order:
/// 1. a provider placement is `Translate` iff its kind has an egress policy
///    API ([`EnvProviderKind::egress_api`]), else `Unmanaged`;
/// 2. `network = "none"` is `Offline` on a verified backend with its own
///    network namespace placed locally or over ssh, else `Unmanaged` — never
///    `Enforce`;
/// 3. otherwise a lease plan decides by its path; with no plan the launch is
///    `Unmanaged`.
pub fn egress_for(i: &EgressInputs<'_>) -> EgressKind {
    if let Placement::Provider(p) = i.placement {
        return if EnvProviderKind::of(&p.provider).egress_api() {
            EgressKind::Translate
        } else {
            EgressKind::Unmanaged
        };
    }
    match i.network {
        Network::None => offline_or_unmanaged(i.backend, i.placement),
        Network::Nat | Network::Host => match i.plan {
            Some(plan) => plan_kind(i.backend, plan),
            None => EgressKind::Unmanaged,
        },
    }
}

/// Rule 2: `network = "none"` is `Offline` only where the backend owns a
/// network namespace (or VM) we placed and its verbs are verified.
fn offline_or_unmanaged(backend: Backend, placement: &Placement) -> EgressKind {
    let own_netns_when_none = match backend.profile().family {
        // Apple `network = "none"` is not expressible yet (refused at launch).
        BackendFamily::Oci => backend != Backend::Apple,
        BackendFamily::Bwrap | BackendFamily::Systemd => true,
        // Host process, Job Object, AppContainer: the host network stack.
        BackendFamily::WinAppContainer | BackendFamily::WinJobObject | BackendFamily::Host => false,
    };
    // A k8s pod's network namespace is not ours; a provider is rule 1.
    let placed = matches!(placement, Placement::Local | Placement::Ssh(_));
    if own_netns_when_none && placed && backend.verified() {
        EgressKind::Offline
    } else {
        EgressKind::Unmanaged
    }
}

/// Rule 3: the kind a lease plan earns, by its datapath.
fn plan_kind(backend: Backend, plan: &EgressPlan) -> EgressKind {
    match plan.path {
        EgressPath::PortPin => EgressKind::PortPinned,
        // B42: `Enforce` only with a proxy-peer identity. The plan carries
        // none yet, so a WFP proxy-only filter is at best port-pinned.
        EgressPath::WfpProxyOnly => EgressKind::PortPinned,
        // Without per-engine evidence the launch is refused elsewhere; the
        // claim is `Unmanaged`.
        EgressPath::InternalRelay if !proxy_path_credited(backend, EgressPath::InternalRelay) => {
            EgressKind::Unmanaged
        }
        path if proxy_path_credited(backend, path) => EgressKind::Enforce,
        _ => EgressKind::Unmanaged,
    }
}

/// Whether the backend's manifest lists `path` as a verified proxy datapath
/// (B11, B22). WS02-P2-02 replaces this body with the manifest read (no
/// signature change). Until then no path is credited.
fn proxy_path_credited(_backend: Backend, _path: EgressPath) -> bool {
    false
}

/// The provider kind of a provider placement.
fn provider_kind(placement: &Placement) -> Option<EnvProviderKind> {
    match placement {
        Placement::Provider(p) => Some(EnvProviderKind::of(&p.provider)),
        Placement::Local | Placement::Ssh(_) | Placement::K8s(_) => None,
    }
}

/// How much structured observability this sandbox can produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObsLevel {
    /// A full structured event stream we instrument ourselves: the backend's
    /// events op is implemented (`EventsCap::Yes`, podman today) on a local or
    /// ssh placement.
    Instrumented,
    /// Reserved: no provider event normaliser exists, so nothing reports this.
    ProviderStream,
    /// Only coarse host-side signals (CPU-activity FSM); no per-process events.
    StatsOnly,
}

/// The kind of boundary that actually separates the workload from the host —
/// "what would have to fail for an escape". This is the *honest* isolation class:
/// it never claims more than the backend/placement provides, so a `sealed`
/// container is reported as a shared host kernel, not as VM-grade isolation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IsolationClass {
    /// A plain host process — no container/VM kernel boundary at all (the `none`
    /// backend, and the Windows Job Object, whose kill-on-close job is
    /// process-tree lifetime/resource scoping, **not** a filesystem or network
    /// boundary). thegn applies no LSM (Landlock/Seatbelt) to it, so nothing but
    /// resource/lifetime scoping — where a wrapper provides it — confines it.
    HostProcess,
    /// A container: namespaces + cgroups + caps/seccomp, but the workload's
    /// syscalls still execute in the **same host (or node) kernel**. A kernel LPE
    /// in any allowed syscall path escapes it, no matter how locked-down.
    SharedKernel,
    /// A userspace application kernel (gVisor's Sentry) services the workload's
    /// syscalls; the host kernel sees only a small allowlist from the Sentry.
    UserspaceKernel,
    /// A hardware-virtualized **guest kernel** (microVM / libkrun / Apple
    /// container). The host kernel sees KVM ioctls + virtio I/O, not the guest
    /// syscall ABI.
    GuestKernel,
    /// The boundary is enforced by an external managed provider's own
    /// infrastructure (e.g. Sprites microVMs). thegn cannot verify or run its
    /// own datapath inside it — you are trusting the provider's TCB and operator.
    ProviderManaged,
}

/// The aggregated capability declaration for a resolved [`SandboxSpec`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    /// The honest boundary class — what would have to fail for an escape.
    pub isolation: IsolationClass,
    pub projection: ProjectionMode,
    pub egress: EgressKind,
    pub observability: ObsLevel,
    /// The backend can snapshot/checkpoint filesystem+memory state.
    pub can_snapshot: bool,
    /// The backend can suspend and later resume the environment.
    pub can_suspend_resume: bool,
    /// Per-request cost is metered (model traffic via the proxy, and/or the
    /// provider's own billing).
    pub meters_cost: bool,
}

impl Capabilities {
    /// Derive the capabilities of a resolved spec.
    pub fn derive(spec: &SandboxSpec) -> Self {
        Self::from_parts(
            spec.backend,
            &spec.placement,
            spec.network,
            spec.vpn.is_some(),
            spec.oci_runtime.as_deref(),
        )
        .with_plan(
            spec.backend,
            &spec.placement,
            effective_network(spec.network, spec.vpn.is_some()),
            spec.egress.as_ref(),
        )
    }

    /// Recompute `egress` with the launch's egress plan.
    fn with_plan(
        self,
        backend: Backend,
        placement: &Placement,
        network: Network,
        plan: Option<&EgressPlan>,
    ) -> Self {
        Capabilities {
            egress: egress_for(&EgressInputs::local(backend, placement, network).with_plan(plan)),
            ..self
        }
    }

    /// The pure derivation, factored out of [`SandboxSpec`] so it is trivially
    /// unit-testable without constructing the full spec struct.
    ///
    /// Note: [`ProjectionMode`] is derived from the placement here as the v1
    /// heuristic. Once the resolved `DataMode` is threaded onto the spec (the
    /// projection phase) this should consult it directly — a remote placement may
    /// be `sshfs` *or* `in_env`, which the placement alone cannot distinguish.
    ///
    /// `oci_runtime` is the resolved `[sandbox] oci_runtime` (e.g. `"runsc"`,
    /// `"krun"`); it raises the honest isolation class for engines that accept
    /// `--runtime` (`podman`, `podman-rootful`, `docker`), and is ignored by
    /// every other backend.
    ///
    /// `network` is the resolved `[sandbox] network`; callers that read only
    /// `isolation` pass [`Network::Nat`].
    pub fn from_parts(
        backend: Backend,
        placement: &Placement,
        network: Network,
        has_vpn: bool,
        oci_runtime: Option<&str>,
    ) -> Self {
        Self::from_parts_on(
            backend,
            placement,
            network,
            has_vpn,
            oci_runtime,
            crate::sandbox_backend::host_os(),
        )
    }

    /// [`from_parts`](Self::from_parts) with the host OS explicit, so every
    /// platform's classification is unit-testable from one machine — the idiom
    /// `sandbox_backend::backend_suitable_on` and `sandbox_cpucap::reachable_on`
    /// already use.
    pub fn from_parts_on(
        backend: Backend,
        placement: &Placement,
        network: Network,
        has_vpn: bool,
        oci_runtime: Option<&str>,
        os: crate::sandbox_backend::HostOs,
    ) -> Self {
        let kind = provider_kind(placement);
        let network = effective_network(network, has_vpn);
        Capabilities {
            isolation: isolation_for(backend, placement, oci_runtime, os),
            projection: projection_for(placement),
            egress: egress_for(&EgressInputs::local(backend, placement, network)),
            observability: obs_for(backend, placement),
            // Only a provider kind with native checkpoints can snapshot; no
            // local backend can (nothing runs `podman checkpoint`).
            can_snapshot: kind.is_some_and(EnvProviderKind::checkpoints),
            can_suspend_resume: kind.is_some_and(EnvProviderKind::scale_to_zero),
            // The proxy meters model traffic for every sandbox, but that is a
            // property of the proxy, not the sandbox backend; here `meters_cost`
            // means the *backend itself* bills (every provider kind with a
            // lifecycle API does; an `exec_command`-only custom provider does not).
            meters_cost: kind.is_some_and(|k| k != EnvProviderKind::Custom),
        }
    }
}

/// The network the egress rules see. A VPN sidecar join replaces
/// `--network none` with `--network container:<sidecar>`, so a VPN is a route
/// out: never `Offline`, never `Enforce`.
fn effective_network(network: Network, has_vpn: bool) -> Network {
    if has_vpn { Network::Nat } else { network }
}

fn isolation_for(
    backend: Backend,
    placement: &Placement,
    oci_runtime: Option<&str>,
    os: crate::sandbox_backend::HostOs,
) -> IsolationClass {
    // Placement decides first when it owns the boundary: a managed provider runs
    // the workload in its own infra, and a k8s pod is a container on a node we
    // don't control — both honestly a kernel we cannot harden ourselves.
    match placement {
        Placement::Provider(_) => return IsolationClass::ProviderManaged,
        // A pod shares its node's kernel (unless the cluster opts into a VM
        // RuntimeClass like Kata, which we can't detect — so under-promise).
        Placement::K8s(_) => return IsolationClass::SharedKernel,
        Placement::Local | Placement::Ssh(_) => {}
    }
    // A stronger OCI runtime raises the honest class: gVisor's `runsc` services
    // syscalls in a userspace kernel; libkrun's `krun` boots the container inside
    // a hardware-virtualized microVM. Only engines whose create command accepts
    // `--runtime` honor it (Apple's `container` rejects the flag); every other
    // backend falls through to its normal arm, so a config string can never
    // claim a boundary the engine did not deliver. `runc`/`crun`/unset also fall
    // through to the shared-kernel default below.
    if backend.accepts_runtime_flag() {
        match oci_runtime.map(str::trim) {
            Some("runsc") => return IsolationClass::UserspaceKernel,
            Some("krun") => return IsolationClass::GuestKernel,
            _ => {}
        }
    }
    // A LOCAL OCI container on macOS is not a host-kernel container: there is no
    // Linux kernel to share, so podman/docker run it inside a VM. Relative to
    // THIS Mac the boundary is a guest kernel — the same class `Backend::Apple`
    // already gets, and for the same reason. Reporting `SharedKernel` here
    // UNDER-promises, which is the opposite of every other honesty bug in this
    // area but just as wrong.
    //
    // Scoped to `Placement::Local` deliberately: an ssh'd host's OS is unknown,
    // and a Mac driving a Linux box must not inherit the laptop's VM. Placed
    // after the `runsc`/`krun` arms so an explicit stronger runtime still wins.
    //
    // This measures the SYSCALL boundary only. Escaping the container on a Mac
    // lands you in a Linux VM that has the worktree mounted rw — that file
    // exposure is modelled by `ProjectionMode` + `file_access`, not by this
    // class — so `escape_note`'s "escape needs a VMM/KVM bug" stays true for
    // reaching the Mac's own kernel.
    //
    // Windows/WSL2 is also a VM but stays `SharedKernel`: `backend_suitable_on`
    // already rejects every OCI backend there except the aspirational
    // `Backend::Wsl` stub, and reclassifying a backend that cannot run would be
    // claiming thought we have not done.
    if matches!(placement, Placement::Local)
        && backend.is_oci()
        && os == crate::sandbox_backend::HostOs::MacOs
    {
        return IsolationClass::GuestKernel;
    }
    match backend {
        // Plain host processes, a Windows Job Object (lifetime/resource
        // scoping only), and the reserved AppContainer (no token/ACL/network
        // implementation) bind no mount/network namespace. They satisfy no
        // isolation floor at `shared-kernel` or above.
        Backend::None | Backend::WinAppContainer | Backend::WinJobObject => {
            IsolationClass::HostProcess
        }
        // Apple's `container` runs each container in its own lightweight VM.
        Backend::Apple => IsolationClass::GuestKernel,
        // The reserved smol and wsl under-promise as shared-kernel; neither is
        // ever selected.
        Backend::Podman
        | Backend::PodmanRootful
        | Backend::Docker
        | Backend::Smol
        | Backend::Bwrap
        | Backend::Systemd
        | Backend::Wsl => IsolationClass::SharedKernel,
    }
}

fn projection_for(placement: &Placement) -> ProjectionMode {
    match placement {
        Placement::Local => ProjectionMode::Bind,
        Placement::Ssh(_) => ProjectionMode::Sshfs,
        Placement::K8s(_) => ProjectionMode::InEnv,
        Placement::Provider(_) => ProjectionMode::Sync,
    }
}

fn obs_for(backend: Backend, placement: &Placement) -> ObsLevel {
    // Only a backend whose events op is implemented (podman) feeds the
    // timeline a structured stream, and only where we run its engine. Docker
    // and Apple have a reserved events op; providers have no normaliser.
    let placed = matches!(placement, Placement::Local | Placement::Ssh(_));
    if placed && backend.profile().events == EventsCap::Yes {
        ObsLevel::Instrumented
    } else {
        ObsLevel::StatsOnly
    }
}

impl IsolationClass {
    pub fn as_str(self) -> &'static str {
        match self {
            IsolationClass::HostProcess => "host-process",
            IsolationClass::SharedKernel => "shared-kernel",
            IsolationClass::UserspaceKernel => "userspace-kernel",
            IsolationClass::GuestKernel => "guest-kernel",
            IsolationClass::ProviderManaged => "provider-managed",
        }
    }

    /// This class's rung on the containment ladder, weakest to strongest:
    /// host-process < shared-kernel < userspace-kernel < guest-kernel. `None`
    /// for [`ProviderManaged`](IsolationClass::ProviderManaged), which is
    /// deliberately **outside** the order — a provider placement is trusted, not
    /// ranked, so it can neither satisfy nor miss an isolation floor (see
    /// [`crate::sandbox_floor`]).
    pub fn rank(self) -> Option<u8> {
        Some(match self {
            IsolationClass::HostProcess => 0,
            IsolationClass::SharedKernel => 1,
            IsolationClass::UserspaceKernel => 2,
            IsolationClass::GuestKernel => 3,
            IsolationClass::ProviderManaged => return None,
        })
    }

    /// A one-line, honest description of "what would have to fail for an escape".
    pub fn escape_note(self) -> &'static str {
        match self {
            IsolationClass::HostProcess => {
                "no kernel boundary; resource/lifetime scoping only where wrapped"
            }
            IsolationClass::SharedKernel => {
                "a kernel exploit in any allowed syscall reaches the host"
            }
            IsolationClass::UserspaceKernel => "escape needs a gVisor Sentry or host-allowlist bug",
            IsolationClass::GuestKernel => "escape needs a VMM/KVM bug",
            IsolationClass::ProviderManaged => "you trust the provider's TCB and operator",
        }
    }
}
impl std::fmt::Display for IsolationClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl ProjectionMode {
    pub fn as_str(self) -> &'static str {
        match self {
            ProjectionMode::Bind => "bind",
            ProjectionMode::Sshfs => "sshfs",
            ProjectionMode::Sync => "sync",
            ProjectionMode::InEnv => "in_env",
        }
    }
}
impl std::fmt::Display for ProjectionMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl EgressKind {
    pub fn as_str(self) -> &'static str {
        match self {
            EgressKind::Enforce => "enforce",
            EgressKind::Translate => "translate",
            EgressKind::PortPinned => "port-pinned",
            EgressKind::Offline => "offline",
            EgressKind::Unmanaged => "unmanaged",
        }
    }
}
impl std::fmt::Display for EgressKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl ObsLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            ObsLevel::Instrumented => "instrumented",
            ObsLevel::ProviderStream => "provider_stream",
            ObsLevel::StatsOnly => "stats_only",
        }
    }
}
impl std::fmt::Display for ObsLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::egress::plan::{LeaseId, LeaseToken, ProxyAuth, ProxyEndpoint};
    use crate::placement::{K8sPlacement, ProviderPlacement, SshPlacement, TransportKind};
    use crate::sandbox_backend::HostOs;
    use crate::seam::Kind as _;

    const ALL_OS: [HostOs; 4] = [HostOs::Linux, HostOs::MacOs, HostOs::Windows, HostOs::Other];
    const ALL_PATHS: [EgressPath; 8] = [
        EgressPath::NetnsBridge,
        EgressPath::SidecarNetns,
        EgressPath::InternalRelay,
        EgressPath::LoopbackPort,
        EgressPath::PortPin,
        EgressPath::WfpProxyOnly,
        EgressPath::ProviderNative,
        EgressPath::External,
    ];

    fn ssh() -> Placement {
        Placement::Ssh(SshPlacement::plain(
            "host".into(),
            22,
            false,
            TransportKind::Ssh,
        ))
    }
    fn k8s() -> Placement {
        Placement::K8s(K8sPlacement {
            kubectl: "kubectl".into(),
            context: None,
            namespace: None,
            pod: "pod".into(),
            container: None,
            pod_template: None,
            image: None,
        })
    }
    fn provider_named(name: &str) -> Placement {
        Placement::Provider(ProviderPlacement {
            provider: name.into(),
            id: "abc".into(),
            interactive_prefix: vec![],
            control_prefix: vec![],
            up_command: vec![],
            down_command: vec![],
        })
    }
    /// An `exec_command`-only provider (unknown name: the `custom` kind).
    fn provider() -> Placement {
        provider_named("e2b")
    }
    fn provider_of(kind: EnvProviderKind) -> Placement {
        provider_named(kind.as_str())
    }
    fn plan(path: EgressPath) -> EgressPlan {
        EgressPlan {
            lease: LeaseId([0; 16]),
            path,
            endpoint: ProxyEndpoint::Relay {
                host: "relay.internal".into(),
                port: 3128,
            },
            auth: ProxyAuth::Token {
                lease_user: "lease-0000".into(),
                token: LeaseToken::new("t".into()),
            },
            env: vec![],
            binds: vec![],
            policy_digest: [0; 32],
        }
    }
    fn caps(backend: Backend, placement: &Placement, network: Network, vpn: bool) -> Capabilities {
        Capabilities::from_parts_on(backend, placement, network, vpn, None, HostOs::Linux)
    }
    fn egress_with_plan(backend: Backend, path: EgressPath) -> EgressKind {
        let plan = plan(path);
        egress_for(
            &EgressInputs::local(backend, &Placement::Local, Network::Nat).with_plan(Some(&plan)),
        )
    }

    #[test]
    fn local_oci_nat_is_unmanaged_and_podman_instrumented() {
        for backend in [Backend::Podman, Backend::PodmanRootful] {
            let c = caps(backend, &Placement::Local, Network::Nat, false);
            assert_eq!(c.projection, ProjectionMode::Bind);
            assert_eq!(c.egress, EgressKind::Unmanaged, "{backend:?}");
            assert_eq!(c.observability, ObsLevel::Instrumented, "{backend:?}");
            assert!(!c.can_snapshot);
            assert!(!c.can_suspend_resume);
            assert!(!c.meters_cost);
        }
        let over_ssh = caps(Backend::Podman, &ssh(), Network::Nat, false);
        assert_eq!(over_ssh.projection, ProjectionMode::Sshfs);
        assert_eq!(over_ssh.egress, EgressKind::Unmanaged);
        assert_eq!(over_ssh.observability, ObsLevel::Instrumented);
    }

    #[test]
    fn docker_apple_are_stats_only_events_reserved() {
        for backend in [Backend::Docker, Backend::Apple] {
            let c = caps(backend, &Placement::Local, Network::Nat, false);
            assert_eq!(c.observability, ObsLevel::StatsOnly, "{backend:?}");
            assert_eq!(c.egress, EgressKind::Unmanaged, "{backend:?}");
        }
        // No backend or placement reports the reserved provider stream.
        for backend in Backend::ALL {
            for placement in [Placement::Local, ssh(), k8s(), provider()] {
                let c = caps(backend, &placement, Network::Nat, false);
                assert_ne!(c.observability, ObsLevel::ProviderStream);
            }
        }
        // A k8s pod's engine is not ours, even for podman.
        assert_eq!(
            caps(Backend::Podman, &k8s(), Network::Nat, false).observability,
            ObsLevel::StatsOnly
        );
    }

    #[test]
    fn network_none_is_offline_on_verified_own_netns_backends() {
        for backend in [
            Backend::Podman,
            Backend::PodmanRootful,
            Backend::Docker,
            Backend::Bwrap,
            Backend::Systemd,
        ] {
            for placement in [Placement::Local, ssh()] {
                assert_eq!(
                    caps(backend, &placement, Network::None, false).egress,
                    EgressKind::Offline,
                    "{backend:?} on {placement:?}"
                );
            }
        }
    }

    #[test]
    fn network_none_unmanaged_on_apple_host_process_reserved_and_k8s() {
        for backend in [
            Backend::Apple,
            Backend::None,
            Backend::WinJobObject,
            Backend::WinAppContainer,
            Backend::Smol,
            Backend::Wsl,
        ] {
            assert_eq!(
                caps(backend, &Placement::Local, Network::None, false).egress,
                EgressKind::Unmanaged,
                "{backend:?}"
            );
        }
        assert_eq!(
            caps(Backend::Podman, &k8s(), Network::None, false).egress,
            EgressKind::Unmanaged
        );
    }

    #[test]
    fn vpn_never_enforce_nor_offline() {
        for backend in Backend::ALL {
            for network in Network::ALL.iter().copied() {
                for placement in [Placement::Local, ssh(), k8s()] {
                    let egress = caps(backend, &placement, network, true).egress;
                    assert_eq!(
                        egress,
                        EgressKind::Unmanaged,
                        "{backend:?} {network:?} {placement:?} with a VPN"
                    );
                }
            }
        }
    }

    #[test]
    fn bwrap_with_vpn_is_unmanaged() {
        // A tunnel is not an exclusive route: it governs nothing.
        let c = caps(Backend::Bwrap, &Placement::Local, Network::Nat, true);
        assert_eq!(c.egress, EgressKind::Unmanaged);
    }

    #[test]
    fn host_toolchain_local_is_stats_only_unmanaged() {
        let c = caps(Backend::Bwrap, &Placement::Local, Network::Nat, false);
        assert_eq!(c.projection, ProjectionMode::Bind);
        assert_eq!(c.egress, EgressKind::Unmanaged);
        assert_eq!(c.observability, ObsLevel::StatsOnly);
        let none = caps(Backend::None, &Placement::Local, Network::Nat, false);
        assert_eq!(none.egress, EgressKind::Unmanaged);
        assert_eq!(none.observability, ObsLevel::StatsOnly);
    }

    #[test]
    fn k8s_placement_is_in_env() {
        let c = caps(Backend::Podman, &k8s(), Network::Nat, false);
        assert_eq!(c.projection, ProjectionMode::InEnv);
        assert_eq!(c.egress, EgressKind::Unmanaged);
    }

    #[test]
    fn port_pin_plan_is_port_pinned() {
        for backend in Backend::ALL {
            assert_eq!(
                egress_with_plan(backend, EgressPath::PortPin),
                EgressKind::PortPinned
            );
        }
    }

    #[test]
    fn wfp_without_peer_is_port_pinned() {
        for backend in Backend::ALL {
            assert_eq!(
                egress_with_plan(backend, EgressPath::WfpProxyOnly),
                EgressKind::PortPinned
            );
        }
    }

    #[test]
    fn internal_relay_without_evidence_is_unmanaged() {
        for backend in Backend::ALL {
            assert_eq!(
                egress_with_plan(backend, EgressPath::InternalRelay),
                EgressKind::Unmanaged
            );
        }
    }

    #[test]
    fn uncredited_proxy_path_is_unmanaged() {
        for backend in Backend::ALL {
            for path in [
                EgressPath::NetnsBridge,
                EgressPath::SidecarNetns,
                EgressPath::LoopbackPort,
                EgressPath::ProviderNative,
                EgressPath::External,
            ] {
                assert_eq!(
                    egress_with_plan(backend, path),
                    EgressKind::Unmanaged,
                    "{backend:?} {path:?}"
                );
            }
        }
    }

    #[test]
    fn plan_with_network_none_stays_offline_or_unmanaged() {
        // `network = "none"` decides before any plan: no route, no proxy.
        let plan = plan(EgressPath::PortPin);
        let kind = egress_for(
            &EgressInputs::local(Backend::Bwrap, &Placement::Local, Network::None)
                .with_plan(Some(&plan)),
        );
        assert_eq!(kind, EgressKind::Offline);
    }

    #[test]
    fn sprites_translates() {
        let sprites = provider_of(EnvProviderKind::Sprites);
        for network in Network::ALL.iter().copied() {
            for vpn in [false, true] {
                assert_eq!(
                    caps(Backend::Podman, &sprites, network, vpn).egress,
                    EgressKind::Translate,
                    "{network:?} vpn={vpn}"
                );
            }
        }
    }

    #[test]
    fn provider_without_egress_api_is_unmanaged() {
        for kind in [
            EnvProviderKind::Daytona,
            EnvProviderKind::Hetzner,
            EnvProviderKind::DigitalOcean,
            EnvProviderKind::Fly,
            EnvProviderKind::Machine0,
            EnvProviderKind::Custom,
        ] {
            assert_eq!(
                caps(Backend::Podman, &provider_of(kind), Network::Nat, false).egress,
                EgressKind::Unmanaged,
                "{kind:?}"
            );
        }
        assert_eq!(
            caps(Backend::Podman, &provider(), Network::Nat, false).egress,
            EgressKind::Unmanaged
        );
    }

    #[test]
    fn egress_api_kinds_are_exactly_translate_kinds() {
        for kind in EnvProviderKind::ALL.iter().copied() {
            let egress = caps(Backend::None, &provider_of(kind), Network::Nat, false).egress;
            assert_eq!(
                egress == EgressKind::Translate,
                kind.egress_api(),
                "{kind:?}"
            );
        }
    }

    #[test]
    fn sprites_provider_snapshots_suspends_meters() {
        let c = caps(
            Backend::Podman,
            &provider_of(EnvProviderKind::Sprites),
            Network::Nat,
            false,
        );
        assert_eq!(c.projection, ProjectionMode::Sync);
        assert_eq!(c.observability, ObsLevel::StatsOnly);
        assert!(c.can_snapshot);
        assert!(c.can_suspend_resume);
        assert!(c.meters_cost);
    }

    #[test]
    fn daytona_provider_no_snapshot_no_suspend_meters() {
        let c = caps(
            Backend::Podman,
            &provider_of(EnvProviderKind::Daytona),
            Network::Nat,
            false,
        );
        assert!(!c.can_snapshot);
        assert!(!c.can_suspend_resume);
        assert!(c.meters_cost);
    }

    #[test]
    fn fly_provider_suspends_no_snapshot() {
        let c = caps(
            Backend::Podman,
            &provider_of(EnvProviderKind::Fly),
            Network::Nat,
            false,
        );
        assert!(!c.can_snapshot);
        assert!(c.can_suspend_resume);
        assert!(c.meters_cost);
    }

    #[test]
    fn machine0_provider_snapshots_and_suspends() {
        let c = caps(
            Backend::Podman,
            &provider_of(EnvProviderKind::Machine0),
            Network::Nat,
            false,
        );
        assert!(c.can_snapshot);
        assert!(c.can_suspend_resume);
        assert!(c.meters_cost);
    }

    #[test]
    fn custom_provider_does_not_meter() {
        let c = caps(Backend::Podman, &provider(), Network::Nat, false);
        assert!(!c.can_snapshot);
        assert!(!c.can_suspend_resume);
        assert!(!c.meters_cost);
    }

    #[test]
    fn podman_local_cannot_snapshot() {
        // No local backend can snapshot (B25): nothing runs `podman checkpoint`.
        for backend in Backend::ALL {
            for placement in [Placement::Local, ssh(), k8s()] {
                let c = caps(backend, &placement, Network::Nat, false);
                assert!(!c.can_snapshot, "{backend:?} {placement:?}");
                assert!(!c.can_suspend_resume, "{backend:?} {placement:?}");
                assert!(!c.meters_cost, "{backend:?} {placement:?}");
            }
        }
    }

    /// The regression gate: nothing reports `Enforce` until a thegn egress
    /// datapath exists. WS02-P2-02 and WS04-P2-08 edit this gate deliberately.
    #[test]
    fn no_backend_or_placement_reports_enforce() {
        let mut placements = vec![Placement::Local, ssh(), k8s(), provider()];
        placements.extend(EnvProviderKind::ALL.iter().copied().map(provider_of));
        let plans: Vec<Option<EgressPlan>> = std::iter::once(None)
            .chain(ALL_PATHS.iter().map(|path| Some(plan(*path))))
            .collect();
        for backend in Backend::ALL {
            for placement in &placements {
                for vpn in [false, true] {
                    for network in Network::ALL.iter().copied() {
                        for plan in &plans {
                            for os in ALL_OS {
                                let c = Capabilities::from_parts_on(
                                    backend, placement, network, vpn, None, os,
                                )
                                .with_plan(
                                    backend,
                                    placement,
                                    effective_network(network, vpn),
                                    plan.as_ref(),
                                );
                                assert_ne!(
                                    c.egress,
                                    EgressKind::Enforce,
                                    "{backend:?} {placement:?} vpn={vpn} {network:?} {plan:?} {os:?}"
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn oci_runtime_raises_isolation_class_honestly() {
        // gVisor: userspace kernel. libkrun: guest kernel. Both keep the local
        // container's path-preserving bind — the whole point of a runtime
        // modifier vs a remote provider.
        let runsc = Capabilities::from_parts_on(
            Backend::Podman,
            &Placement::Local,
            Network::Nat,
            false,
            Some("runsc"),
            HostOs::Linux,
        );
        assert_eq!(runsc.isolation, IsolationClass::UserspaceKernel);
        assert_eq!(runsc.projection, ProjectionMode::Bind);
        assert_eq!(runsc.egress, EgressKind::Unmanaged);

        let krun = Capabilities::from_parts_on(
            Backend::Docker,
            &Placement::Local,
            Network::Nat,
            false,
            Some("krun"),
            HostOs::Linux,
        );
        assert_eq!(krun.isolation, IsolationClass::GuestKernel);
        assert_eq!(krun.projection, ProjectionMode::Bind);

        // runc/crun/unknown stay shared-kernel; non-OCI backends ignore it.
        assert_eq!(
            Capabilities::from_parts_on(
                Backend::Podman,
                &Placement::Local,
                Network::Nat,
                false,
                Some("crun"),
                HostOs::Linux,
            )
            .isolation,
            IsolationClass::SharedKernel
        );
        assert_eq!(
            Capabilities::from_parts_on(
                Backend::Bwrap,
                &Placement::Local,
                Network::Nat,
                false,
                Some("krun"),
                HostOs::Linux,
            )
            .isolation,
            IsolationClass::SharedKernel
        );
        // A managed provider still owns the boundary regardless of runtime.
        assert_eq!(
            Capabilities::from_parts(
                Backend::Podman,
                &provider(),
                Network::Nat,
                false,
                Some("krun")
            )
            .isolation,
            IsolationClass::ProviderManaged
        );
    }

    #[test]
    fn apple_runsc_stays_guest_kernel_not_userspace() {
        // Apple's `container` has no `--runtime` flag, so a configured `runsc`
        // is never delivered. Its real boundary is the per-container VM, and
        // reporting `UserspaceKernel` because of a config string would
        // under-report the engine's actual isolation.
        let apple = Capabilities::from_parts_on(
            Backend::Apple,
            &Placement::Local,
            Network::Nat,
            false,
            Some("runsc"),
            HostOs::MacOs,
        );
        assert_eq!(apple.isolation, IsolationClass::GuestKernel);
    }

    #[test]
    fn smol_runsc_class_unchanged() {
        // `smol` is a reserved docker clone with no `--runtime` flag; setting
        // the config value must not invent a class the backend never delivered.
        let with_runtime = Capabilities::from_parts_on(
            Backend::Smol,
            &Placement::Local,
            Network::Nat,
            false,
            Some("runsc"),
            HostOs::Linux,
        );
        let without_runtime = Capabilities::from_parts_on(
            Backend::Smol,
            &Placement::Local,
            Network::Nat,
            false,
            None,
            HostOs::Linux,
        );
        assert_eq!(with_runtime.isolation, without_runtime.isolation);
        assert_eq!(with_runtime.isolation, IsolationClass::SharedKernel);
    }

    #[test]
    fn enum_strings_round_trip_for_ui() {
        assert_eq!(ProjectionMode::Sync.as_str(), "sync");
        assert_eq!(EgressKind::Translate.to_string(), "translate");
        assert_eq!(ObsLevel::StatsOnly.to_string(), "stats_only");
        assert_eq!(ObsLevel::ProviderStream.to_string(), "provider_stream");
        assert_eq!(ObsLevel::Instrumented.to_string(), "instrumented");
        assert_eq!(IsolationClass::GuestKernel.to_string(), "guest-kernel");
    }

    #[test]
    fn egress_kind_strings() {
        let all = [
            (EgressKind::Enforce, "enforce"),
            (EgressKind::Translate, "translate"),
            (EgressKind::PortPinned, "port-pinned"),
            (EgressKind::Offline, "offline"),
            (EgressKind::Unmanaged, "unmanaged"),
        ];
        for (kind, text) in all {
            assert_eq!(kind.as_str(), text);
            assert_eq!(kind.to_string(), text);
        }
    }

    /// `egress_for` is the only producer of an egress claim: no other
    /// production source in this crate constructs `PortPinned` or `Offline`.
    #[test]
    fn egress_kinds_constructed_only_in_capabilities() {
        use crate::test_support::ratchet::{code_only, sources};
        fn production(body: &str) -> &str {
            body.split_once("\n#[cfg(test)]\nmod tests {")
                .map(|(production, _)| production)
                .unwrap_or(body)
        }
        for (key, body) in sources(env!("CARGO_MANIFEST_DIR"), &[]) {
            if key.ends_with("_tests.rs") || key == "capabilities.rs" {
                continue;
            }
            let code = code_only(production(&body));
            for kind in ["EgressKind::PortPinned", "EgressKind::Offline"] {
                assert_eq!(
                    code.matches(kind).count(),
                    0,
                    "{key}: {kind} is produced only by capabilities::egress_for"
                );
            }
        }
    }

    #[test]
    fn isolation_class_is_honest_per_backend() {
        // Pinned to Linux so this asserts a FACT about the classifier rather
        // than a property of whatever machine runs the suite — on macOS a local
        // OCI container is behind a VM and correctly reports guest-kernel.
        let on_linux = |b| caps(b, &Placement::Local, Network::Nat, false).isolation;
        // Containers — including a fully-sealed one — are honestly a shared kernel.
        for b in [
            Backend::Podman,
            Backend::Docker,
            Backend::Bwrap,
            Backend::Systemd,
        ] {
            assert_eq!(
                on_linux(b),
                IsolationClass::SharedKernel,
                "{b:?} should report shared-kernel on Linux"
            );
        }
        // The plain host fallback has no kernel boundary at all.
        assert_eq!(on_linux(Backend::None), IsolationClass::HostProcess);
        // Apple's `container` runs each container in its own lightweight VM.
        assert_eq!(on_linux(Backend::Apple), IsolationClass::GuestKernel);
    }

    #[test]
    fn macos_local_oci_is_guest_kernel_behind_the_vm() {
        let on = |b, os| {
            Capabilities::from_parts_on(b, &Placement::Local, Network::Nat, false, None, os)
                .isolation
        };
        // There is no Linux kernel on a Mac to share: podman/docker run the
        // container in a VM, so relative to the Mac the boundary is a guest
        // kernel. Reporting shared-kernel here UNDER-promises.
        for b in [Backend::Podman, Backend::PodmanRootful, Backend::Docker] {
            assert_eq!(on(b, HostOs::MacOs), IsolationClass::GuestKernel, "{b:?}");
            assert_eq!(on(b, HostOs::Linux), IsolationClass::SharedKernel, "{b:?}");
        }
        // bwrap is not OCI — it is a host-namespace tool and unaffected. (It is
        // also Linux-only, so this arm is about the classifier, not reachability.)
        assert_eq!(
            on(Backend::Bwrap, HostOs::MacOs),
            IsolationClass::SharedKernel
        );
        // The host shell has no boundary anywhere.
        assert_eq!(
            on(Backend::None, HostOs::MacOs),
            IsolationClass::HostProcess
        );
        // An ssh'd host's OS is unknown — a Mac driving a Linux box must not
        // inherit the laptop's VM.
        assert_eq!(
            Capabilities::from_parts_on(
                Backend::Podman,
                &ssh(),
                Network::Nat,
                false,
                None,
                HostOs::MacOs,
            )
            .isolation,
            IsolationClass::SharedKernel
        );
        // An explicit stronger runtime still wins over the macOS arm.
        assert_eq!(
            Capabilities::from_parts_on(
                Backend::Podman,
                &Placement::Local,
                Network::Nat,
                false,
                Some("runsc"),
                HostOs::MacOs,
            )
            .isolation,
            IsolationClass::UserspaceKernel
        );
    }

    #[test]
    fn isolation_class_lets_placement_own_the_boundary() {
        // A provider runs the workload in its own infra — not a boundary we control.
        assert_eq!(
            caps(Backend::Podman, &provider(), Network::Nat, false).isolation,
            IsolationClass::ProviderManaged
        );
        // A k8s pod shares its node's kernel regardless of the local backend value.
        assert_eq!(
            caps(Backend::Podman, &k8s(), Network::Nat, false).isolation,
            IsolationClass::SharedKernel
        );
        // SSH falls through to the backend running on the remote host.
        assert_eq!(
            caps(Backend::Podman, &ssh(), Network::Nat, false).isolation,
            IsolationClass::SharedKernel
        );
    }

    #[test]
    fn windows_jobobject_is_a_host_process_never_a_container() {
        // A kill-on-close Job Object is process-tree lifetime + resource scoping,
        // with no filesystem or network namespace — so it is honestly a host
        // process, and MUST NOT be reported as a shared-kernel container (the
        // over-report this change fixes). It satisfies no floor at shared-kernel+.
        for backend in [Backend::WinJobObject, Backend::WinAppContainer] {
            assert_eq!(
                Capabilities::from_parts_on(
                    backend,
                    &Placement::Local,
                    Network::Nat,
                    false,
                    None,
                    HostOs::Windows,
                )
                .isolation,
                IsolationClass::HostProcess,
                "{backend:?}"
            );
        }
    }

    #[test]
    fn rank_orders_the_ladder_and_leaves_provider_unranked() {
        use IsolationClass::*;
        // Strictly increasing along the ladder.
        let ladder = [HostProcess, SharedKernel, UserspaceKernel, GuestKernel];
        for w in ladder.windows(2) {
            assert!(
                w[0].rank().unwrap() < w[1].rank().unwrap(),
                "{:?} < {:?}",
                w[0],
                w[1]
            );
        }
        // A provider placement is trusted, not ranked — outside the order.
        assert_eq!(ProviderManaged.rank(), None);
    }

    #[test]
    fn escape_note_is_present_for_every_class() {
        for c in [
            IsolationClass::HostProcess,
            IsolationClass::SharedKernel,
            IsolationClass::UserspaceKernel,
            IsolationClass::GuestKernel,
            IsolationClass::ProviderManaged,
        ] {
            assert!(!c.escape_note().is_empty());
            assert!(!c.as_str().is_empty());
        }
    }
}
