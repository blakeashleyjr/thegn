//! The THE-791 launcher taxonomy (alignment T1): every spawn of
//! repo-influenced code has exactly one [`LaunchClass`].
//!
//! Stub landed by WS00-P1-13: types and the T2 defaults table as data, no
//! caller. Semantics owned by WS02-P5-08 (the single admission handle,
//! `sandbox_admission::admit_launch`, with `LAUNCH_ROUTES` keyed by class).
//! Consumers: WS02-P5-09..P5-12, WS08-P7-05, WS10-P3-02, WS11-P3-11..P3-16 and
//! WS11-P6-05 (which flips each class to its default, T17).
//!
//! The class says whose code runs and how much it is trusted; it is orthogonal
//! to [`LaunchKind`](crate::launch_kind::LaunchKind) (PTY vs command). The
//! user's own interactive panes keep the `standard` profile (owner decision
//! 2026-11-05), so [`LaunchClass::InteractiveOwner`] has no row in
//! [`CLASS_DEFAULTS`]. The module name stays outside the frozen `sandbox*.rs`
//! glob (B7).

use serde::{Deserialize, Serialize};

/// Who a launch belongs to and how far it is trusted (T1). Stub (WS00-P1-13);
/// semantics owned by WS02-P5-08.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LaunchClass {
    /// A pane the user opens from the TUI or attaches to: worktree shells,
    /// interactive agent panes, terminal tabs, tools started interactively.
    InteractiveOwner,
    /// Repo-influenced work nobody is watching; admitted only through the
    /// single admission handle.
    Untrusted(UntrustedKind),
    /// A declared host-trust exception, scoped to its command digest.
    HostTrust(HostTrustKind),
}

impl LaunchClass {
    /// Whether this class must satisfy a secure policy (boundary tier, never a
    /// host fallback): true exactly for [`LaunchClass::Untrusted`].
    pub const fn is_secure(self) -> bool {
        matches!(self, LaunchClass::Untrusted(_))
    }
}

/// The untrusted launchers (T1). Stub (WS00-P1-13); semantics owned by
/// WS02-P5-08.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UntrustedKind {
    /// A merge-queue / PR-queue / CI / review agent.
    QueueAgent,
    /// The autopilot.
    Autopilot,
    /// `thegn agent run` and headless `session open` (THE-2005).
    AgentRun,
    /// The merge-queue fold gate.
    MergeGate,
    /// Regeneration and bisect runs.
    Regenerate,
    /// Tests-panel runs.
    TestRun,
    /// Test discovery.
    TestDiscovery,
    /// Repo lifecycle scripts.
    RepoLifecycle,
    /// A language server.
    Lsp,
    /// Provider-native exec started by a headless launcher.
    ProviderExec,
    /// `tools.run` from automations, MCP and the control API.
    HeadlessTool,
    /// An untrusted plugin.
    Plugin,
}

impl UntrustedKind {
    /// Every untrusted kind, in a stable order.
    pub const ALL: [UntrustedKind; 12] = [
        UntrustedKind::QueueAgent,
        UntrustedKind::Autopilot,
        UntrustedKind::AgentRun,
        UntrustedKind::MergeGate,
        UntrustedKind::Regenerate,
        UntrustedKind::TestRun,
        UntrustedKind::TestDiscovery,
        UntrustedKind::RepoLifecycle,
        UntrustedKind::Lsp,
        UntrustedKind::ProviderExec,
        UntrustedKind::HeadlessTool,
        UntrustedKind::Plugin,
    ];

    /// Whether this kind runs an agent (the T2 agent kinds), as opposed to a
    /// batch command.
    pub const fn is_agent(self) -> bool {
        matches!(
            self,
            UntrustedKind::QueueAgent | UntrustedKind::Autopilot | UntrustedKind::AgentRun
        )
    }
}

/// The declared host-trust exceptions (T1). Stub (WS00-P1-13); semantics owned
/// by WS02-P5-08.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HostTrustKind {
    /// A hook from the user's own config.
    UserHook,
    /// git run by thegn on the host.
    HostGit,
    /// A configured MCP server.
    McpServer,
    /// A plugin the user marked trusted.
    TrustedPlugin,
    /// The user's editor.
    Editor,
    /// A viewer (pager, diff, file manager).
    Viewer,
}

impl HostTrustKind {
    /// Every host-trust kind, in a stable order.
    pub const ALL: [HostTrustKind; 6] = [
        HostTrustKind::UserHook,
        HostTrustKind::HostGit,
        HostTrustKind::McpServer,
        HostTrustKind::TrustedPlugin,
        HostTrustKind::Editor,
        HostTrustKind::Viewer,
    ];
}

/// The credentials an untrusted class gets by default (T2). Stub (WS00-P1-13).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialDefault {
    /// Pass through the user's interactive credentials (`InteractiveOwner`;
    /// not in [`CLASS_DEFAULTS`]).
    Passthrough,
    /// Credential-free apart from model access, which comes only from a
    /// dedicated task account or brokering (agent kinds).
    ModelAccessOnly,
    /// No credential of any kind (batch kinds).
    None,
}

