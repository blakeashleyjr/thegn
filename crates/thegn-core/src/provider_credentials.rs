//! The credential-free rule for provider kinds where no host credential may
//! enter the sandbox ([`EnvProviderKind::credential_free`]): strip the
//! credential-bearing provisioning steps from a plan, and refuse them in the
//! applier (defence in depth).
//!
//! [`EnvProviderKind::credential_free`]: crate::config::EnvProviderKind::credential_free

use crate::envplan::{EnvPlan, StepKind};

/// Host env names that carry a model credential. Fixed list; user-added
/// passthrough names are not in it.
pub const MODEL_CREDENTIAL_ENV: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "OPENAI_API_KEY",
    "CODEX_API_KEY",
    "GEMINI_API_KEY",
    "GOOGLE_API_KEY",
    "OPENROUTER_API_KEY",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelGrant {
    pub env_keys: bool,
    pub login_files: bool,
}

/// The model-credential grant for a provider env. Vercel keeps its own
/// TTY-upload path (`vercel_bridge` decides); Sprites are explicit: `upload`
/// grants both channels and `none` grants neither. Every other kind retains
/// the existing behavior for this batch.
pub fn model_grant(
    kind: crate::config::EnvProviderKind,
    mode: crate::config::ModelCredentials,
) -> ModelGrant {
    match kind {
        crate::config::EnvProviderKind::Vercel => ModelGrant {
            env_keys: false,
            login_files: false,
        },
        crate::config::EnvProviderKind::Sprites => match mode {
            crate::config::ModelCredentials::Upload => ModelGrant {
                env_keys: true,
                login_files: true,
            },
            crate::config::ModelCredentials::None
            | crate::config::ModelCredentials::Proxy
            | crate::config::ModelCredentials::Inject => ModelGrant {
                env_keys: false,
                login_files: false,
            },
        },
        _ => ModelGrant {
            env_keys: true,
            login_files: true,
        },
    }
}

/// Drop fixed model-credential names, returning the kept env and sorted names
/// only. Values never appear in the returned disclosure list.
pub fn strip_model_env(env: Vec<(String, String)>) -> (Vec<(String, String)>, Vec<String>) {
    let mut dropped = Vec::new();
    let kept = env
        .into_iter()
        .filter(|(key, _)| {
            if MODEL_CREDENTIAL_ENV.contains(&key.as_str()) {
                dropped.push(key.clone());
                false
            } else {
                true
            }
        })
        .collect();
    dropped.sort();
    dropped.dedup();
    (kept, dropped)
}

/// One-line disclosure for a Sprites env.
pub fn sprites_disclosure(mode: crate::config::ModelCredentials) -> &'static str {
    match mode {
        crate::config::ModelCredentials::None => {
            "nothing: no model API key or agent login is sent; set model_credentials = \"upload\" to send them"
        }
        crate::config::ModelCredentials::Upload => {
            "your agent login files and model API keys from env_passthrough are sent at provision and refreshed on reuse"
        }
        crate::config::ModelCredentials::Proxy => {
            "proxy model credentials are not offered for sprites"
        }
        crate::config::ModelCredentials::Inject => {
            "inject model credentials are not offered for sprites"
        }
    }
}

/// The fixed, value-free label for a credential-bearing step kind, or `None`
/// when the kind carries no host credential. The label is safe to log: it
/// never names a path, dotfile or store root.
fn credential_step_label(kind: &StepKind) -> Option<&'static str> {
    match kind {
        StepKind::AgentConfigs(_) => Some("agent-configs"),
        StepKind::Dotfiles(_) => Some("dotfiles"),
        StepKind::AtuinSync => Some("atuin-sync"),
        StepKind::HomeClosurePush(_) => Some("home-closure-push"),
        StepKind::Exec(_)
        | StepKind::Checkpoint
        | StepKind::LocalParity { .. }
        | StepKind::SnapshotRestore { .. }
        | StepKind::BundleSeed { .. } => None,
    }
}

/// Remove every credential-bearing step (agent configs, dotfiles, atuin sync,
/// home-closure push) from `plan`, returning one fixed kind label per removed
/// step, in plan order.
pub fn strip_credential_steps(plan: &mut EnvPlan) -> Vec<&'static str> {
    let mut removed = Vec::new();
    plan.steps
        .retain(|step| match credential_step_label(&step.kind) {
            Some(label) => {
                removed.push(label);
                false
            }
            None => true,
        });
    removed
}

