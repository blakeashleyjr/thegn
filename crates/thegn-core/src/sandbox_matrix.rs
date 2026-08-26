//! The **enforcement matrix** — one derived, exhaustive-by-construction table of
//! what every sandbox [`Backend`] actually enforces on every host [`HostOs`]:
//! filesystem isolation, network isolation, resource-ceiling strength, process
//! scoping, and the honest [`IsolationClass`]. `thegn doctor` renders the host's
//! column so a user (or a test) can see the whole picture in one place.
//!
//! This module owns **no new policy**. Every cell is an *aggregation* of the
//! predicates the resolver already uses:
//! - the honest class — [`Capabilities::from_parts_on`] (the same derivation the
//!   floor and the trust ladder read);
//! - filesystem / network / scoping — the backend's profile *family*
//!   ([`Backend::is_oci`] / [`Backend::is_host_toolchain`] / the profile table);
//! - the resource ceiling — the [`CpuCap`]-shaped
//!   rule the pane wrapper applies, overlaid with the *probed* mechanism when
//!   doctor renders the host column, so a degraded ceiling shows as soft, never
//!   hard.
//!
//! It is exhaustive **by construction**: [`row`] matches `Backend` (and, where it
//! matters, `HostOs`) with no wildcard arm, so adding a backend or an OS without
//! declaring its enforcement fails the build — the same containment-label gate
//! pattern [`IsolationClass`] itself uses. Because every cell is *derived* from
//! the enforcing predicate, the matrix can never quietly disagree with the
//! resolver: a wrong cell requires the enforcement source itself to be wrong.

use crate::capabilities::{Capabilities, EgressKind, IsolationClass};
use crate::placement::Placement;
use crate::sandbox::Backend;
use crate::sandbox_backend::{HostOs, backend_runs_on};
use crate::sandbox_cpucap::CpuCap;

/// Filesystem isolation a backend provides on the default (`hardened`) profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsIsolation {
    /// A separate root (image rootfs / guest fs / mount namespace); the host
    /// filesystem is not visible except the path-preserving worktree + git binds.
    Isolated,
    /// Only unit-level protections (`ProtectSystem`/`ProtectHome`); the same
    /// host filesystem, hardened in places — not a separate root.
    Partial,
    /// The full host filesystem, unmediated.
    None,
}

/// Network isolation a backend can provide.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetIsolation {
    /// thegn owns egress: `network=none`, the DNS filter, or a VPN sidecar can
    /// all be applied (`EgressKind::Enforce`).
    Enforceable,
    /// A network namespace is unshared **only** under `network = none`; otherwise
    /// the pane shares the host network stack.
    WhenSealed,
    /// The host network stack, always shared — no isolation available.
    HostShared,
}

/// The strength of the resource ceiling a backend gets. For the host-toolchain
/// backends on Linux the honest answer depends on the *probed*
/// [`CpuCap`], so this carries `HostProbed` and
/// doctor overlays the measured mechanism ([`EnforcementRow::ceiling_label`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CeilingStrength {
    /// A real cgroup / VM ceiling: OCI `--cpus`/`--memory`, or a systemd unit's
    /// `CPUQuota`/`MemoryMax`.
    Hard,
    /// Host-toolchain on Linux: a `systemd-run --scope` `CPUQuota` when cgroup
    /// `cpu` is delegated, degrading to soft `nice` when it is not — the probed
    /// [`CpuCap`] decides, so doctor renders the
    /// measured value.
    HostProbed,
    /// Priority only (`nice`/QoS), no hard cap — macOS host panes, where there is
    /// no cgroup equivalent.
    Soft,
    /// The mechanism exists but resource limits are not wired yet (Windows Job
    /// Objects — deferred until on-machine validation, per `add-windows-parity`).
    Deferred,
    /// No ceiling mechanism reaches a pane here.
    None,
}

/// Process-tree scoping — the lifecycle/reaping boundary. Always *present* (every
/// backend at least scopes a pgid); the variant names how.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scoping {
    /// The OCI engine owns container lifetime.
    Engine,
    /// A PID namespace + `--die-with-parent` (bwrap).
    PidNamespace,
    /// A `--collect` transient systemd unit.
    TransientUnit,
    /// A kill-on-close Windows Job Object.
    JobObject,
    /// A process group only (plain host).
    Pgid,
}

