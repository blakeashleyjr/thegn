//! The launch-admission vocabulary (THE-791 alignment T3, T7): what a launch
//! asks for, what an adapter actually enforces, and the verdict.
//!
//! Stub landed by WS00-P1-13: types only, no `evaluate`, no caller. Owners after
//! WS00: WS02-P5-07 ([`EnforcedControls`] computed from the manifest, the host
//! report and attestation, and [`EffectivePolicyId`]), WS02-P5-08 (`evaluate` and the
//! host handle `sandbox_admission::admit_launch`), WS02-P5-09 (secure-policy
//! qualification and the refusal table).
//!
//! Rules this vocabulary serves (T3, T7):
//! - what an adapter enforces is computed, never claimed;
//! - an untrusted class missing a required control is refused before any
//!   process exists, and a secure refusal never falls back to the host;
//! - the effective-policy identity is sha256 over the resolved spec's policy
//!   fingerprint, the egress policy digest, the workspace generation, the
//!   backend definition digest + binary sha256 and the manifest revision; reuse
//!   of compute requires equality.
//!
//! The module name stays outside the frozen `sandbox*.rs` glob (B7).

use std::fmt;

use crate::launch_class::LaunchClass;

/// Who authored the command being launched. Stub (WS00-P1-13); semantics owned
/// by WS02-P5-08.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SourceTrust {
    /// The user at the TUI or a host terminal (may carry a recorded per-run
    /// override, T15).
    HostUser,
    /// Repo content: overlays, lifecycle scripts, agent-written code; it can
    /// only narrow a class default (B31).
    Repo,
}

/// The workspace a launch runs in, by identity, with no path. Stub
/// (WS00-P1-13); final owner WS11-P3-06a (private workspaces), read by
/// WS02-P5-07 for the [`EffectivePolicyId`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WorkspaceRef {
    /// The workspace's stable worktree key.
    pub worktree_key: String,
    /// The workspace generation (an input to [`EffectivePolicyId`]).
    pub generation: u64,
}

/// One launch asking to be admitted (T3). Stub (WS00-P1-13); owned by
/// WS02-P5-08.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmissionRequest {
    /// The launch class (T1).
    pub class: LaunchClass,
    /// Who authored the command.
    pub source_trust: SourceTrust,
    /// Digest of the resolved policy ceiling the launch may not exceed.
    pub ceiling_digest: [u8; 32],
    /// The workspace, when the launch has one.
    pub workspace: Option<WorkspaceRef>,
    /// Digest of the command (host-trust exceptions are scoped to it).
    pub command_digest: [u8; 32],
}

/// One control an adapter can enforce (T7). Stub (WS00-P1-13); the mapping from
/// manifest columns is owned by WS02-P5-07.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EnforcedControl {
    /// Filesystem scope limited to the workspace and declared mounts.
    FsScope,
    /// The host `$HOME` is hidden.
    HomeHidden,
    /// The sandbox has its own network namespace (or VM network).
    NetIsolated,
    /// Egress is enforced by a thegn datapath (`EgressKind::Enforce`).
    EgressEnforced,
    /// The workspace survives the sandbox (not ephemeral storage).
    DurableStorage,
    /// A refusal can never fall back to a host process.
    NoHostFallback,
    /// Linux capabilities are dropped.
    CapsDropped,
    /// Resource limits apply.
    Limits,
}

impl EnforcedControl {
    /// Every control, in bit order.
    pub const ALL: [EnforcedControl; 8] = [
        EnforcedControl::FsScope,
        EnforcedControl::HomeHidden,
        EnforcedControl::NetIsolated,
        EnforcedControl::EgressEnforced,
        EnforcedControl::DurableStorage,
        EnforcedControl::NoHostFallback,
        EnforcedControl::CapsDropped,
        EnforcedControl::Limits,
    ];

    const fn bit(self) -> u32 {
        match self {
            EnforcedControl::FsScope => 1 << 0,
            EnforcedControl::HomeHidden => 1 << 1,
            EnforcedControl::NetIsolated => 1 << 2,
            EnforcedControl::EgressEnforced => 1 << 3,
            EnforcedControl::DurableStorage => 1 << 4,
            EnforcedControl::NoHostFallback => 1 << 5,
            EnforcedControl::CapsDropped => 1 << 6,
            EnforcedControl::Limits => 1 << 7,
        }
    }
}

