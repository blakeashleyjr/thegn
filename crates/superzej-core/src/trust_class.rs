//! The co-tenancy **trust ladder** — a total order over "how strong is the
//! boundary between two sandboxes packed onto one host", PROJECTED from the
//! honest isolation vocabulary ([`crate::capabilities::IsolationClass`]) and
//! the probed runtime ([`crate::host::HostCaps`]). Never a second ladder:
//! capabilities.rs stays the source of truth for what a backend honestly
//! provides; this module only orders it for the placement engine's pack gate.
//!
//! The independent-host asymmetry is deliberate: superzej builds a Managed
//! host's stack from a pinned image, so a probed runtime there implies the
//! enforcement posture that image carries. On a user-owned box the probe
//! proves PRESENCE (podman exists, userns on) but can never prove
//! ENFORCEMENT (that the egress/config posture actually holds), so the
//! effective class defaults ONE NOTCH DOWN — restorable only by the owner's
//! explicit `trust_egress_enforced = true` attestation, which is taken on
//! faith and never verified.

use serde::{Deserialize, Serialize};

use crate::capacity::HostOwnership;
use crate::config::SandboxProfile;
use crate::host::{HostCaps, RuntimeKind};

/// Boundary strength between co-tenants, weakest to strongest. `Ord` follows
/// declaration order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum TrustClass {
    /// No container boundary at all (plain host shell).
    #[default]
    T0HostShell,
    /// A rootful container: namespaces + cgroups on a shared kernel.
    T1Container,
    /// A rootless container: the userns layer means even a container escape
    /// lands in an unprivileged user, not root.
    T2RootlessContainer,
    /// A hardware-virtualized guest kernel (microVM / Apple container).
    T3GuestKernel,
}

impl TrustClass {
    pub fn as_str(self) -> &'static str {
        match self {
            TrustClass::T0HostShell => "host-shell",
            TrustClass::T1Container => "container",
            TrustClass::T2RootlessContainer => "rootless-container",
            TrustClass::T3GuestKernel => "guest-kernel",
        }
    }

    /// One step weaker; T0 is the fixpoint.
    pub fn one_notch_down(self) -> TrustClass {
        match self {
            TrustClass::T0HostShell | TrustClass::T1Container => TrustClass::T0HostShell,
            TrustClass::T2RootlessContainer => TrustClass::T1Container,
            TrustClass::T3GuestKernel => TrustClass::T2RootlessContainer,
        }
    }
}

impl std::fmt::Display for TrustClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Derive a host's boundary class from its probed capabilities. No runtime ⇒
/// T0 (nothing to contain a co-tenant with).
pub fn derived_class(caps: &HostCaps) -> TrustClass {
    match caps.runtime.as_ref() {
        None => TrustClass::T0HostShell,
        Some(rt) => match rt.kind {
            RuntimeKind::Podman if rt.rootless => TrustClass::T2RootlessContainer,
            RuntimeKind::Podman | RuntimeKind::Docker => TrustClass::T1Container,
            // Provider-managed runtimes boot per-sandbox microVMs (Sprites'
            // Firecracker class) — but that trust belongs to the provider,
            // rated per provider by the spillover layer, not here. A cloud
            // host that somehow enters the pack pool reads as a guest kernel.
            RuntimeKind::CloudManaged => TrustClass::T3GuestKernel,
        },
    }
}

/// The effective class the pack gate consults: derived, then the ownership
/// asymmetry — independent hosts drop one notch unless attested.
pub fn effective_class(caps: &HostCaps, ownership: HostOwnership, attested: bool) -> TrustClass {
    let derived = derived_class(caps);
    match ownership {
        HostOwnership::Managed => derived,
        HostOwnership::Independent if attested => derived,
        HostOwnership::Independent => derived.one_notch_down(),
    }
}

