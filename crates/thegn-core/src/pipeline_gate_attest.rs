//! Pure policy for commit-bound pipeline gate evidence.
//!
//! A later trusted runner produces an attestation outside the worker's write
//! boundary. A worker-authored gate: line is only a claim, never an
//! attestation. This module evaluates supplied values without I/O or clock reads.

use serde::Serialize;
use std::fmt;

/// Identity of the process that produced gate evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateRunner {
    /// Gate run by the host during a fold.
    HostFoldGate,
    /// Gate run by the trusted lead runner.
    LeadRunner,
    /// Worker-authored self-report; never a trusted producer.
    Worker,
}

impl GateRunner {
    /// Stable display name for the runner identity.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::HostFoldGate => "host_fold_gate",
            Self::LeadRunner => "lead_runner",
            Self::Worker => "worker",
        }
    }

    /// Parse a stable runner identifier.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "host_fold_gate" => Some(Self::HostFoldGate),
            "lead_runner" => Some(Self::LeadRunner),
            "worker" => Some(Self::Worker),
            _ => None,
        }
    }
}

/// Result of one trusted gate execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GateOutcome {
    Passed,
    Failed,
    Error,
}

impl GateOutcome {
    /// Stable database and JSON representation.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Failed => "failed",
            Self::Error => "error",
        }
    }

    /// Parse a stable outcome identifier.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "passed" => Some(Self::Passed),
            "failed" => Some(Self::Failed),
            "error" => Some(Self::Error),
            _ => None,
        }
    }
}

/// A trusted runner's record of one gate command execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateAttestation {
    /// Dispatch row for which the command was run.
    pub dispatch_row: i64,
    /// Canonical git common directory.
    pub repo: String,
    /// Worktree path.
    pub worktree: String,
    /// Full branch ref.
    pub branch_ref: String,
    /// Commit observed before running the command.
    pub commit_oid: String,
    /// Tree observed before running the command.
    pub tree_oid: String,
    /// Gate command identity.
    pub command: String,
    /// Start time supplied by the runner, in milliseconds.
    pub started_ms: i64,
    /// End time supplied by the runner, in milliseconds.
    pub ended_ms: i64,
    /// Exit code, or None if killed or timed out.
    pub exit: Option<i32>,
    /// Durable log/evidence path.
    pub log_path: String,
    /// Runner identity.
    pub runner: GateRunner,
}

/// Current facts gathered by the caller for an attestation decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttestationContext {
    /// Dispatch row being evaluated.
    pub dispatch_row: i64,
    /// Canonical git common directory.
    pub repo: String,
    /// Worktree path.
    pub worktree: String,
    /// Full branch ref.
    pub branch_ref: String,
    /// Current HEAD commit.
    pub head_oid: String,
    /// Current HEAD tree.
    pub head_tree_oid: String,
    /// Whether the relevant worktree state is dirty.
    pub tree_dirty: bool,
    /// Caller verified that the log exists and is non-empty.
    pub log_present: bool,
}

/// A typed reason an attestation cannot satisfy the current gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttestationRefusal {
    /// Worker self-reports are not trusted evidence.
    UntrustedRunner,
    /// An OID is not 40 or 64 lowercase hexadecimal characters.
    MalformedOid { field: &'static str },
    /// The command identity is blank.
    EmptyCommand,
    /// The supplied end time precedes the start time.
    ClockInverted,
    /// The runner did not obtain an exit status.
    NoExitStatus,
    /// The command exited unsuccessfully.
    Failed { exit: i32 },
    /// The log path is empty or the caller could not verify a non-empty log.
    MissingLog,
    /// Attestation identity does not match this dispatch context.
    Replayed { field: &'static str },
    /// The attested commit differs from current HEAD.
    StaleCommit { attested: String, head: String },
    /// The attested tree differs from current HEAD's tree.
    StaleTree,
    /// The relevant worktree is dirty.
    DirtyWorktree,
}

impl fmt::Display for AttestationRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UntrustedRunner => write!(f, "gate runner is not trusted"),
            Self::MalformedOid { field } => {
                write!(f, "{field} is not a valid lowercase Git object ID")
            }
            Self::EmptyCommand => write!(f, "gate command is empty"),
            Self::ClockInverted => write!(f, "gate command end time precedes its start time"),
            Self::NoExitStatus => write!(f, "gate command has no exit status"),
            Self::Failed { exit } => write!(f, "gate command exited with status {exit}"),
            Self::MissingLog => write!(f, "gate command log is missing or empty"),
            Self::Replayed { field } => {
                write!(f, "gate attestation belongs to a different {field}")
            }
            Self::StaleCommit { attested, head } => {
                write!(
                    f,
                    "gate commit {attested} does not match current HEAD {head}"
                )
            }
            Self::StaleTree => write!(f, "gate tree does not match current HEAD tree"),
            Self::DirtyWorktree => write!(f, "worktree has changes since the gate ran"),
        }
    }
}

/// Result of evaluating one gate attestation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttestationVerdict {
    /// All attested values match the current context and the gate succeeded.
    Satisfied,
    /// The attestation fails one of the ordered checks.
    Refused(AttestationRefusal),
}

