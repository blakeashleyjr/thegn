//! One declared description of what a *resolved* sandbox can do, so the UI and
//! policy engine can degrade gracefully instead of special-casing each backend
//! (grey out a snapshot affordance, show the weaker egress guarantee honestly,
//! pick the right projection lifecycle).
//!
//! This module owns **no new policy**. Every field is an *aggregation* of the
//! existing source-of-truth predicates that already live next to the thing they
//! describe:
//! - the isolation engine — [`Backend::is_oci`](crate::sandbox::Backend::is_oci),
//! - the execution placement — the `Placement` variant,
//! - the hardening preset — the [`SandboxProfile`](crate::config::SandboxProfile) methods,
//! - the tunnel attachment — [`SandboxSpec::vpn`](crate::sandbox::SandboxSpec).
//!
//! Those remain the source of truth; [`Capabilities`] just reads them back as one
//! value. Adding a new backend/placement updates the `match` arms here and the
//! rest of the system asks `spec.capabilities()` instead of re-deriving the same
//! booleans in every call site.

use crate::placement::Placement;
use crate::sandbox::{Backend, SandboxSpec};

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

/// How the single egress policy is realized for this sandbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EgressKind {
    /// thegn owns egress directly — the DNS filter, the VPN sidecar, and the
    /// `tgproxy` chokepoint all run on a host we control.
    Enforce,
    /// The policy is *lowered* to a managed provider's own controls (CIDR rules,
    /// credential injection); we cannot run our own datapath inside their box.
    Translate,
    /// No egress controls are available for this combination (e.g. the plain
    /// `none` backend with no tunnel).
    Unmanaged,
}

