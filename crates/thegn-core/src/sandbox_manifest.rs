//! The enforcement-manifest column vocabulary (resolutions B22): what a sandbox
//! backend can enforce, declared once per backend as data.
//!
//! Stub landed by WS00-P1-04: types only, no rows, no callers, and not re-exported
//! from [`crate::sandbox`]. Owner after WS00: WS02-P2-02, which fills one row per
//! built-in backend (unused columns become `NotApplicable`) and may rename a
//! column when it does. [`LifecycleCaps`] is owned by WS07-P5-01 after that (it
//! replaces WS02's `SnapshotSupport`).
//!
//! WS02-P2-02 also adds the columns that reuse types it refactors in the same
//! change: `class`, `fs: FsIsolation` (with the new `AccessControlled` variant),
//! `net: NetSupport`, `ceilings` (with disk), `secrets` and `events`.
//!
//! Host-dependent state is NOT a manifest column: the Landlock ABI, userns
//! availability, the Seatbelt self-test and Windows PSEC availability come from
//! `ConfineReport` (WS03), `SeatbeltProbe` (WS05) and `WinProbeReport` (WS06),
//! overlaid at launch into `Truth`.
//!
//! [`Verification`] here is the sandbox one; the provider seam's enum is
//! `ProviderVerification` (WS08).

use crate::egress::plan::EgressPath;

/// How a backend can control egress (B11). TOML spelling in
/// `[sandbox.backends.<n>.manifest]`: `egress = "none" | "deny-only" |
/// "proxy:<path>[,<path>]" | "native:<mode>"`. Stub (WS00-P1-04); owner
/// WS02-P2-02.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EgressSupport {
    /// No egress control at all.
    None,
    /// Can deny all network, but cannot route through a proxy.
    DenyOnly,
    /// Can route through the egress proxy on these datapaths; a path is
    /// credited only after attestation records it.
    Proxy(&'static [EgressPath]),
    /// Lowers the policy to the runtime's own rules.
    Native(NativeEgress),
}

/// The granularity of a runtime's native egress rules. Stub (WS00-P1-04); owner
/// WS02-P2-02.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeEgress {
    /// Host names resolved once, at start.
    HostnameAtStart,
    /// Address ranges only.
    Cidr,
    /// Host names matched live.
    HostnameLive,
}

/// How a backend keeps the host `$HOME` out of the sandbox. Stub (WS00-P1-04);
/// owner WS02-P2-02.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HomeSealing {
    /// A private tmpfs over `$HOME` holding only an allowlist.
    TmpfsSlot,
    /// `$HOME` is never mounted (separate root filesystem).
    NotVisible,
    /// Deny rules over `$HOME` (Seatbelt, Landlock, MXC); satisfies a sealed
    /// home (B23).
    DenyRules,
    /// The backend cannot seal `$HOME`.
    Unsupported,
}

/// Whether thegn's own state (control sockets, the DB) is visible inside.
/// Stub (WS00-P1-04); owner WS02-P2-02.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtectedState {
    /// Hidden from the sandbox.
    Hidden,
    /// Readable from the sandbox.
    Readable,
}

/// Which mount destinations a backend can remap. Stub (WS00-P1-04); owner
/// WS02-P2-02.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Remap {
    /// Any destination.
    Any,
    /// Only links under the home directory.
    HomeLinksOnly,
    /// Destinations always equal sources.
    None,
}

/// How the worktree reaches the sandbox. Stub (WS00-P1-04); owner WS02-P2-02.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Projection {
    /// A bind mount at the same path as on the host.
    BindPathPreserving,
    /// A bind mount at another path.
    Bind,
    /// A copy.
    Copy,
}

/// How the backend kills a sandbox's whole process tree. Stub (WS00-P1-04);
/// owner WS02-P2-02.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KillTree {
    /// A PID namespace: killing its init kills everything.
    PidNs,
    /// A cgroup kill.
    Cgroup,
    /// A Windows Job Object.
    Job,
    /// A process-group signal; children that leave the group survive.
    BestEffortPgid,
}

/// Whether the backend can host an interactive PTY. Stub (WS00-P1-04); owner
/// WS02-P2-02.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PtySupport {
    /// A native PTY.
    Native,
    /// Exec only (no PTY).
    ExecOnly,
}

/// What stays resident while sandboxes run. Stub (WS00-P1-04); owner
/// WS02-P2-02.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resident {
    /// Long-lived helper processes per sandbox instance.
    pub per_instance: u8,
    /// A system daemon the backend needs, if any (e.g. `"dockerd"`).
    pub daemon: Option<&'static str>,
}

/// A three-valued fact. Stub (WS00-P1-04); owner WS02-P2-02.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tri {
    /// Known to hold.
    Yes,
    /// Known not to hold.
    No,
    /// Not known.
    Unknown,
}

/// One hardening control a backend may apply. Stub (WS00-P1-04); owner
/// WS02-P2-02.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HardeningItem {
    /// Filesystem scope limited to the worktree and declared mounts.
    FsScope,
    /// `$HOME` sealed or read-only.
    Home,
    /// Read-only root filesystem.
    ReadOnlyRoot,
    /// `no_new_privs`.
    NoNewPrivs,
    /// Linux capabilities dropped.
    DropCaps,
    /// A process-count limit.
    Pids,
    /// A CPU limit.
    Cpu,
    /// A memory limit.
    Memory,
}

/// The hardening controls a backend applies, split by what they protect.
/// Stub (WS00-P1-04); owner WS02-P2-02.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HardeningSupport {
    /// Controls that protect integrity (the host's files and state).
    pub integrity: &'static [HardeningItem],
    /// Controls that protect availability (the host's resources).
    pub availability: &'static [HardeningItem],
}