/// The controls an adapter actually enforces, with the evidence for them (T7).
/// Stub (WS00-P1-13); computed by WS02-P5-07.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EnforcedControls {
    bits: u32,
    /// Human-readable evidence lines (manifest row, host report, attestation).
    pub evidence: Vec<String>,
}

impl EnforcedControls {
    /// No control is recorded as enforced.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record `control` as enforced and return the updated set.
    #[must_use]
    pub fn with(mut self, control: EnforcedControl) -> Self {
        self.bits |= control.bit();
        self
    }

    /// Whether `control` is enforced.
    pub fn has(&self, control: EnforcedControl) -> bool {
        self.bits & control.bit() != 0
    }
}

/// The immutable effective-policy identity of an admitted launch (T7): a
/// SHA-256, displayed as 64 lowercase hex digits. Stub (WS00-P1-13); computed
/// by WS02-P5-07 and stamped as the `thegn.policy` label.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EffectivePolicyId(pub [u8; 32]);

impl fmt::Display for EffectivePolicyId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// Why an admission was refused. Stub (WS00-P1-13); owned by WS02-P5-08/P5-09.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefusalReason {
    /// The class requires a control the adapter does not enforce.
    UnsupportedControl(EnforcedControl),
    /// The adapter is not verified for a secure policy.
    Unverified,
    /// No backend on this host qualifies for the policy.
    NoQualifiedBackend,
    /// The only option left would run the launch on the host.
    HostFallbackForbidden,
}

/// The admission verdict. Stub (WS00-P1-13); produced by WS02-P5-08's
/// `evaluate`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmissionVerdict {
    /// Admit the launch.
    Admit,
    /// Refuse it, before any process exists.
    Refuse(RefusalReason),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::launch_class::UntrustedKind;

    #[test]
    fn effective_policy_id_hex_is_64_chars() {
        let id = EffectivePolicyId([0xab; 32]);
        let hex = id.to_string();
        assert_eq!(hex.len(), 64);
        assert_eq!(hex, "ab".repeat(32));
        let mut bytes = [0u8; 32];
        bytes[0] = 0x01;
        bytes[31] = 0xf0;
        let hex = EffectivePolicyId(bytes).to_string();
        assert!(hex.starts_with("01") && hex.ends_with("f0"));
        assert!(
            hex.bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        );
    }

    #[test]
    fn enforced_controls_bitset_and_verdicts_construct() {
        assert!(
            EnforcedControl::ALL
                .iter()
                .all(|c| !EnforcedControls::new().has(*c)),
            "a fresh set enforces nothing"
        );
        let mut controls = EnforcedControls::new()
            .with(EnforcedControl::FsScope)
            .with(EnforcedControl::NoHostFallback);
        controls.evidence.push("manifest: bwrap".into());
        for control in EnforcedControl::ALL {
            assert_eq!(
                controls.has(control),
                matches!(
                    control,
                    EnforcedControl::FsScope | EnforcedControl::NoHostFallback
                ),
                "{control:?}"
            );
        }
        let mut seen = 0u32;
        for control in EnforcedControl::ALL {
            assert_eq!(seen & control.bit(), 0, "{control:?} shares a bit");
            seen |= control.bit();
        }

        let request = AdmissionRequest {
            class: LaunchClass::Untrusted(UntrustedKind::MergeGate),
            source_trust: SourceTrust::Repo,
            ceiling_digest: [0; 32],
            workspace: Some(WorkspaceRef {
                worktree_key: "ws-1".into(),
                generation: 3,
            }),
            command_digest: [1; 32],
        };
        assert_eq!(request.clone(), request);
        assert_ne!(SourceTrust::HostUser, SourceTrust::Repo);
        let verdicts = [
            AdmissionVerdict::Admit,
            AdmissionVerdict::Refuse(RefusalReason::UnsupportedControl(
                EnforcedControl::EgressEnforced,
            )),
            AdmissionVerdict::Refuse(RefusalReason::Unverified),
            AdmissionVerdict::Refuse(RefusalReason::NoQualifiedBackend),
            AdmissionVerdict::Refuse(RefusalReason::HostFallbackForbidden),
        ];
        for (i, a) in verdicts.iter().enumerate() {
            for (j, b) in verdicts.iter().enumerate() {
                assert_eq!(a == b, i == j);
            }
        }
    }
}