/// How much structured observability this sandbox can produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObsLevel {
    /// A full structured event stream we instrument ourselves (OCI engines expose
    /// `events`; the host also synthesizes pane exec/die for every backend).
    Instrumented,
    /// We normalize the provider's own event/file/process stream into the timeline.
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
    /// backend). Only host-side LSM policy (Landlock/Seatbelt) confines it.
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
            spec.vpn.is_some(),
            spec.oci_runtime.as_deref(),
        )
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
    /// `"krun"`); it raises the honest isolation class for OCI backends that run
    /// under a stronger runtime, and is ignored by non-OCI backends.
    pub fn from_parts(
        backend: Backend,
        placement: &Placement,
        has_vpn: bool,
        oci_runtime: Option<&str>,
    ) -> Self {
        Self::from_parts_on(
            backend,
            placement,
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
        has_vpn: bool,
        oci_runtime: Option<&str>,
        os: crate::sandbox_backend::HostOs,
    ) -> Self {
        let is_provider = matches!(placement, Placement::Provider(_));
        // podman can checkpoint/restore a container (CRIU) — a real snapshot.
        let podman_checkpoint = matches!(backend, Backend::Podman | Backend::PodmanRootful);
        Capabilities {
            isolation: isolation_for(backend, placement, oci_runtime, os),
            projection: projection_for(placement),
            egress: egress_for(backend, placement, has_vpn),
            observability: obs_for(backend, placement),
            // Managed providers expose native snapshot/suspend; local podman adds
            // snapshot via checkpoint (but not live suspend/resume).
            can_snapshot: is_provider || podman_checkpoint,
            can_suspend_resume: is_provider,
            // The proxy meters model traffic for every sandbox, but that is a
            // property of the proxy, not the sandbox backend; here `meters_cost`
            // means the *backend itself* bills (providers do).
            meters_cost: is_provider,
        }
    }
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
    // a hardware-virtualized microVM. Only the OCI backends honor `--runtime`;
    // `runc`/`crun`/unset fall through to the shared-kernel default below.
    if backend.is_oci() {
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
        Backend::None => IsolationClass::HostProcess,
        // Apple's `container` runs each container in its own lightweight VM.
        Backend::Apple => IsolationClass::GuestKernel,
        Backend::Podman
        | Backend::PodmanRootful
        | Backend::Docker
        | Backend::Smol
        | Backend::Bwrap
        | Backend::Systemd
        | Backend::Wsl
        | Backend::WinAppContainer
        | Backend::WinJobObject => IsolationClass::SharedKernel,
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

fn egress_for(backend: Backend, placement: &Placement, has_vpn: bool) -> EgressKind {
    if matches!(placement, Placement::Provider(_)) {
        // We cannot run our DNS filter / proxy inside a managed provider's box;
        // the policy is translated to their controls.
        return EgressKind::Translate;
    }
    // We can actively enforce egress when there is an OCI container to apply
    // `--dns`/network policy to, or a VPN attachment carrying the only route.
    if backend.is_oci() || has_vpn {
        EgressKind::Enforce
    } else {
        EgressKind::Unmanaged
    }
}

fn obs_for(backend: Backend, placement: &Placement) -> ObsLevel {
    if matches!(placement, Placement::Provider(_)) {
        return ObsLevel::ProviderStream;
    }
    if backend.is_oci() {
        // The OCI engine's `events` stream (exec/die/network) feeds the timeline.
        ObsLevel::Instrumented
    } else {
        // bwrap/systemd/none: only the host-side CPU-activity FSM today. The
        // timeline phase synthesizes pane exec/die for these from the host, which
        // is what lifts them toward `Instrumented` in practice.
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

    /// A one-line, honest description of "what would have to fail for an escape".
    pub fn escape_note(self) -> &'static str {
        match self {
            IsolationClass::HostProcess => {
                "no kernel boundary; only host LSM policy (Landlock/Seatbelt) confines it"
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
    use crate::placement::{K8sPlacement, ProviderPlacement, SshPlacement, TransportKind};

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
    fn provider() -> Placement {
        Placement::Provider(ProviderPlacement {
            provider: "e2b".into(),
            id: "abc".into(),
            interactive_prefix: vec![],
            control_prefix: vec![],
            up_command: vec![],
            down_command: vec![],
        })
    }

    #[test]
    fn local_oci_binds_enforces_and_is_instrumented() {
        let c = Capabilities::from_parts(Backend::Podman, &Placement::Local, false, None);
        assert_eq!(c.projection, ProjectionMode::Bind);
        assert_eq!(c.egress, EgressKind::Enforce);
        assert_eq!(c.observability, ObsLevel::Instrumented);
        // podman can checkpoint → snapshot, but no live suspend and no native billing.
        assert!(c.can_snapshot);
        assert!(!c.can_suspend_resume);
        assert!(!c.meters_cost);
    }

    #[test]
    fn bwrap_cannot_snapshot_but_podman_can() {
        let bwrap = Capabilities::from_parts(Backend::Bwrap, &Placement::Local, false, None);
        assert!(!bwrap.can_snapshot);
        let podman = Capabilities::from_parts(Backend::Podman, &Placement::Local, false, None);
        assert!(podman.can_snapshot);
    }

    #[test]
    fn host_toolchain_local_is_stats_only_unmanaged() {
        // bwrap with no OCI container and no tunnel: no egress hooks, stats only.
        let c = Capabilities::from_parts(Backend::Bwrap, &Placement::Local, false, None);
        assert_eq!(c.projection, ProjectionMode::Bind);
        assert_eq!(c.egress, EgressKind::Unmanaged);
        assert_eq!(c.observability, ObsLevel::StatsOnly);
    }

    #[test]
    fn host_toolchain_with_vpn_can_enforce() {
        // A tunnel gives a route to govern even without an OCI container.
        let c = Capabilities::from_parts(Backend::Bwrap, &Placement::Local, true, None);
        assert_eq!(c.egress, EgressKind::Enforce);
    }

    #[test]
    fn plain_none_backend_is_unmanaged_stats_only() {
        let c = Capabilities::from_parts(Backend::None, &Placement::Local, false, None);
        assert_eq!(c.egress, EgressKind::Unmanaged);
        assert_eq!(c.observability, ObsLevel::StatsOnly);
    }

    #[test]
    fn ssh_placement_projects_via_sshfs_and_enforces_for_oci() {
        let c = Capabilities::from_parts(Backend::Podman, &ssh(), false, None);
        assert_eq!(c.projection, ProjectionMode::Sshfs);
        assert_eq!(c.egress, EgressKind::Enforce);
        assert_eq!(c.observability, ObsLevel::Instrumented);
    }

    #[test]
    fn k8s_placement_is_in_env() {
        let c = Capabilities::from_parts(Backend::Podman, &k8s(), false, None);
        assert_eq!(c.projection, ProjectionMode::InEnv);
        assert_eq!(c.egress, EgressKind::Enforce);
    }

    #[test]
    fn provider_translates_streams_and_snapshots() {
        // Provider overrides backend: translate egress, provider-stream obs,
        // sync projection, and native snapshot/suspend/metering.
        let c = Capabilities::from_parts(Backend::Podman, &provider(), false, None);
        assert_eq!(c.projection, ProjectionMode::Sync);
        assert_eq!(c.egress, EgressKind::Translate);
        assert_eq!(c.observability, ObsLevel::ProviderStream);
        assert!(c.can_snapshot);
        assert!(c.can_suspend_resume);
        assert!(c.meters_cost);
    }

    #[test]
    fn oci_runtime_raises_isolation_class_honestly() {
        use crate::sandbox_backend::HostOs;
        // gVisor: userspace kernel. libkrun: guest kernel. Both keep the local
        // container's path-preserving bind + our own egress enforcement — the
        // whole point of a runtime modifier vs a remote provider.
        let runsc = Capabilities::from_parts_on(
            Backend::Podman,
            &Placement::Local,
            false,
            Some("runsc"),
            HostOs::Linux,
        );
        assert_eq!(runsc.isolation, IsolationClass::UserspaceKernel);
        assert_eq!(runsc.projection, ProjectionMode::Bind);
        assert_eq!(runsc.egress, EgressKind::Enforce);

        let krun = Capabilities::from_parts_on(
            Backend::Docker,
            &Placement::Local,
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
                false,
                Some("krun"),
                HostOs::Linux,
            )
            .isolation,
            IsolationClass::SharedKernel
        );
        // A managed provider still owns the boundary regardless of runtime.
        assert_eq!(
            Capabilities::from_parts(Backend::Podman, &provider(), false, Some("krun")).isolation,
            IsolationClass::ProviderManaged
        );
    }

    #[test]
    fn enum_strings_round_trip_for_ui() {
        assert_eq!(ProjectionMode::Sync.as_str(), "sync");
        assert_eq!(EgressKind::Translate.to_string(), "translate");
        assert_eq!(ObsLevel::StatsOnly.to_string(), "stats_only");
        assert_eq!(IsolationClass::GuestKernel.to_string(), "guest-kernel");
    }

    #[test]
    fn isolation_class_is_honest_per_backend() {
        use crate::sandbox_backend::HostOs;
        // Pinned to Linux so this asserts a FACT about the classifier rather
        // than a property of whatever machine runs the suite — on macOS a local
        // OCI container is behind a VM and correctly reports guest-kernel.
        let on_linux = |b| {
            Capabilities::from_parts_on(b, &Placement::Local, false, None, HostOs::Linux).isolation
        };
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
        use crate::sandbox_backend::HostOs;
        let on =
            |b, os| Capabilities::from_parts_on(b, &Placement::Local, false, None, os).isolation;
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
                &Placement::Ssh(crate::placement::SshPlacement::plain(
                    "box".into(),
                    22,
                    false,
                    crate::placement::TransportKind::Ssh,
                )),
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
            Capabilities::from_parts(Backend::Podman, &provider(), false, None).isolation,
            IsolationClass::ProviderManaged
        );
        // A k8s pod shares its node's kernel regardless of the local backend value.
        assert_eq!(
            Capabilities::from_parts(Backend::Podman, &k8s(), false, None).isolation,
            IsolationClass::SharedKernel
        );
        // SSH falls through to the backend running on the remote host.
        assert_eq!(
            Capabilities::from_parts(Backend::Podman, &ssh(), false, None).isolation,
            IsolationClass::SharedKernel
        );
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