impl FsIsolation {
    pub fn as_str(self) -> &'static str {
        match self {
            FsIsolation::Isolated => "isolated",
            FsIsolation::Partial => "partial (unit-protect)",
            FsIsolation::None => "none (host fs)",
        }
    }
}

impl NetIsolation {
    pub fn as_str(self) -> &'static str {
        match self {
            NetIsolation::Enforceable => "enforceable",
            NetIsolation::WhenSealed => "only when network=none",
            NetIsolation::HostShared => "none (host net)",
        }
    }
}

impl CeilingStrength {
    /// The label to print when no probed [`CpuCap`] is available (JSON, or an OS
    /// that isn't the running host). For `HostProbed` this names the *structural*
    /// mechanism; [`EnforcementRow::ceiling_label`] refines it with the probe.
    pub fn as_str(self) -> &'static str {
        match self {
            CeilingStrength::Hard => "hard",
            CeilingStrength::HostProbed => "hard-or-soft (host cpu cap)",
            CeilingStrength::Soft => "soft (nice/QoS)",
            CeilingStrength::Deferred => "deferred (not yet wired)",
            CeilingStrength::None => "none",
        }
    }
}

impl Scoping {
    pub fn as_str(self) -> &'static str {
        match self {
            Scoping::Engine => "engine lifecycle",
            Scoping::PidNamespace => "pid namespace",
            Scoping::TransientUnit => "transient unit",
            Scoping::JobObject => "job object (kill-on-close)",
            Scoping::Pgid => "pgid",
        }
    }

    /// Process-tree scoping is present for every backend (at least a pgid). The
    /// Windows honesty scenario turns on this being `true` even for `jobobject`.
    pub fn present(self) -> bool {
        true
    }
}

/// One matrix cell: everything a `(backend, os)` pair actually enforces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnforcementRow {
    pub backend: Backend,
    pub os: HostOs,
    /// Whether this backend can run on this OS at all (a `bwrap` row on macOS is
    /// declared but unreachable). Doctor renders only the reachable rows.
    pub reachable: bool,
    /// Whether thegn's verbs for this backend were ever checked against a real
    /// install ([`Backend::verified`]); an unverified row carries its caveat and
    /// under-promises its class.
    pub verified: bool,
    pub fs: FsIsolation,
    pub net: NetIsolation,
    pub ceiling: CeilingStrength,
    pub scoping: Scoping,
    /// The honest isolation class — the resolver's own
    /// [`Capabilities::from_parts_on`] derivation (baseline, no `oci_runtime`
    /// raise; doctor reports the runtime-raised class separately).
    pub class: IsolationClass,
}

impl EnforcementRow {
    /// The ceiling cell for doctor, refined by the probed
    /// [`CpuCap`] when this is the running host and
    /// the backend's strength is [`HostProbed`](CeilingStrength::HostProbed) — so
    /// a Linux box with no cgroup cpu delegation shows *soft*, not hard. `probed`
    /// is the measured cap for `self.os`, or `None` when it can't be probed (a
    /// non-host OS column, or JSON output).
    pub fn ceiling_label(&self, probed: Option<CpuCap>) -> String {
        match (self.ceiling, probed) {
            (CeilingStrength::HostProbed, Some(cap)) => match cap {
                CpuCap::ScopeHard => "hard (systemd scope)".to_string(),
                CpuCap::NiceSoft => "soft (nice — no cgroup cpu delegation)".to_string(),
                CpuCap::None => "none (no cap mechanism)".to_string(),
            },
            (strength, _) => strength.as_str().to_string(),
        }
    }
}

/// The enforcement row for a `(backend, os)` pair — every cell derived from the
/// resolver's own predicates (see the module docs). Exhaustive over `Backend`
/// with no wildcard, so a new variant cannot ship without declaring its row.
pub fn row(backend: Backend, os: HostOs) -> EnforcementRow {
    EnforcementRow {
        backend,
        os,
        reachable: backend_runs_on(backend, os),
        verified: backend.verified(),
        fs: fs_for(backend),
        net: net_for(backend, os),
        ceiling: ceiling_for(backend, os),
        scoping: scoping_for(backend),
        // The honest class straight from the capabilities derivation — the same
        // source the floor compares over — with no `oci_runtime` raise (that is a
        // config-time modifier doctor reports on its own line).
        class: Capabilities::from_parts_on(backend, &Placement::Local, false, None, os).isolation,
    }
}