/// One untrusted kind's T2 defaults, as canonical config strings. Stub
/// (WS00-P1-13); no caller reads it yet (WS02-P5-08 does).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClassDefaults {
    /// The hardening profile id.
    pub profile: &'static str,
    /// The `workspace.mode`.
    pub workspace: &'static str,
    /// The network preset name (THE-791 vocabulary, T6).
    pub network: &'static str,
    /// The credential default.
    pub credentials: CredentialDefault,
}

const AGENT_DEFAULTS: ClassDefaults = ClassDefaults {
    profile: "sealed",
    workspace: "private-copy",
    network: "agent@1",
    credentials: CredentialDefault::ModelAccessOnly,
};

const BATCH_DEFAULTS: ClassDefaults = ClassDefaults {
    profile: "sealed",
    workspace: "private-copy",
    network: "offline",
    credentials: CredentialDefault::None,
};

/// T2's defaults for every untrusted kind: `sealed`, `private-copy`, a private
/// home (implied by `sealed`), `agent@1` for agent kinds and `offline` for batch
/// kinds, credential-free. `InteractiveOwner` keeps `standard` and has no row.
/// Stub (WS00-P1-13): data only, read by WS02-P5-08.
pub static CLASS_DEFAULTS: &[(UntrustedKind, ClassDefaults)] = &[
    (UntrustedKind::QueueAgent, AGENT_DEFAULTS),
    (UntrustedKind::Autopilot, AGENT_DEFAULTS),
    (UntrustedKind::AgentRun, AGENT_DEFAULTS),
    (UntrustedKind::MergeGate, BATCH_DEFAULTS),
    (UntrustedKind::Regenerate, BATCH_DEFAULTS),
    (UntrustedKind::TestRun, BATCH_DEFAULTS),
    (UntrustedKind::TestDiscovery, BATCH_DEFAULTS),
    (UntrustedKind::RepoLifecycle, BATCH_DEFAULTS),
    (UntrustedKind::Lsp, BATCH_DEFAULTS),
    (UntrustedKind::ProviderExec, BATCH_DEFAULTS),
    (UntrustedKind::HeadlessTool, BATCH_DEFAULTS),
    (UntrustedKind::Plugin, BATCH_DEFAULTS),
];

#[cfg(test)]
mod tests {
    use super::*;

    fn every_class() -> Vec<LaunchClass> {
        let mut classes = vec![LaunchClass::InteractiveOwner];
        classes.extend(UntrustedKind::ALL.map(LaunchClass::Untrusted));
        classes.extend(HostTrustKind::ALL.map(LaunchClass::HostTrust));
        classes
    }

    #[test]
    fn launch_class_serde_roundtrip() {
        for class in every_class() {
            let json = serde_json::to_string(&class).unwrap();
            let back: LaunchClass = serde_json::from_str(&json).unwrap();
            assert_eq!(back, class, "{json}");
        }
        assert_eq!(
            serde_json::to_value(LaunchClass::InteractiveOwner).unwrap(),
            serde_json::json!("interactive-owner")
        );
        assert_eq!(
            serde_json::to_value(LaunchClass::Untrusted(UntrustedKind::MergeGate)).unwrap(),
            serde_json::json!({ "untrusted": "merge-gate" })
        );
        assert_eq!(
            serde_json::to_value(LaunchClass::HostTrust(HostTrustKind::McpServer)).unwrap(),
            serde_json::json!({ "host-trust": "mcp-server" })
        );
        assert!(serde_json::from_str::<LaunchClass>("\"untrusted\"").is_err());
    }

    #[test]
    fn untrusted_is_secure_interactive_is_not() {
        for class in every_class() {
            assert_eq!(
                class.is_secure(),
                matches!(class, LaunchClass::Untrusted(_)),
                "{class:?}"
            );
        }
        assert!(!LaunchClass::InteractiveOwner.is_secure());
        assert!(!LaunchClass::HostTrust(HostTrustKind::HostGit).is_secure());
        assert!(LaunchClass::Untrusted(UntrustedKind::Lsp).is_secure());
    }

    #[test]
    fn class_defaults_cover_every_untrusted_kind() {
        assert_eq!(CLASS_DEFAULTS.len(), UntrustedKind::ALL.len());
        assert!(
            CLASS_DEFAULTS
                .iter()
                .all(|(_, d)| d.credentials != CredentialDefault::Passthrough),
            "Passthrough belongs to InteractiveOwner, which has no row"
        );
        for kind in UntrustedKind::ALL {
            let rows: Vec<&ClassDefaults> = CLASS_DEFAULTS
                .iter()
                .filter(|(k, _)| *k == kind)
                .map(|(_, d)| d)
                .collect();
            assert_eq!(rows.len(), 1, "{kind:?} needs exactly one row");
            let defaults = rows[0];
            assert_eq!(defaults.profile, "sealed");
            assert_eq!(defaults.workspace, "private-copy");
            if kind.is_agent() {
                assert_eq!(defaults.network, "agent@1", "{kind:?}");
                assert_eq!(defaults.credentials, CredentialDefault::ModelAccessOnly);
            } else {
                assert_eq!(defaults.network, "offline", "{kind:?}");
                assert_eq!(defaults.credentials, CredentialDefault::None);
            }
        }
        let agents = UntrustedKind::ALL.iter().filter(|k| k.is_agent()).count();
        assert_eq!(agents, 3);
    }
}