/// The lifecycle verbs a backend supports, as a bitset. Stub (WS00-P1-04);
/// owner WS02-P2-02, then WS07-P5-01, which defines the verb bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LifecycleCaps {
    /// One bit per lifecycle verb; no bit is assigned yet.
    pub bits: u16,
}

/// How far a backend's manifest row is trusted (the sandbox enum; the provider
/// seam's is `ProviderVerification`). Stub (WS00-P1-04); owner WS02-P2-02.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verification {
    /// Verified by thegn's own tests.
    Verified,
    /// Not verified, with the reason.
    Unverified(&'static str),
    /// Declared by a definition and not yet attested (counts as a host
    /// process for the floor until attested).
    Declared,
    /// Attested by `thegn sandbox verify` at this Unix time (seconds).
    Attested {
        /// When the attestation ran.
        at: i64,
    },
}

/// One backend's enforcement manifest (B22 column set). Stub (WS00-P1-04);
/// owner WS02-P2-02, which adds the columns listed in the module docs and fills
/// the rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnforcementManifest {
    /// How `$HOME` is kept out.
    pub home: HomeSealing,
    /// How egress can be controlled.
    pub egress: EgressSupport,
    /// Whether thegn's own state is hidden.
    pub protected_state: ProtectedState,
    /// Which mount destinations can be remapped.
    pub remap: Remap,
    /// How the worktree reaches the sandbox.
    pub projection: Projection,
    /// Whether single files can be bind-mounted.
    pub single_file_mounts: bool,
    /// How the process tree is killed.
    pub kill_tree: KillTree,
    /// PTY support.
    pub pty: PtySupport,
    /// What stays resident.
    pub resident: Resident,
    /// Supported lifecycle verbs.
    pub lifecycle: LifecycleCaps,
    /// Whether the backend runs thegn's confinement helper inside.
    pub applies_thegn_confine: bool,
    /// Whether a nested user namespace is available inside.
    pub nested_userns: Tri,
    /// Whether a nested sandbox gets its own kernel.
    pub nested_sandbox_kernel: bool,
    /// Hardening controls applied.
    pub hardening: HardeningSupport,
    /// Free-text notes shown with the row.
    pub notes: &'static [&'static str],
    /// How far the row is trusted.
    pub verification: Verification,
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROXY_PATHS: &[EgressPath] = &[EgressPath::NetnsBridge, EgressPath::SidecarNetns];
    const INTEGRITY: &[HardeningItem] = &[
        HardeningItem::FsScope,
        HardeningItem::Home,
        HardeningItem::ReadOnlyRoot,
        HardeningItem::NoNewPrivs,
        HardeningItem::DropCaps,
    ];
    const AVAILABILITY: &[HardeningItem] = &[
        HardeningItem::Pids,
        HardeningItem::Cpu,
        HardeningItem::Memory,
    ];

    #[test]
    fn manifest_types_construct() {
        let egress = [
            EgressSupport::None,
            EgressSupport::DenyOnly,
            EgressSupport::Proxy(PROXY_PATHS),
            EgressSupport::Native(NativeEgress::HostnameAtStart),
            EgressSupport::Native(NativeEgress::Cidr),
            EgressSupport::Native(NativeEgress::HostnameLive),
        ];
        let home = [
            HomeSealing::TmpfsSlot,
            HomeSealing::NotVisible,
            HomeSealing::DenyRules,
            HomeSealing::Unsupported,
        ];
        let protected = [ProtectedState::Hidden, ProtectedState::Readable];
        let remap = [Remap::Any, Remap::HomeLinksOnly, Remap::None];
        let projection = [
            Projection::BindPathPreserving,
            Projection::Bind,
            Projection::Copy,
        ];
        let kill = [
            KillTree::PidNs,
            KillTree::Cgroup,
            KillTree::Job,
            KillTree::BestEffortPgid,
        ];
        let pty = [PtySupport::Native, PtySupport::ExecOnly];
        let tri = [Tri::Yes, Tri::No, Tri::Unknown];
        let verification = [
            Verification::Verified,
            Verification::Unverified("no runner"),
            Verification::Declared,
            Verification::Attested { at: 1_763_337_600 },
        ];

        let mut rows = Vec::new();
        for (i, egress) in egress.into_iter().enumerate() {
            rows.push(EnforcementManifest {
                home: home[i % home.len()],
                egress,
                protected_state: protected[i % protected.len()],
                remap: remap[i % remap.len()],
                projection: projection[i % projection.len()],
                single_file_mounts: i % 2 == 0,
                kill_tree: kill[i % kill.len()],
                pty: pty[i % pty.len()],
                resident: Resident {
                    per_instance: 1,
                    daemon: (i % 2 == 1).then_some("dockerd"),
                },
                lifecycle: LifecycleCaps::default(),
                applies_thegn_confine: i % 2 == 1,
                nested_userns: tri[i % tri.len()],
                nested_sandbox_kernel: false,
                hardening: HardeningSupport {
                    integrity: INTEGRITY,
                    availability: AVAILABILITY,
                },
                notes: &["stub row"],
                verification: verification[i % verification.len()],
            });
        }
        for row in &rows {
            let copy = *row;
            assert_eq!(copy, *row);
            assert!(format!("{row:?}").starts_with("EnforcementManifest {"));
        }
        assert_ne!(rows[0], rows[1]);
        assert_eq!(LifecycleCaps::default().bits, 0);
        assert_eq!(rows[2].egress, EgressSupport::Proxy(PROXY_PATHS));
        assert_eq!(rows[1].resident.daemon, Some("dockerd"));
        assert_eq!(rows[0].hardening.integrity.len(), 5);
        assert_eq!(rows[0].hardening.availability.len(), 3);
    }
}