/// Filesystem isolation, from the backend's profile family. Exhaustive (no `_`).
fn fs_for(backend: Backend) -> FsIsolation {
    match backend {
        // A container image root / guest fs / mount namespace: the host fs is not
        // visible except the path-preserving worktree + git binds.
        Backend::Podman
        | Backend::PodmanRootful
        | Backend::Docker
        | Backend::Smol
        | Backend::Apple
        | Backend::Wsl
        | Backend::Bwrap => FsIsolation::Isolated,
        // An OS-enforced capability boundary (reserved) restricts fs access.
        Backend::WinAppContainer => FsIsolation::Isolated,
        // systemd unit props harden parts of the host fs but do not give a root.
        Backend::Systemd => FsIsolation::Partial,
        // A Job Object and the plain host both see the whole host filesystem.
        Backend::WinJobObject | Backend::None => FsIsolation::None,
    }
}

/// Network isolation, aggregating the egress predicate with the profile family:
/// an OCI/enforceable egress means we own the datapath; the host-toolchain
/// containers only unshare the netns under `network = none`; the host-process
/// backends share the host stack outright.
fn net_for(backend: Backend, os: HostOs) -> NetIsolation {
    // `Placement::Local`, no VPN: exactly the egress the matrix row describes.
    let egress = Capabilities::from_parts_on(backend, &Placement::Local, false, None, os).egress;
    if egress == EgressKind::Enforce {
        return NetIsolation::Enforceable;
    }
    match backend {
        // An OS capability boundary can gate network (reserved).
        Backend::WinAppContainer => NetIsolation::Enforceable,
        // Namespace tools: sealed netns only under `network = none`.
        Backend::Bwrap | Backend::Systemd => NetIsolation::WhenSealed,
        // Host-process backends: the host network, always shared.
        Backend::None | Backend::WinJobObject => NetIsolation::HostShared,
        // OCI backends always reach `Enforce` above; a fall-through here would be
        // a bug, but keep it honest rather than panicking.
        Backend::Podman
        | Backend::PodmanRootful
        | Backend::Docker
        | Backend::Smol
        | Backend::Apple
        | Backend::Wsl => NetIsolation::Enforceable,
    }
}

/// Resource-ceiling strength. OCI/systemd cap natively (hard); the host-toolchain
/// backends depend on the probed cgroup delegation on Linux (`HostProbed`) and
/// are soft on macOS; Windows Job Object limits are deferred.
fn ceiling_for(backend: Backend, os: HostOs) -> CeilingStrength {
    match backend {
        // Native OCI `--cpus`/`--memory` (inside the VM on macOS/apple, still a
        // hard cgroup relative to the container).
        Backend::Podman
        | Backend::PodmanRootful
        | Backend::Docker
        | Backend::Smol
        | Backend::Apple
        | Backend::Wsl => CeilingStrength::Hard,
        // Inline systemd unit props (`CPUQuota`/`MemoryMax`) + the shared slice.
        Backend::Systemd => CeilingStrength::Hard,
        // bwrap is Linux-only: a `systemd-run --scope` quota when cgroup cpu is
        // delegated, else soft `nice` — the probe decides, doctor renders it.
        Backend::Bwrap => CeilingStrength::HostProbed,
        // The plain host pane's cap depends on the OS: Linux joins the slice via a
        // scope wrap (probe-dependent); macOS has no cgroup, so soft only;
        // Windows/other have no cap mechanism.
        Backend::None => match os {
            HostOs::Linux => CeilingStrength::HostProbed,
            HostOs::MacOs => CeilingStrength::Soft,
            HostOs::Windows | HostOs::Other => CeilingStrength::None,
        },
        // Job Object resource limits are deferred (add-windows-parity).
        Backend::WinJobObject | Backend::WinAppContainer => CeilingStrength::Deferred,
    }
}

/// Process-tree scoping, from the profile family. Exhaustive (no `_`).
fn scoping_for(backend: Backend) -> Scoping {
    match backend {
        Backend::Podman
        | Backend::PodmanRootful
        | Backend::Docker
        | Backend::Smol
        | Backend::Apple
        | Backend::Wsl => Scoping::Engine,
        Backend::Bwrap => Scoping::PidNamespace,
        Backend::Systemd => Scoping::TransientUnit,
        Backend::WinJobObject | Backend::WinAppContainer => Scoping::JobObject,
        Backend::None => Scoping::Pgid,
    }
}