/// Whether the applier refuses `kind` for a credential-free provider.
pub fn refuses_step(kind: &StepKind) -> bool {
    credential_step_label(kind).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{EnvProviderKind, ModelCredentials};
    use crate::envplan::{ProvisionStep, Tier};

    #[test]
    fn model_grant_is_explicit_for_sprites_and_unchanged_elsewhere() {
        for (kind, mode, expected) in [
            (
                EnvProviderKind::Sprites,
                ModelCredentials::None,
                ModelGrant {
                    env_keys: false,
                    login_files: false,
                },
            ),
            (
                EnvProviderKind::Sprites,
                ModelCredentials::Upload,
                ModelGrant {
                    env_keys: true,
                    login_files: true,
                },
            ),
            (
                EnvProviderKind::Vercel,
                ModelCredentials::Upload,
                ModelGrant {
                    env_keys: false,
                    login_files: false,
                },
            ),
            (
                EnvProviderKind::Fly,
                ModelCredentials::Upload,
                ModelGrant {
                    env_keys: true,
                    login_files: true,
                },
            ),
        ] {
            assert_eq!(model_grant(kind, mode), expected, "{kind:?} {mode}");
        }
    }

    #[test]
    fn strip_model_env_keeps_non_model_names_and_returns_names_only() {
        let (kept, dropped) = strip_model_env(vec![
            ("OPENAI_API_KEY".into(), "sentinel".into()),
            ("MY_TOKEN".into(), "user-value".into()),
            ("TERM".into(), "xterm-256color".into()),
            ("ANTHROPIC_API_KEY".into(), "another-sentinel".into()),
        ]);
        assert_eq!(
            kept,
            vec![
                ("MY_TOKEN".into(), "user-value".into()),
                ("TERM".into(), "xterm-256color".into()),
            ]
        );
        assert_eq!(
            dropped,
            vec![
                "ANTHROPIC_API_KEY".to_string(),
                "OPENAI_API_KEY".to_string()
            ]
        );
        assert!(!dropped.iter().any(|name| name.contains("sentinel")));
    }

    fn step(id: &str, kind: StepKind) -> ProvisionStep {
        ProvisionStep {
            id: id.into(),
            label: id.into(),
            kind,
        }
    }

    /// One step of every [`StepKind`]. The exhaustive match in
    /// `credential_step_label` forces a decision for any new kind.
    fn every_kind_plan() -> EnvPlan {
        EnvPlan {
            steps: vec![
                step("exec", StepKind::Exec("true".into())),
                step("dotfiles", StepKind::Dotfiles(vec![".gitconfig".into()])),
                step("agents", StepKind::AgentConfigs(vec!["claude".into()])),
                step("checkpoint", StepKind::Checkpoint),
                step(
                    "closure",
                    StepKind::HomeClosurePush(vec!["/nix/store/x".into()]),
                ),
                step("atuin", StepKind::AtuinSync),
                step(
                    "parity",
                    StepKind::LocalParity {
                        worktree: "/w".into(),
                        workdir: "/r".into(),
                    },
                ),
                step(
                    "restore",
                    StepKind::SnapshotRestore {
                        worktree: "/w".into(),
                        workdir: "/r".into(),
                        snapshot_id: "s".into(),
                    },
                ),
                step(
                    "seed",
                    StepKind::BundleSeed {
                        worktree: "/w".into(),
                        workdir: "/r".into(),
                        branch: "b".into(),
                    },
                ),
            ],
            tier: Tier::Bare,
        }
    }

    #[test]
    fn strip_removes_exactly_the_four_credential_steps() {
        let mut plan = every_kind_plan();
        let removed = strip_credential_steps(&mut plan);
        assert_eq!(
            removed,
            vec![
                "dotfiles",
                "agent-configs",
                "home-closure-push",
                "atuin-sync"
            ]
        );
        let kept: Vec<&str> = plan.steps.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(
            kept,
            vec!["exec", "checkpoint", "parity", "restore", "seed"]
        );
        assert!(plan.steps.iter().all(|s| !refuses_step(&s.kind)));
        assert!(strip_credential_steps(&mut plan).is_empty(), "idempotent");
    }

    #[test]
    fn refuse_matches_strip_for_every_kind() {
        let plan = every_kind_plan();
        let refused: Vec<&str> = plan
            .steps
            .iter()
            .filter(|s| refuses_step(&s.kind))
            .map(|s| s.id.as_str())
            .collect();
        assert_eq!(refused, vec!["dotfiles", "agents", "closure", "atuin"]);
    }

    #[test]
    fn removed_labels_never_carry_step_payloads() {
        let mut plan = every_kind_plan();
        for label in strip_credential_steps(&mut plan) {
            assert!(!label.contains('/') && !label.contains('.'), "{label}");
        }
    }
}
