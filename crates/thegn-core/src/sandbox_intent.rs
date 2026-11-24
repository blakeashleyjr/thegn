//! The serialisable sandbox intent (resolutions B34): what a launch asks for,
//! expressed without host paths, so it can travel to a target (a WSL distro, a
//! remote daemon) and be resolved there against the target's own filesystem.
//!
//! Stub landed by WS00-P1-05: types and the pinned v1 wire only, no caller.
//! Owner after WS00: WS02-P5-06, which adds `resolve_intent(&SandboxIntent,
//! &Config, &HostFacts) -> Result<SandboxSpec, Refusal>`, `floor::evaluate` and
//! the hidden `thegn sandbox enter --intent-fd | --intent-b64` verb. Consumers:
//! WS06-P2-05b (WSL), WS09-P5-05 (remote daemon), WS11 (agent presets travel in
//! the intent).
//!
//! Rules:
//! - **The remote may only raise the floor.** The effective floor is the max of
//!   the local floor carried here and the target's own floor (B34).
//! - **No host paths, no secrets.** Mounts are named by role
//!   ([`IntentMount`]), never by host path; nothing here carries a credential
//!   or an executable path. Every struct is `deny_unknown_fields`.
//! - **Strings, not config enums.** Profile, floor, backend, git mode and
//!   network are canonical config strings, so the wire stays independent of
//!   config-enum churn; WS02-P5-06 validates them against the target's
//!   `from_str_validated`.
//! - **The v1 wire is pinned.** The decoder accepts only
//!   [`SANDBOX_INTENT_VERSION`] and refuses every unknown field, so a newer peer
//!   cannot be silently misread by an older one.

use serde::{Deserialize, Serialize};

fn deserialize_v1<'de, D>(deserializer: D) -> Result<u16, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;

    let version = u16::deserialize(deserializer)?;
    if version == SANDBOX_INTENT_VERSION {
        Ok(version)
    } else {
        Err(D::Error::custom(format!(
            "unsupported sandbox intent version {version}; expected {SANDBOX_INTENT_VERSION}"
        )))
    }
}

/// The wire version this build writes (`SandboxIntent::version`).
pub const SANDBOX_INTENT_VERSION: u16 = 1;

/// What a sandbox launch asks for, by name and role (B34). Stub (WS00-P1-05);
/// owner WS02-P5-06.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxIntent {
    /// The wire version ([`SANDBOX_INTENT_VERSION`]).
    #[serde(deserialize_with = "deserialize_v1")]
    pub version: u16,
    /// Canonical profile id, e.g. `"standard"`.
    pub profile: String,
    /// Canonical isolation-floor id; the target may only raise it.
    pub floor: Option<String>,
    /// Backend selector string; `None` walks the target's own chain.
    pub backend: Option<String>,
    /// The `[sandbox.git] mode`.
    pub git_mode: Option<String>,
    /// Mounts, by role (never host paths).
    pub mounts: Vec<IntentMount>,
    /// `"nat"`, `"host"`, `"none"` or `"allowlist"`.
    pub network: String,
    /// The egress preset name (e.g. `"agent@1"`), for `allowlist`.
    pub egress_preset: Option<String>,
    /// Agent groups whose endpoints the egress policy admits.
    pub agent_groups: Vec<String>,
    /// The agent preset id.
    pub agent_preset: Option<String>,
    /// Resource ceilings.
    pub limits: IntentLimits,
}

/// One mount, named by its role. Stub (WS00-P1-05); owner WS02-P5-06.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "kebab-case", deny_unknown_fields)]
pub enum IntentMount {
    /// The worktree itself.
    Worktree {
        /// Writable.
        rw: bool,
    },
    /// The target's tool caches.
    Caches,
    /// A user mount, by its configured name.
    UserMount {
        /// The configured mount name.
        name: String,
        /// Writable.
        rw: bool,
    },
}