/// Every `(backend, os)` cell — for doctor's host column, the JSON projection,
/// and the exhaustiveness test.
pub fn all_rows() -> impl Iterator<Item = EnforcementRow> {
    const OSES: [HostOs; 4] = [HostOs::Linux, HostOs::MacOs, HostOs::Windows, HostOs::Other];
    Backend::ALL
        .into_iter()
        .flat_map(|b| OSES.into_iter().map(move |os| row(b, os)))
}

/// The reachable rows for one host OS, in [`Backend::ALL`] order — the column
/// `thegn doctor` renders for the running host.
pub fn column_for(os: HostOs) -> Vec<EnforcementRow> {
    Backend::ALL
        .into_iter()
        .map(|b| row(b, os))
        .filter(|r| r.reachable)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Pin each OS's honest class column from one machine (the `from_parts_on(os)`
    // idiom): these are FACTS about the classifier, not the box running the suite.

    #[test]
    fn linux_column_is_honest() {
        let c = |b| row(b, HostOs::Linux);
        // OCI + bwrap + systemd share the Linux kernel.
        for b in [
            Backend::Podman,
            Backend::Docker,
            Backend::Bwrap,
            Backend::Systemd,
        ] {
            assert_eq!(c(b).class, IsolationClass::SharedKernel, "{b:?}");
        }
        // The plain host has no boundary.
        assert_eq!(c(Backend::None).class, IsolationClass::HostProcess);
        // bwrap: mount-namespace fs, netns only when sealed, probe-dependent cap.
        let bwrap = c(Backend::Bwrap);
        assert_eq!(bwrap.fs, FsIsolation::Isolated);
        assert_eq!(bwrap.net, NetIsolation::WhenSealed);
        assert_eq!(bwrap.ceiling, CeilingStrength::HostProbed);
        assert_eq!(bwrap.scoping, Scoping::PidNamespace);
        // OCI: isolated fs, enforceable egress, hard native cap.
        let podman = c(Backend::Podman);
        assert_eq!(podman.fs, FsIsolation::Isolated);
        assert_eq!(podman.net, NetIsolation::Enforceable);
        assert_eq!(podman.ceiling, CeilingStrength::Hard);
        assert_eq!(podman.scoping, Scoping::Engine);
        // The plain host: no fs/net isolation, host-probed cap, pgid scoping.
        let none = c(Backend::None);
        assert_eq!(none.fs, FsIsolation::None);
        assert_eq!(none.net, NetIsolation::HostShared);
        assert_eq!(none.ceiling, CeilingStrength::HostProbed);
    }

    #[test]
    fn macos_column_is_honest() {
        let c = |b| row(b, HostOs::MacOs);
        // A local OCI container on macOS runs in a VM → guest-kernel (the
        // verify-sandbox-mounts fix), and its resource cap is hard (native).
        for b in [Backend::Podman, Backend::Docker] {
            assert_eq!(c(b).class, IsolationClass::GuestKernel, "{b:?}");
            assert_eq!(c(b).ceiling, CeilingStrength::Hard, "{b:?}");
        }
        // Apple's `container` is a per-container lightweight VM.
        assert_eq!(c(Backend::Apple).class, IsolationClass::GuestKernel);
        // The plain host has no cgroup equivalent → a SOFT ceiling, and doctor
        // must say so rather than implying a hard cap.
        let none = c(Backend::None);
        assert_eq!(none.ceiling, CeilingStrength::Soft);
        assert_eq!(none.class, IsolationClass::HostProcess);
        // bwrap/systemd cannot run on macOS — declared but unreachable.
        assert!(!c(Backend::Bwrap).reachable);
        assert!(!c(Backend::Systemd).reachable);
        assert!(c(Backend::Apple).reachable);
    }

    #[test]
    fn windows_column_is_honest_about_jobobject() {
        let c = |b| row(b, HostOs::Windows);
        let job = c(Backend::WinJobObject);
        // The scenario: process-tree scoping present, fs + net isolation absent,
        // the host-process class — never a container class.
        assert!(job.scoping.present());
        assert_eq!(job.scoping, Scoping::JobObject);
        assert_eq!(job.fs, FsIsolation::None);
        assert_eq!(job.net, NetIsolation::HostShared);
        assert_eq!(job.class, IsolationClass::HostProcess);
        // Job Object resource limits are deferred, not hard.
        assert_eq!(job.ceiling, CeilingStrength::Deferred);
        assert!(job.reachable);
        // OCI is declined by policy on native Windows (the Linux VM can't bind the
        // worktree at its real path) — declared but unreachable locally.
        assert!(!c(Backend::Podman).reachable);
        assert!(!c(Backend::Docker).reachable);
        // AppContainer keeps a container-class reservation.
        assert_eq!(
            c(Backend::WinAppContainer).class,
            IsolationClass::SharedKernel
        );
    }

    #[test]
    fn host_toolchain_ceiling_degrades_hard_to_soft_when_delegation_absent() {
        // The doctor overlay: HostProbed renders hard WITH cgroup cpu delegation,
        // soft WITHOUT it. This is the 1.2 scenario, as a pure test.
        let bwrap = row(Backend::Bwrap, HostOs::Linux);
        assert!(
            bwrap
                .ceiling_label(Some(CpuCap::ScopeHard))
                .contains("hard")
        );
        let soft = bwrap.ceiling_label(Some(CpuCap::NiceSoft));
        assert!(soft.contains("soft"), "{soft}");
        assert!(soft.contains("nice"), "{soft}");
        // With no probe (a non-host column), the structural label stands.
        assert!(bwrap.ceiling_label(None).contains("host cpu cap"));
        // A native-hard backend ignores the probe entirely.
        let podman = row(Backend::Podman, HostOs::Linux);
        assert_eq!(podman.ceiling_label(Some(CpuCap::NiceSoft)), "hard");
    }

    #[test]
    fn unverified_backends_under_promise() {
        // smol/wsl are not verified: the row carries the caveat flag and its class
        // must not claim a microVM it never proved. `Backend::Smol` stays
        // shared-kernel (an under-promise) until the smolvm verification lands.
        for b in [Backend::Smol, Backend::Wsl] {
            let r = row(b, HostOs::Linux);
            assert!(!r.verified, "{b:?} must carry the unverified caveat");
        }
        assert_eq!(
            row(Backend::Smol, HostOs::Linux).class,
            IsolationClass::SharedKernel,
            "smol under-promises until verified (it is a microVM once activated)"
        );
        // Every shipped backend is verified.
        for b in [
            Backend::Podman,
            Backend::Docker,
            Backend::Bwrap,
            Backend::Apple,
            Backend::None,
            Backend::WinJobObject,
        ] {
            assert!(row(b, HostOs::Linux).verified, "{b:?}");
        }
    }

    #[test]
    fn no_cell_claims_unapplied_lsm_on_the_host_backend() {
        // The `none` backend's honest class must carry no LSM confinement claim —
        // thegn applies neither Landlock nor Seatbelt.
        for os in [HostOs::Linux, HostOs::MacOs, HostOs::Windows] {
            let r = row(Backend::None, os);
            assert_eq!(r.class, IsolationClass::HostProcess);
            let note = r.class.escape_note().to_lowercase();
            assert!(!note.contains("landlock"), "{note}");
            assert!(!note.contains("seatbelt"), "{note}");
        }
    }

    #[test]
    fn every_backend_os_cell_is_declared() {
        // Exhaustiveness by construction: `row` matches Backend (and OS) with no
        // wildcard, so this merely proves the 11×4 grid is all reachable without a
        // panic, and that labels are non-empty for rendering.
        let rows: Vec<_> = all_rows().collect();
        assert_eq!(rows.len(), Backend::ALL.len() * 4);
        for r in rows {
            assert!(!r.fs.as_str().is_empty());
            assert!(!r.net.as_str().is_empty());
            assert!(!r.scoping.as_str().is_empty());
            assert!(!r.ceiling.as_str().is_empty());
            assert!(!r.class.as_str().is_empty());
        }
    }

    #[test]
    fn column_for_returns_only_reachable_backends() {
        let linux = column_for(HostOs::Linux);
        assert!(linux.iter().any(|r| r.backend == Backend::Bwrap));
        assert!(linux.iter().all(|r| r.backend != Backend::Apple));
        assert!(linux.iter().all(|r| r.reachable));
        let mac = column_for(HostOs::MacOs);
        assert!(mac.iter().any(|r| r.backend == Backend::Apple));
        assert!(mac.iter().all(|r| r.backend != Backend::Bwrap));
    }
}
