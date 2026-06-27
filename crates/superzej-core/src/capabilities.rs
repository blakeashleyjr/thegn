//! One declared description of what a *resolved* sandbox can do, so the UI and
//! policy engine can degrade gracefully instead of special-casing each backend
//! (grey out a snapshot affordance, show the weaker egress guarantee honestly,
//! pick the right projection lifecycle).
//!
//! This module owns **no new policy**. Every field is an *aggregation* of the
//! existing source-of-truth predicates that already live next to the thing they
//! describe:
//! - the isolation engine — [`Backend::is_oci`](crate::sandbox::Backend::is_oci),
//! - the execution placement — the [`Placement`](crate::placement::Placement) variant,
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
    /// superzej owns egress directly — the DNS filter, the VPN sidecar, and the
    /// `szproxy` chokepoint all run on a host we control.
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

/// The aggregated capability declaration for a resolved [`SandboxSpec`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
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
        Self::from_parts(spec.backend, &spec.placement, spec.vpn.is_some())
    }

    /// The pure derivation, factored out of [`SandboxSpec`] so it is trivially
    /// unit-testable without constructing the full spec struct.
    ///
    /// Note: [`ProjectionMode`] is derived from the placement here as the v1
    /// heuristic. Once the resolved `DataMode` is threaded onto the spec (the
    /// projection phase) this should consult it directly — a remote placement may
    /// be `sshfs` *or* `in_env`, which the placement alone cannot distinguish.
    pub fn from_parts(backend: Backend, placement: &Placement, has_vpn: bool) -> Self {
        let is_provider = matches!(placement, Placement::Provider(_));
        // podman can checkpoint/restore a container (CRIU) — a real snapshot.
        let podman_checkpoint = matches!(backend, Backend::Podman | Backend::PodmanRootful);
        Capabilities {
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
        let c = Capabilities::from_parts(Backend::Podman, &Placement::Local, false);
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
        let bwrap = Capabilities::from_parts(Backend::Bwrap, &Placement::Local, false);
        assert!(!bwrap.can_snapshot);
        let podman = Capabilities::from_parts(Backend::Podman, &Placement::Local, false);
        assert!(podman.can_snapshot);
    }

    #[test]
    fn host_toolchain_local_is_stats_only_unmanaged() {
        // bwrap with no OCI container and no tunnel: no egress hooks, stats only.
        let c = Capabilities::from_parts(Backend::Bwrap, &Placement::Local, false);
        assert_eq!(c.projection, ProjectionMode::Bind);
        assert_eq!(c.egress, EgressKind::Unmanaged);
        assert_eq!(c.observability, ObsLevel::StatsOnly);
    }

    #[test]
    fn host_toolchain_with_vpn_can_enforce() {
        // A tunnel gives a route to govern even without an OCI container.
        let c = Capabilities::from_parts(Backend::Bwrap, &Placement::Local, true);
        assert_eq!(c.egress, EgressKind::Enforce);
    }

    #[test]
    fn plain_none_backend_is_unmanaged_stats_only() {
        let c = Capabilities::from_parts(Backend::None, &Placement::Local, false);
        assert_eq!(c.egress, EgressKind::Unmanaged);
        assert_eq!(c.observability, ObsLevel::StatsOnly);
    }

    #[test]
    fn ssh_placement_projects_via_sshfs_and_enforces_for_oci() {
        let c = Capabilities::from_parts(Backend::Podman, &ssh(), false);
        assert_eq!(c.projection, ProjectionMode::Sshfs);
        assert_eq!(c.egress, EgressKind::Enforce);
        assert_eq!(c.observability, ObsLevel::Instrumented);
    }

    #[test]
    fn k8s_placement_is_in_env() {
        let c = Capabilities::from_parts(Backend::Podman, &k8s(), false);
        assert_eq!(c.projection, ProjectionMode::InEnv);
        assert_eq!(c.egress, EgressKind::Enforce);
    }

    #[test]
    fn provider_translates_streams_and_snapshots() {
        // Provider overrides backend: translate egress, provider-stream obs,
        // sync projection, and native snapshot/suspend/metering.
        let c = Capabilities::from_parts(Backend::Podman, &provider(), false);
        assert_eq!(c.projection, ProjectionMode::Sync);
        assert_eq!(c.egress, EgressKind::Translate);
        assert_eq!(c.observability, ObsLevel::ProviderStream);
        assert!(c.can_snapshot);
        assert!(c.can_suspend_resume);
        assert!(c.meters_cost);
    }

    #[test]
    fn enum_strings_round_trip_for_ui() {
        assert_eq!(ProjectionMode::Sync.as_str(), "sync");
        assert_eq!(EgressKind::Translate.to_string(), "translate");
        assert_eq!(ObsLevel::StatsOnly.to_string(), "stats_only");
    }
}