/// The minimum class a request's resolved hardening demands for MULTI-TENANT
/// packing. Dedicated placements have no class requirement — exclusivity is
/// the boundary.
pub fn required_class(profile: SandboxProfile) -> TrustClass {
    match profile {
        SandboxProfile::Sealed | SandboxProfile::SealedTunnel => TrustClass::T2RootlessContainer,
        SandboxProfile::Open | SandboxProfile::Hardened => TrustClass::T1Container,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(probe: &str) -> HostCaps {
        HostCaps::parse_probe(&format!("ARCH=x86_64\nOS=linux\n{probe}")).unwrap()
    }

    #[test]
    fn ladder_is_totally_ordered() {
        use TrustClass::*;
        let ladder = [T0HostShell, T1Container, T2RootlessContainer, T3GuestKernel];
        for w in ladder.windows(2) {
            assert!(w[0] < w[1], "{:?} < {:?}", w[0], w[1]);
        }
        for c in ladder {
            assert!(!c.as_str().is_empty());
            assert_eq!(c.to_string(), c.as_str());
        }
    }

    #[test]
    fn derivation_table() {
        assert_eq!(derived_class(&caps("")), TrustClass::T0HostShell);
        assert_eq!(
            derived_class(&caps("DOCKER=24.0\n")),
            TrustClass::T1Container
        );
        assert_eq!(
            derived_class(&caps("PODMAN=5.0\n")),
            TrustClass::T1Container,
            "rootful podman"
        );
        assert_eq!(
            derived_class(&caps("PODMAN=5.0\nPODMAN_ROOTLESS=1\n")),
            TrustClass::T2RootlessContainer
        );
        assert_eq!(
            derived_class(&crate::host::HostCaps::cloud_managed(
                crate::host::Arch::Amd64
            )),
            TrustClass::T3GuestKernel
        );
    }

    #[test]
    fn one_notch_down_chain_with_t0_fixpoint() {
        use TrustClass::*;
        assert_eq!(T3GuestKernel.one_notch_down(), T2RootlessContainer);
        assert_eq!(T2RootlessContainer.one_notch_down(), T1Container);
        assert_eq!(T1Container.one_notch_down(), T0HostShell);
        assert_eq!(T0HostShell.one_notch_down(), T0HostShell, "fixpoint");
    }

    #[test]
    fn effective_class_matrix() {
        let rootless = caps("PODMAN=5.0\nPODMAN_ROOTLESS=1\n");
        // Managed keeps the derived class regardless of attestation.
        assert_eq!(
            effective_class(&rootless, HostOwnership::Managed, false),
            TrustClass::T2RootlessContainer
        );
        // Independent unattested: one notch down.
        assert_eq!(
            effective_class(&rootless, HostOwnership::Independent, false),
            TrustClass::T1Container
        );
        // Attestation restores parity.
        assert_eq!(
            effective_class(&rootless, HostOwnership::Independent, true),
            TrustClass::T2RootlessContainer
        );
        // A bare independent box can't be attested into having a boundary.
        assert_eq!(
            effective_class(&caps(""), HostOwnership::Independent, true),
            TrustClass::T0HostShell
        );
    }

    #[test]
    fn required_class_per_profile() {
        assert_eq!(
            required_class(SandboxProfile::Open),
            TrustClass::T1Container
        );
        assert_eq!(
            required_class(SandboxProfile::Hardened),
            TrustClass::T1Container
        );
        assert_eq!(
            required_class(SandboxProfile::Sealed),
            TrustClass::T2RootlessContainer
        );
        assert_eq!(
            required_class(SandboxProfile::SealedTunnel),
            TrustClass::T2RootlessContainer
        );
    }

    #[test]
    fn the_asymmetry_scenario_from_the_spec() {
        // Same probe result, different effective trust: managed packs sealed,
        // unattested independent does not.
        let rootless = caps("PODMAN=5.0\nPODMAN_ROOTLESS=1\n");
        let required = required_class(SandboxProfile::Sealed);
        assert!(effective_class(&rootless, HostOwnership::Managed, false) >= required);
        assert!(effective_class(&rootless, HostOwnership::Independent, false) < required);
        assert!(effective_class(&rootless, HostOwnership::Independent, true) >= required);
    }

    #[test]
    fn serde_round_trips_kebab() {
        let j = serde_json::to_string(&TrustClass::T2RootlessContainer).unwrap();
        assert_eq!(j, "\"rootless-container\"");
        assert_eq!(
            serde_json::from_str::<TrustClass>(&j).unwrap(),
            TrustClass::T2RootlessContainer
        );
    }
}