/// Decide whether trusted gate evidence still satisfies the current context.
///
/// Checks are ordered so callers receive the first refusal consistently.
pub fn attestation_satisfies(
    att: &GateAttestation,
    ctx: &AttestationContext,
) -> AttestationVerdict {
    if matches!(&att.runner, GateRunner::Worker) {
        return AttestationVerdict::Refused(AttestationRefusal::UntrustedRunner);
    }
    for (field, oid) in [
        ("commit_oid", att.commit_oid.as_str()),
        ("tree_oid", att.tree_oid.as_str()),
        ("head_oid", ctx.head_oid.as_str()),
        ("head_tree_oid", ctx.head_tree_oid.as_str()),
    ] {
        if !valid_oid(oid) {
            return AttestationVerdict::Refused(AttestationRefusal::MalformedOid { field });
        }
    }
    if att.command.trim().is_empty() {
        return AttestationVerdict::Refused(AttestationRefusal::EmptyCommand);
    }
    if att.ended_ms < att.started_ms {
        return AttestationVerdict::Refused(AttestationRefusal::ClockInverted);
    }
    let Some(exit) = att.exit else {
        return AttestationVerdict::Refused(AttestationRefusal::NoExitStatus);
    };
    if exit != 0 {
        return AttestationVerdict::Refused(AttestationRefusal::Failed { exit });
    }
    if att.log_path.trim().is_empty() || !ctx.log_present {
        return AttestationVerdict::Refused(AttestationRefusal::MissingLog);
    }
    for (field, matches) in [
        ("dispatch_row", att.dispatch_row == ctx.dispatch_row),
        ("repo", att.repo == ctx.repo),
        ("worktree", att.worktree == ctx.worktree),
        ("branch_ref", att.branch_ref == ctx.branch_ref),
    ] {
        if !matches {
            return AttestationVerdict::Refused(AttestationRefusal::Replayed { field });
        }
    }
    if att.commit_oid != ctx.head_oid {
        return AttestationVerdict::Refused(AttestationRefusal::StaleCommit {
            attested: att.commit_oid.clone(),
            head: ctx.head_oid.clone(),
        });
    }
    if att.tree_oid != ctx.head_tree_oid {
        return AttestationVerdict::Refused(AttestationRefusal::StaleTree);
    }
    if ctx.tree_dirty {
        return AttestationVerdict::Refused(AttestationRefusal::DirtyWorktree);
    }
    AttestationVerdict::Satisfied
}