/// Resource ceilings, as config strings. Stub (WS00-P1-05); owner WS02-P5-06.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IntentLimits {
    /// CPU ceiling in cores (`[sandbox.limits] cpu`).
    pub cpus: Option<String>,
    /// Memory ceiling (`"4g"`).
    pub memory: Option<String>,
    /// Process-count cap.
    pub pids: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOLDEN_V1: &str = include_str!("../tests/fixtures/sandbox_intent_v1.json");

    fn sample() -> SandboxIntent {
        SandboxIntent {
            version: SANDBOX_INTENT_VERSION,
            profile: "sealed".into(),
            floor: Some("shared-kernel".into()),
            backend: Some("bwrap".into()),
            git_mode: Some("read-only".into()),
            mounts: vec![
                IntentMount::Worktree { rw: true },
                IntentMount::Caches,
                IntentMount::UserMount {
                    name: "datasets".into(),
                    rw: false,
                },
            ],
            network: "allowlist".into(),
            egress_preset: Some("agent@1".into()),
            agent_groups: vec!["claude".into()],
            agent_preset: Some("claude@1".into()),
            limits: IntentLimits {
                cpus: Some("2".into()),
                memory: Some("4g".into()),
                pids: Some(512),
            },
        }
    }

    #[test]
    fn intent_v1_roundtrip() {
        let intent = sample();
        let json = serde_json::to_string(&intent).unwrap();
        let back: SandboxIntent = serde_json::from_str(&json).unwrap();
        assert_eq!(back, intent);

        let minimal = SandboxIntent {
            version: SANDBOX_INTENT_VERSION,
            profile: "standard".into(),
            floor: None,
            backend: None,
            git_mode: None,
            mounts: Vec::new(),
            network: "nat".into(),
            egress_preset: None,
            agent_groups: Vec::new(),
            agent_preset: None,
            limits: IntentLimits::default(),
        };
        let json = serde_json::to_string(&minimal).unwrap();
        assert_eq!(
            serde_json::from_str::<SandboxIntent>(&json).unwrap(),
            minimal
        );
    }

    #[test]
    fn intent_rejects_unknown_fields() {
        let mut value = serde_json::to_value(sample()).unwrap();
        value["host_path"] = serde_json::json!("/home/u");
        assert!(serde_json::from_value::<SandboxIntent>(value).is_err());

        let mut value = serde_json::to_value(sample()).unwrap();
        value["limits"]["swap"] = serde_json::json!("1g");
        assert!(serde_json::from_value::<SandboxIntent>(value).is_err());

        let mount = serde_json::json!({ "role": "worktree", "rw": true, "host": "/wt" });
        assert!(serde_json::from_value::<IntentMount>(mount).is_err());

        let unknown_role = serde_json::json!({ "role": "home" });
        assert!(serde_json::from_value::<IntentMount>(unknown_role).is_err());
    }

    #[test]
    fn intent_rejects_unknown_versions() {
        for version in [0, 2, u16::MAX] {
            let mut value: serde_json::Value = serde_json::from_str(GOLDEN_V1).unwrap();
            value["version"] = serde_json::json!(version);
            let wire = serde_json::to_string(&value).unwrap();
            assert!(
                serde_json::from_str::<SandboxIntent>(&wire).is_err(),
                "accepted unsupported version {version}"
            );
        }
    }

    #[test]
    fn intent_v1_golden() {
        let golden: serde_json::Value = serde_json::from_str(GOLDEN_V1).unwrap();
        assert_eq!(serde_json::to_value(sample()).unwrap(), golden);
        let parsed: SandboxIntent = serde_json::from_str(GOLDEN_V1).unwrap();
        assert_eq!(parsed, sample());
        assert_eq!(golden["version"], 1);
        assert_eq!(
            golden["mounts"][2],
            serde_json::json!({ "role": "user-mount", "name": "datasets", "rw": false })
        );
    }

    #[test]
    fn intent_has_no_host_paths() {
        // Roles only: nothing in a serialised intent names a host path.
        let json = serde_json::to_string(&sample()).unwrap();
        assert!(!json.contains('/'), "intent wire carries a path: {json}");
        assert!(!GOLDEN_V1.contains('/'));
    }
}
