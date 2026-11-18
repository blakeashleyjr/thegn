//! The credential-free rule for provider kinds where no host credential may
//! enter the sandbox ([`EnvProviderKind::credential_free`]): strip the
//! credential-bearing provisioning steps from a plan, and refuse them in the
//! applier (defence in depth).
//!
//! [`EnvProviderKind::credential_free`]: crate::config::EnvProviderKind::credential_free

use crate::envplan::{EnvPlan, StepKind};

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
    use crate::envplan::{ProvisionStep, Tier};

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