fn valid_oid(oid: &str) -> bool {
    matches!(oid.len(), 40 | 64)
        && oid
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
mod tests {
    use super::{
        AttestationContext, AttestationRefusal, AttestationVerdict, GateAttestation, GateOutcome,
        GateRunner, attestation_satisfies,
    };

    #[test]
    fn gate_runner_round_trips() {
        for runner in [
            GateRunner::HostFoldGate,
            GateRunner::LeadRunner,
            GateRunner::Worker,
        ] {
            assert_eq!(GateRunner::parse(runner.as_str()), Some(runner));
        }
        assert_eq!(GateRunner::parse("unknown"), None);
    }

    #[test]
    fn gate_outcome_round_trips() {
        for outcome in [GateOutcome::Passed, GateOutcome::Failed, GateOutcome::Error] {
            assert_eq!(GateOutcome::parse(outcome.as_str()), Some(outcome));
        }
        assert_eq!(GateOutcome::parse("unknown"), None);
    }

    fn good() -> (GateAttestation, AttestationContext) {
        let att = GateAttestation {
            dispatch_row: 17,
            repo: "/repo/.git".to_string(),
            worktree: "/repo/worktree".to_string(),
            branch_ref: "refs/heads/topic".to_string(),
            commit_oid: "a".repeat(40),
            tree_oid: "b".repeat(40),
            command: "just test".to_string(),
            started_ms: 100,
            ended_ms: 200,
            exit: Some(0),
            log_path: "/logs/gate.log".to_string(),
            runner: GateRunner::HostFoldGate,
        };
        let ctx = AttestationContext {
            dispatch_row: 17,
            repo: "/repo/.git".to_string(),
            worktree: "/repo/worktree".to_string(),
            branch_ref: "refs/heads/topic".to_string(),
            head_oid: "a".repeat(40),
            head_tree_oid: "b".repeat(40),
            tree_dirty: false,
            log_present: true,
        };
        (att, ctx)
    }

    #[test]
    fn satisfied_when_every_fact_matches() {
        let (att, ctx) = good();
        assert_eq!(att.runner.as_str(), "host_fold_gate");
        assert_eq!(GateRunner::LeadRunner.as_str(), "lead_runner");
        assert_eq!(GateRunner::Worker.as_str(), "worker");
        assert_eq!(
            attestation_satisfies(&att, &ctx),
            AttestationVerdict::Satisfied
        );
    }

    #[test]
    fn each_refusal_class_in_order() {
        let mut rows = Vec::new();
        let (mut att, ctx) = good();
        att.runner = GateRunner::Worker;
        rows.push((
            "untrusted runner",
            attestation_satisfies(&att, &ctx),
            AttestationRefusal::UntrustedRunner,
        ));

        for field in ["commit_oid", "tree_oid"] {
            let (mut att, ctx) = good();
            if field == "commit_oid" {
                att.commit_oid = "A".repeat(40);
            } else {
                att.tree_oid = "g".repeat(40);
            }
            rows.push((
                field,
                attestation_satisfies(&att, &ctx),
                AttestationRefusal::MalformedOid { field },
            ));
        }
        for field in ["head_oid", "head_tree_oid"] {
            let (att, mut ctx) = good();
            if field == "head_oid" {
                ctx.head_oid = "z".repeat(40);
            } else {
                ctx.head_tree_oid = "c".repeat(39);
            }
            rows.push((
                field,
                attestation_satisfies(&att, &ctx),
                AttestationRefusal::MalformedOid { field },
            ));
        }
        let (mut att, ctx) = good();
        att.command = " \n ".to_string();
        rows.push((
            "empty command",
            attestation_satisfies(&att, &ctx),
            AttestationRefusal::EmptyCommand,
        ));
        let (mut att, ctx) = good();
        att.ended_ms = 99;
        rows.push((
            "inverted clock",
            attestation_satisfies(&att, &ctx),
            AttestationRefusal::ClockInverted,
        ));
        let (mut att, ctx) = good();
        att.exit = None;
        rows.push((
            "no exit",
            attestation_satisfies(&att, &ctx),
            AttestationRefusal::NoExitStatus,
        ));
        let (mut att, ctx) = good();
        att.exit = Some(1);
        rows.push((
            "failed exit",
            attestation_satisfies(&att, &ctx),
            AttestationRefusal::Failed { exit: 1 },
        ));
        let (mut att, ctx) = good();
        att.log_path.clear();
        rows.push((
            "empty log path",
            attestation_satisfies(&att, &ctx),
            AttestationRefusal::MissingLog,
        ));
        let (att, mut ctx) = good();
        ctx.log_present = false;
        rows.push((
            "missing log",
            attestation_satisfies(&att, &ctx),
            AttestationRefusal::MissingLog,
        ));

        for field in ["dispatch_row", "repo", "worktree", "branch_ref"] {
            let (mut att, ctx) = good();
            match field {
                "dispatch_row" => att.dispatch_row += 1,
                "repo" => att.repo.push_str("/other"),
                "worktree" => att.worktree.push_str("/other"),
                "branch_ref" => att.branch_ref.push_str("-other"),
                _ => unreachable!(),
            }
            rows.push((
                field,
                attestation_satisfies(&att, &ctx),
                AttestationRefusal::Replayed { field },
            ));
        }
        let (mut att, ctx) = good();
        att.commit_oid = "c".repeat(40);
        rows.push((
            "stale commit",
            attestation_satisfies(&att, &ctx),
            AttestationRefusal::StaleCommit {
                attested: att.commit_oid.clone(),
                head: ctx.head_oid.clone(),
            },
        ));
        let (mut att, ctx) = good();
        att.tree_oid = "c".repeat(40);
        rows.push((
            "stale tree",
            attestation_satisfies(&att, &ctx),
            AttestationRefusal::StaleTree,
        ));
        let (att, mut ctx) = good();
        ctx.tree_dirty = true;
        rows.push((
            "dirty worktree",
            attestation_satisfies(&att, &ctx),
            AttestationRefusal::DirtyWorktree,
        ));

        let actual: Vec<_> = rows
            .iter()
            .map(|(label, verdict, expected)| {
                (
                    label,
                    verdict == &AttestationVerdict::Refused(expected.clone()),
                )
            })
            .collect();
        assert!(
            actual.iter().all(|(_, matches)| *matches),
            "refusal rows: {actual:#?}"
        );

        let (mut att, mut ctx) = good();
        att.commit_oid = "a".repeat(64);
        ctx.head_oid = "a".repeat(64);
        att.tree_oid = "b".repeat(64);
        ctx.head_tree_oid = "b".repeat(64);
        assert_eq!(
            attestation_satisfies(&att, &ctx),
            AttestationVerdict::Satisfied
        );
    }

    #[test]
    fn display_names_every_refusal() {
        let refusals = [
            AttestationRefusal::UntrustedRunner,
            AttestationRefusal::MalformedOid {
                field: "commit_oid",
            },
            AttestationRefusal::EmptyCommand,
            AttestationRefusal::ClockInverted,
            AttestationRefusal::NoExitStatus,
            AttestationRefusal::Failed { exit: 1 },
            AttestationRefusal::MissingLog,
            AttestationRefusal::Replayed { field: "repo" },
            AttestationRefusal::StaleCommit {
                attested: "a".to_string(),
                head: "b".to_string(),
            },
            AttestationRefusal::StaleTree,
            AttestationRefusal::DirtyWorktree,
        ];
        let displays: Vec<_> = refusals.iter().map(ToString::to_string).collect();
        assert!(displays.iter().all(|display| !display.is_empty()));
        let mut unique = displays.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(
            unique.len(),
            displays.len(),
            "refusal displays: {displays:#?}"
        );
    }
}
