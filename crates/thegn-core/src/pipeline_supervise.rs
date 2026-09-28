//! The supervisor's policy — deciding what, if anything, thegn may do about
//! one pipeline lane without anybody watching.
//!
//! # The shape of the problem
//!
//! A `[[pipeline.stages]]` chart has no engine. Every transition, every
//! compile/test/lint run and every land is dispatched by hand by a supervising
//! agent, so the pipeline stalls the moment no agent session is open — and what
//! stalls with it is mostly **not judgement**. Running a build is a rule.
//! Classifying its output is a rule. Advancing a lane whose declared
//! preconditions are all met is a rule. Only deciding whether the work is any
//! good is judgement.
//!
//! This module is the rules, written down as a pure function. It is shaped
//! deliberately like [`crate::pipeline_reap`]: facts in, verdicts out, no
//! clock beyond the one the caller passes, nothing applied. The caller — the
//! daemon task or the `supervise run` verb — is the only thing that acts, and
//! `supervise plan` prints this function's output verbatim, which is what makes
//! the supervisor's behaviour reviewable before it is trusted.
//!
//! # The line it must not cross
//!
//! thegn deliberately rejected a native drain driver ("every driver feature
//! hard-codes judgement the prompt should own"). The daemon's reaper then
//! carved the exception this module generalizes: a transition may be applied
//! when *"applying it is arithmetic on recorded facts, not a judgement about
//! whether the work was any good"*.
//!
//! So every [`SuperviseAction::Advance`] and [`SuperviseAction::Enqueue`]
//! carries the exact [`Requirement`]s that were checked and met. The judgement
//! is the operator's, expressed once in each stage's `requires` list; this
//! function only checks it off. In particular there is **no action that creates
//! an approval** — granting is an operator-scoped write on a surface the daemon
//! does not hold — and there is no action that writes a quality verdict on a
//! row.
//!
//! A stage that declares **no** requirements therefore yields an advance
//! authorized by an empty list, and that is not a hole: it is the operator
//! having said, in the config file, that this transition needs no gate. It is
//! nonetheless the most consequential thing a chart can say, so it must never
//! render as a blank — [`Authorization::is_ungated`] exists so every surface
//! spells it out rather than printing an empty set.
//!
//! # Idempotence
//!
//! [`plan`] is a pure function of the facts, so calling it twice over unchanged
//! facts yields the same actions. It is the *facts* that make it converge:
//! a validation already recorded at the lane's current tip is not re-run, and a
//! transition whose child row already exists is not re-dispatched. Both are
//! keyed on the lane's **head commit**, never on a row's status — an earlier
//! prototype keyed on `(row, status)` and re-ran a 9-minute validation because
//! the row moved from `running` to `done` between passes.

use crate::config_pipeline::{Pipeline, Requirement, Supervisor};
use crate::issue::AgentDispatchStatus;
use crate::pipeline_approval::{Approval, state_for};
use crate::pipeline_validate::ValidationClass;

/// How many times a validation may be re-run for the same tip before the
/// supervisor stops trying and asks for a person.
///
/// Only classes that are statements about the *run* rather than the tree are
/// retried at all ([`ValidationClass::is_retryable`]), so this bounds exactly
/// the case worth bounding: a transient environment failure. Two is the whole
/// budget because each attempt is a full compile, and because an environment
/// failure that survives one retry is not transient.
pub const MAX_VALIDATION_ATTEMPTS: u32 = 2;

/// One recorded validation run for a lane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationRecord {
    /// The `[[tasks]]` entry that produced it.
    pub task: String,
    /// What it established.
    pub class: ValidationClass,
    /// The lane tip it was run against. A record for a different commit says
    /// nothing about the tree as it stands now.
    pub commit_sha: String,
    /// How many times this task has been run against this commit.
    pub attempts: u32,
}

/// Everything the supervisor has established about one roster row and its lane.
///
/// The caller gathers these — from the roster, from git, from the validation
/// and approval tables — so that this module can stay pure and exhaustively
/// testable. Each field is a *recorded fact*; none is an opinion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneFacts {
    /// The roster row id.
    pub row: i64,
    /// Tracker issue id (`"<provider>:<key>"`) — the lane's identity.
    pub issue_id: String,
    /// The `[[pipeline.stages]]` name this row is.
    pub stage: String,
    /// The row's recorded status.
    pub status: AgentDispatchStatus,
    /// Whether a worker may still be running here.
    ///
    /// **Unknown must map to `true`.** A row from before the exit columns
    /// existed, or one whose daemon died before it could stamp, has no exit
    /// record — and treating "no evidence it finished" as "it finished" is how
    /// a supervisor validates, and then advances past, work that is still being
    /// written. The roster spec states the same rule for slot accounting.
    pub worker_live: bool,
    /// The lane branch's current tip. Every staleness question — is this
    /// validation current, does this approval still apply — is answered against
    /// this.
    pub head_sha: String,
    /// The lane is already an ancestor of the target branch: it landed. Nothing
    /// further is owed, and re-examining it produces noise — a prototype filed
    /// four already-landed lanes as findings needing review, because a merged
    /// lane shows no diff and therefore looked like an empty change.
    pub merged_into_target: bool,
    /// The lane is already on the merge queue awaiting a fold.
    pub enqueued: bool,
    /// The row's handoff artifact is committed in `HEAD` and unchanged there.
    pub artifact_tracked: bool,
    /// The row carries a worker report.
    pub report_present: bool,
    /// Validation runs recorded for this row, at any commit.
    pub validations: Vec<ValidationRecord>,
    /// The approval recorded for this lane and stage, if any.
    pub approval: Option<Approval>,
    /// Stage names already dispatched with this row as their parent. A stage
    /// present here has been advanced into and must not be dispatched twice.
    pub child_stages: Vec<String>,
}

/// Why a lane needs a person.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EscalationReason {
    /// A validation established a verdict about the code.
    ValidationFailed {
        /// The `[[tasks]]` entry that failed.
        task: String,
        /// What it established.
        class: ValidationClass,
    },
    /// A validation could not establish anything, and the retry budget is
    /// spent. Deliberately distinct from [`Self::ValidationFailed`]: this is a
    /// statement about the machine, and reporting it as a verdict about the
    /// branch would send an agent to fix working code.
    ValidationUnresolved {
        /// The `[[tasks]]` entry that never produced a verdict.
        task: String,
        /// The last class seen.
        class: ValidationClass,
    },
    /// Everything mechanical is done and the lane is waiting on a human
    /// decision. **This is the doorbell** — the queue entry that says a session
    /// has something to rule on.
    AwaitingApproval {
        /// The stage whose output needs reading.
        stage: String,
        /// The exact commit to review. Approving anything else will not
        /// authorize the transition.
        commit_sha: String,
    },
    /// The handoff itself is incomplete: the worker produced no committed
    /// artifact, or filed no report. The work may be real but the contract was
    /// not completed, and closing it is a person's call.
    HandoffIncomplete {
        /// The requirement that is not met.
        missing: Requirement,
    },
    /// The chart names a `next` stage that is not configured — config drifted
    /// under a running pipeline.
    StageMissing {
        /// The name that resolves to nothing.
        name: String,
    },
}

impl EscalationReason {
    /// A short token for tables, `--json` and de-duplication by the applier.
    pub fn token(&self) -> &'static str {
        match self {
            Self::ValidationFailed { .. } => "validation-failed",
            Self::ValidationUnresolved { .. } => "validation-unresolved",
            Self::AwaitingApproval { .. } => "awaiting-approval",
            Self::HandoffIncomplete { .. } => "handoff-incomplete",
            Self::StageMissing { .. } => "stage-missing",
        }
    }

    /// Whether this escalation is a request for **judgement** (as opposed to a
    /// report that something is broken). Only these should ring the doorbell
    /// for a reviewing session; the rest are findings to read when convenient.
    pub fn wants_primary(&self) -> bool {
        matches!(self, Self::AwaitingApproval { .. })
    }

    /// One operator-facing line.
    pub fn explain(&self) -> String {
        match self {
            Self::ValidationFailed { task, class } => {
                format!("{task} recorded {}", class.as_str())
            }
            Self::ValidationUnresolved { task, class } => format!(
                "{task} never produced a verdict after {MAX_VALIDATION_ATTEMPTS} attempts \
                 (last: {}) — this is a fact about the machine, not about the branch",
                class.as_str()
            ),
            Self::AwaitingApproval { stage, commit_sha } => {
                format!("stage {stage} is ready and awaits approval of {commit_sha}")
            }
            Self::HandoffIncomplete { missing } => format!(
                "the handoff is incomplete: {} is not satisfied",
                missing.as_str()
            ),
            Self::StageMissing { name } => {
                format!("`next` names {name:?}, which is not a configured stage")
            }
        }
    }
}

/// The recorded facts that permitted a mutating action.
///
/// A newtype rather than a bare `Vec<Requirement>` for one reason: the empty
/// case is meaningful and dangerous to render as nothing. A stage that declares
/// no `requires` advances with no gate at all, which may be exactly what the
/// operator intended — and is the single most important thing for them to be
/// able to *see* in a plan. A bare vector invites `join(", ")`, which prints an
/// empty string and reads as a rendering bug rather than as an ungated
/// transition.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Authorization(Vec<Requirement>);

impl Authorization {
    /// The requirements checked and met, in [`Requirement::ALL`] order.
    pub fn met(&self) -> &[Requirement] {
        &self.0
    }

    /// True when the target declared no requirements — the transition is
    /// ungated by the operator's own choice.
    pub fn is_ungated(&self) -> bool {
        self.0.is_empty()
    }

    /// One phrase for a plan line or a JSON detail. Never empty: an ungated
    /// transition says so in words.
    pub fn describe(&self) -> String {
        if self.is_ungated() {
            return "no requirements declared — this transition is ungated".to_string();
        }
        self.0
            .iter()
            .map(|r| r.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// The met requirements as their stable config spellings, for `--json`.
    pub fn tokens(&self) -> Vec<&'static str> {
        self.0.iter().map(|r| r.as_str()).collect()
    }
}

impl From<Vec<Requirement>> for Authorization {
    fn from(v: Vec<Requirement>) -> Self {
        Authorization(v)
    }
}

/// What the supervisor should do about one lane. Exactly one per lane per pass,
/// so a plan reads as one line per lane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SuperviseAction {
    /// Run these `[[tasks]]` entries against the lane and record the results.
    /// Pure observation: writes no roster status and dispatches nothing.
    Validate {
        /// The roster row.
        row: i64,
        /// The `[[tasks]]` names still owed for the lane's current tip.
        tasks: Vec<String>,
        /// The tip they will be recorded against.
        commit_sha: String,
    },
    /// Dispatch the next stage.
    ///
    /// `authorized_by` is not decoration: the variant cannot be built without
    /// it, so an advance always carries the recorded facts that permitted it,
    /// and `supervise plan` can print *why* rather than merely *what*.
    Advance {
        /// The parent roster row.
        row: i64,
        /// The stage to dispatch.
        next_stage: String,
        /// The recorded facts that permitted this. Empty means the target
        /// stage declared no requirements — see [`Authorization::is_ungated`].
        authorized_by: Authorization,
    },
    /// Hand the lane to the merge queue. The supervisor never lands anything
    /// itself — the queue owns serialization and the fold gate.
    Enqueue {
        /// The terminal roster row.
        row: i64,
        /// The approved commit, carried so the applier can re-check it against
        /// the tip it actually enqueues rather than trusting this plan.
        commit_sha: String,
        /// The recorded facts that permitted this.
        authorized_by: Authorization,
    },
    /// This lane needs a person.
    Escalate {
        /// The roster row.
        row: i64,
        /// Why.
        why: EscalationReason,
    },
    /// Nothing to do, and here is why. Emitted for every lane the supervisor
    /// looked at and left alone, because a plan that silently omits a lane is
    /// indistinguishable from one that never saw it.
    Hold {
        /// The roster row.
        row: i64,
        /// Why nothing is owed.
        why: &'static str,
    },
}

impl SuperviseAction {
    /// The roster row this action concerns.
    pub fn row(&self) -> i64 {
        match self {
            Self::Validate { row, .. }
            | Self::Advance { row, .. }
            | Self::Enqueue { row, .. }
            | Self::Escalate { row, .. }
            | Self::Hold { row, .. } => *row,
        }
    }

    /// A short token for tables and `--json`.
    pub fn token(&self) -> &'static str {
        match self {
            Self::Validate { .. } => "validate",
            Self::Advance { .. } => "advance",
            Self::Enqueue { .. } => "enqueue",
            Self::Escalate { .. } => "escalate",
            Self::Hold { .. } => "hold",
        }
    }

    /// Does applying this action change anything outside the validation record?
    /// `Validate` and `Hold` are observation; the rest move work.
    pub fn is_mutating(&self) -> bool {
        matches!(self, Self::Advance { .. } | Self::Enqueue { .. })
    }
}

/// Decide what to do about every lane.
///
/// Returns exactly one action per input lane, in input order. `now_ms` is the
/// caller's clock (approval expiry is the only time-dependent rule).
pub fn plan(
    lanes: &[LaneFacts],
    pipeline: &Pipeline,
    sup: &Supervisor,
    now_ms: i64,
) -> Vec<SuperviseAction> {
    lanes
        .iter()
        .map(|lane| plan_one(lane, pipeline, sup, now_ms))
        .collect()
}

/// The per-lane decision. The order of the tests is the contract: each one
/// establishes a precondition the next depends on.
fn plan_one(
    lane: &LaneFacts,
    pipeline: &Pipeline,
    sup: &Supervisor,
    now_ms: i64,
) -> SuperviseAction {
    let row = lane.row;

    // 1. Landed work is finished work. Checked first because a merged lane
    //    shows no diff against the target and every later test would read it as
    //    an empty, suspicious change.
    if lane.merged_into_target {
        return hold(row, "already merged into the target branch");
    }

    // 2. Never touch a lane whose worker may still be writing to it. An absent
    //    exit stamp counts as live (see `LaneFacts::worker_live`).
    if lane.worker_live {
        return hold(row, "a worker may still be running here");
    }

    // 3. Every question below — is this validation current, does this approval
    //    still apply, what exactly should be reviewed — is asked *about a
    //    tree*. Without a resolvable tip there is no tree: nothing to decide,
    //    and nothing a person could act on either.
    //
    //    Found by running the planner over a real roster. Rows from
    //    long-finished batches whose worktrees have since been removed resolve
    //    to an empty head, and without this guard every one of them asked a
    //    reviewer to "approve ''" — the same class of unactionable noise that
    //    made the shell prototype's queue unreadable.
    if lane.head_sha.trim().len() < crate::pipeline_approval::MIN_SHA_PREFIX {
        return hold(
            row,
            "the lane's head commit cannot be resolved (its worktree is gone or has no commits)",
        );
    }

    // 4. A row somebody parked or gave up on is theirs, not the supervisor's.
    match lane.status {
        AgentDispatchStatus::Failed
        | AgentDispatchStatus::Abandoned
        | AgentDispatchStatus::Merged => {
            return hold(row, "the row reached a terminal state a person recorded");
        }
        AgentDispatchStatus::WaitingHuman => {
            // Deliberately NOT a hold: a parked row is often parked awaiting
            // exactly the validation the supervisor can supply, and recording
            // it is what lets the person unpark it. Advancing is still refused
            // below, because only a `done` row has finished its stage.
        }
        _ => {}
    }

    let Some(stage) = pipeline.stage(&lane.stage) else {
        // The row names a stage the chart no longer has. Nothing can be decided
        // about it mechanically.
        return hold(row, "the row's stage is not in the configured chart");
    };

    // 5. Validation: is anything still owed for the lane's CURRENT tip?
    if sup.validate_on_exit {
        match validation_state(lane, stage.validate_tasks().as_slice()) {
            ValidationState::Owed(tasks) => {
                return SuperviseAction::Validate {
                    row,
                    tasks,
                    commit_sha: lane.head_sha.clone(),
                };
            }
            ValidationState::Failed { task, class } => {
                return SuperviseAction::Escalate {
                    row,
                    why: EscalationReason::ValidationFailed { task, class },
                };
            }
            ValidationState::Unresolved { task, class } => {
                return SuperviseAction::Escalate {
                    row,
                    why: EscalationReason::ValidationUnresolved { task, class },
                };
            }
            ValidationState::AllGreen => {}
        }
    }

    // 6. Where does this lane go next?
    match stage.next_name() {
        Some(next_name) => {
            let Some(next) = pipeline.stage(next_name) else {
                return SuperviseAction::Escalate {
                    row,
                    why: EscalationReason::StageMissing {
                        name: next_name.to_string(),
                    },
                };
            };
            if lane.child_stages.iter().any(|c| c == next_name) {
                return hold(row, "the next stage has already been dispatched");
            }
            if lane.status != AgentDispatchStatus::Done {
                return hold(row, "the stage is not closed yet, so nothing follows it");
            }
            if !sup.advance {
                return hold(row, "advancing is switched off in [pipeline.supervisor]");
            }
            match check(
                lane,
                &next.parsed_requires(),
                stage.validate_tasks().as_slice(),
                sup,
                now_ms,
            ) {
                Ok(authorized_by) => SuperviseAction::Advance {
                    row,
                    next_stage: next_name.to_string(),
                    authorized_by: authorized_by.into(),
                },
                Err(why) => SuperviseAction::Escalate { row, why },
            }
        }
        None => {
            // Terminal stage: the lane's destination is the merge queue.
            if lane.enqueued {
                return hold(row, "the lane is already on the merge queue");
            }
            if lane.status != AgentDispatchStatus::Done {
                return hold(row, "the stage is not closed yet, so nothing follows it");
            }
            if !sup.land {
                return hold(row, "landing is switched off in [pipeline.supervisor]");
            }
            match check(
                lane,
                &sup.parsed_land_requires(),
                stage.validate_tasks().as_slice(),
                sup,
                now_ms,
            ) {
                Ok(authorized_by) => SuperviseAction::Enqueue {
                    row,
                    commit_sha: lane.head_sha.clone(),
                    authorized_by: authorized_by.into(),
                },
                Err(why) => SuperviseAction::Escalate { row, why },
            }
        }
    }
}

fn hold(row: i64, why: &'static str) -> SuperviseAction {
    SuperviseAction::Hold { row, why }
}

/// Where a lane stands on the validation its stage declares.
enum ValidationState {
    /// These tasks have no current, usable result and should be run.
    Owed(Vec<String>),
    /// A task established a verdict about the code.
    Failed {
        task: String,
        class: ValidationClass,
    },
    /// A task never established anything and the retry budget is spent.
    Unresolved {
        task: String,
        class: ValidationClass,
    },
    /// Every declared task recorded green for the current tip (vacuously true
    /// for a stage that declares none).
    AllGreen,
}

/// Fold the lane's validation records against what its stage declares.
///
/// Only records at the lane's **current tip** count. A green result for an
/// earlier commit is a true statement about a tree that no longer exists, and
/// treating it as current is exactly how unvalidated code advances.
fn validation_state(lane: &LaneFacts, declared: &[&str]) -> ValidationState {
    let mut owed: Vec<String> = Vec::new();
    for task in declared {
        let current = lane
            .validations
            .iter()
            .find(|v| v.task == *task && commit_matches(&v.commit_sha, &lane.head_sha));
        match current {
            None => owed.push((*task).to_string()),
            Some(v) if v.class.is_green() => {}
            Some(v) if v.class.blames_code() => {
                return ValidationState::Failed {
                    task: v.task.clone(),
                    class: v.class,
                };
            }
            // Retryable: a statement about the run, not the tree. Try again
            // until the budget is spent, then ask a person — and say plainly
            // that it is the machine, not the branch.
            Some(v) if v.attempts < MAX_VALIDATION_ATTEMPTS => owed.push(v.task.clone()),
            Some(v) => {
                return ValidationState::Unresolved {
                    task: v.task.clone(),
                    class: v.class,
                };
            }
        }
    }
    if owed.is_empty() {
        ValidationState::AllGreen
    } else {
        ValidationState::Owed(owed)
    }
}

/// Check every requirement against the lane, returning the met set or the first
/// unmet one. Requirements are checked in [`Requirement::ALL`] order so the
/// reported failure is stable rather than dependent on config ordering.
fn check(
    lane: &LaneFacts,
    required: &[Requirement],
    declared: &[&str],
    sup: &Supervisor,
    now_ms: i64,
) -> Result<Vec<Requirement>, EscalationReason> {
    let mut met = Vec::new();
    for req in Requirement::ALL {
        if !required.contains(&req) {
            continue;
        }
        let ok = match req {
            Requirement::ParentArtifact => lane.artifact_tracked,
            Requirement::ParentReport => lane.report_present,
            // EVERY declared task, not any of them, and only `green`.
            //
            // Two traps in one line. `any` would let a stage that runs clippy
            // and nextest advance on clippy alone — reachable whenever
            // `validate_on_exit` is off, because then this check is the only
            // guard. And "not a failure" is not "passed": an environment error
            // establishes nothing, so a lane whose tests never ran must not
            // advance as though they had. Delegating to `validation_state`
            // keeps both rules defined in exactly one place.
            Requirement::ValidationGreen => {
                matches!(validation_state(lane, declared), ValidationState::AllGreen)
            }
            Requirement::Approval => state_for(
                lane.approval.as_ref(),
                &lane.head_sha,
                now_ms,
                sup.approval_ttl_secs,
            )
            .is_live(),
        };
        if !ok {
            return Err(unmet(lane, req));
        }
        met.push(req);
    }
    Ok(met)
}

/// Turn an unmet requirement into the escalation that best explains it.
///
/// A missing approval is categorically different from a missing artifact: the
/// first is the pipeline working as designed and waiting for a person, the
/// second is a broken handoff. Only the first rings the doorbell.
fn unmet(lane: &LaneFacts, req: Requirement) -> EscalationReason {
    match req {
        Requirement::Approval => EscalationReason::AwaitingApproval {
            stage: lane.stage.clone(),
            commit_sha: lane.head_sha.clone(),
        },
        other => EscalationReason::HandoffIncomplete { missing: other },
    }
}

/// Do two commit strings name the same commit, allowing either to be an
/// abbreviation? Same rule the approval module applies, for the same reason:
/// a short sha from `rev-parse --short` must match a full one.
///
/// An empty string on either side matches nothing — an unrecorded commit must
/// never read as "matches the current tip", which would make every stale
/// validation look current.
fn commit_matches(a: &str, b: &str) -> bool {
    let (a, b) = (a.trim(), b.trim());
    if a.len() < crate::pipeline_approval::MIN_SHA_PREFIX
        || b.len() < crate::pipeline_approval::MIN_SHA_PREFIX
    {
        return false;
    }
    let (short, long) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    long.chars()
        .zip(short.chars())
        .all(|(l, s)| l.eq_ignore_ascii_case(&s))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_pipeline::PipelineStage;

    const NOW: i64 = 1_700_000_000_000;
    const HEAD: &str = "4babb0901f2c3d4e5f60718293a4b5c6d7e8f901";
    const OLD: &str = "1111111111111111111111111111111111111111";

    fn stage(name: &str, next: Option<&str>) -> PipelineStage {
        PipelineStage {
            name: name.into(),
            agent: "codex".into(),
            prompt: "row {row}: run thegn dispatch report".into(),
            next: next.map(str::to_string),
            ..Default::default()
        }
    }

    /// A two-stage chart: `code` validates and hands to `review`, which
    /// requires an approval of `code`'s output. `review` is terminal.
    fn chart() -> Pipeline {
        let mut code = stage("code", Some("review"));
        code.validate = vec!["nextest".into()];
        let mut review = stage("review", None);
        review.requires = vec!["parent_artifact".into(), "approval".into()];
        Pipeline {
            stages: vec![code, review],
            ..Default::default()
        }
    }

    fn sup() -> Supervisor {
        Supervisor {
            enabled: true,
            ..Default::default()
        }
    }

    fn lane(stage: &str) -> LaneFacts {
        LaneFacts {
            row: 677,
            issue_id: "linear:THE-407".into(),
            stage: stage.into(),
            status: AgentDispatchStatus::Done,
            worker_live: false,
            head_sha: HEAD.into(),
            merged_into_target: false,
            enqueued: false,
            artifact_tracked: true,
            report_present: true,
            validations: vec![],
            approval: None,
            child_stages: vec![],
        }
    }

    fn green(task: &str, sha: &str) -> ValidationRecord {
        ValidationRecord {
            task: task.into(),
            class: ValidationClass::Green,
            commit_sha: sha.into(),
            attempts: 1,
        }
    }

    fn approval(sha: &str) -> Approval {
        Approval {
            issue_id: "linear:THE-407".into(),
            stage: "code".into(),
            commit_sha: sha.into(),
            approver: "blake".into(),
            note: None,
            granted_at_ms: NOW - 1_000,
            expires_at_ms: None,
            revoked_at_ms: None,
        }
    }

    fn one(lane: LaneFacts) -> SuperviseAction {
        plan(&[lane], &chart(), &sup(), NOW)
            .pop()
            .expect("one action")
    }

    // --- the three exclusions a bash prototype learned the hard way ----------

    #[test]
    fn a_landed_lane_is_left_alone() {
        // A merged lane shows no diff against the target, so every later test
        // reads it as an empty change. A prototype filed four already-landed
        // lanes as findings needing review; this is the guard.
        let mut l = lane("code");
        l.merged_into_target = true;
        l.validations.clear();
        let action = one(l);
        assert_eq!(action.token(), "hold");
        assert!(!action.is_mutating());
    }

    #[test]
    fn a_live_worker_is_never_disturbed() {
        let mut l = lane("code");
        l.worker_live = true;
        assert_eq!(one(l).token(), "hold");
    }

    #[test]
    fn a_row_with_no_exit_stamp_counts_as_live() {
        // Documented contract of `LaneFacts::worker_live`: the caller maps
        // "unknown" onto `true`, because validating work that is still being
        // written is worse than validating it late.
        let mut l = lane("code");
        l.worker_live = true; // what the caller writes when there is no stamp
        l.status = AgentDispatchStatus::Running;
        assert_eq!(one(l).token(), "hold");
    }

    #[test]
    fn a_lane_whose_head_cannot_be_resolved_is_held_not_escalated() {
        // Rows from finished batches whose worktrees were since removed. Every
        // later rule is a question about a tree; with no tree there is nothing
        // to decide and nothing to ask a person for. Without this guard the
        // planner asked reviewers to "approve ''" for sixty-odd dead rows.
        for head in ["", "   ", "4bab"] {
            let mut l = lane("code");
            l.head_sha = head.into();
            let action = one(l);
            assert_eq!(action.token(), "hold", "head {head:?} produced {action:?}");
            assert!(!action.is_mutating());
        }
    }

    #[test]
    fn a_validation_is_not_repeated_for_the_same_tip() {
        // The keying bug: an earlier prototype keyed on (row, status) and
        // re-ran a nine-minute validation when the row moved running -> done.
        let mut l = lane("code");
        l.validations = vec![green("nextest", HEAD)];
        l.child_stages = vec!["review".into()];
        assert_eq!(
            one(l).token(),
            "hold",
            "validation was re-run for an unchanged tip"
        );
    }

    #[test]
    fn a_validation_from_an_earlier_commit_does_not_count() {
        let mut l = lane("code");
        l.validations = vec![green("nextest", OLD)];
        match one(l) {
            SuperviseAction::Validate {
                tasks, commit_sha, ..
            } => {
                assert_eq!(tasks, ["nextest"]);
                assert_eq!(commit_sha, HEAD);
            }
            other => panic!("stale validation was treated as current: {other:?}"),
        }
    }

    // --- validation ----------------------------------------------------------

    #[test]
    fn an_unvalidated_lane_is_validated_first() {
        match one(lane("code")) {
            SuperviseAction::Validate { row, tasks, .. } => {
                assert_eq!(row, 677);
                assert_eq!(tasks, ["nextest"]);
            }
            other => panic!("expected Validate, got {other:?}"),
        }
    }

    #[test]
    fn a_code_verdict_escalates_as_a_finding() {
        let mut l = lane("code");
        l.validations = vec![ValidationRecord {
            task: "nextest".into(),
            class: ValidationClass::TestFailure,
            commit_sha: HEAD.into(),
            attempts: 1,
        }];
        match one(l) {
            SuperviseAction::Escalate { why, .. } => {
                assert_eq!(why.token(), "validation-failed");
                assert!(!why.wants_primary(), "a finding is not a judgement request");
            }
            other => panic!("expected Escalate, got {other:?}"),
        }
    }

    #[test]
    fn an_environment_error_is_retried_then_reported_as_the_machine() {
        // The lesson from a gate that went red purely from CPU contention: an
        // environment failure must never be reported as a verdict about the
        // branch, and must be retried before it is reported at all.
        let mut l = lane("code");
        l.validations = vec![ValidationRecord {
            task: "nextest".into(),
            class: ValidationClass::EnvironmentError,
            commit_sha: HEAD.into(),
            attempts: 1,
        }];
        assert_eq!(
            one(l.clone()).token(),
            "validate",
            "a transient was not retried"
        );

        l.validations[0].attempts = MAX_VALIDATION_ATTEMPTS;
        match one(l) {
            SuperviseAction::Escalate { why, .. } => {
                assert_eq!(why.token(), "validation-unresolved");
                assert!(
                    why.explain().contains("not about the branch"),
                    "the escalation must not blame the code: {}",
                    why.explain()
                );
            }
            other => panic!("expected Escalate, got {other:?}"),
        }
    }

    #[test]
    fn a_stage_that_declares_no_validation_is_vacuously_green() {
        let mut p = chart();
        p.stages[0].validate.clear();
        let mut l = lane("code");
        l.approval = Some(approval(HEAD));
        let action = plan(&[l], &p, &sup(), NOW).pop().unwrap();
        assert_eq!(action.token(), "advance");
    }

    // --- the review gate -----------------------------------------------------

    #[test]
    fn a_green_lane_without_approval_asks_for_a_primary() {
        let mut l = lane("code");
        l.validations = vec![green("nextest", HEAD)];
        match one(l) {
            SuperviseAction::Escalate { why, .. } => {
                assert_eq!(why.token(), "awaiting-approval");
                assert!(why.wants_primary(), "this is the doorbell and must ring");
                match why {
                    EscalationReason::AwaitingApproval { commit_sha, .. } => {
                        assert_eq!(commit_sha, HEAD, "must name the exact commit to review");
                    }
                    _ => unreachable!(),
                }
            }
            other => panic!("expected Escalate, got {other:?}"),
        }
    }

    #[test]
    fn a_green_approved_lane_advances_and_says_why() {
        let mut l = lane("code");
        l.validations = vec![green("nextest", HEAD)];
        l.approval = Some(approval(HEAD));
        match one(l) {
            SuperviseAction::Advance {
                next_stage,
                authorized_by,
                ..
            } => {
                assert_eq!(next_stage, "review");
                assert_eq!(
                    authorized_by.met(),
                    [Requirement::ParentArtifact, Requirement::Approval]
                );
                assert!(!authorized_by.is_ungated());
            }
            other => panic!("expected Advance, got {other:?}"),
        }
    }

    #[test]
    fn an_approval_of_an_earlier_commit_does_not_advance_the_lane() {
        // THE test of the programme: approve, then one more commit lands on the
        // lane. The supervisor must stop.
        let mut l = lane("code");
        l.validations = vec![green("nextest", HEAD)];
        l.approval = Some(approval(OLD));
        let action = one(l);
        assert!(
            !action.is_mutating(),
            "a superseded approval advanced the lane: {action:?}"
        );
        assert_eq!(action.token(), "escalate");
    }

    #[test]
    fn a_revoked_approval_does_not_advance_the_lane() {
        let mut l = lane("code");
        l.validations = vec![green("nextest", HEAD)];
        let mut a = approval(HEAD);
        a.revoked_at_ms = Some(NOW - 1);
        l.approval = Some(a);
        assert!(!one(l).is_mutating());
    }

    #[test]
    fn a_lapsed_approval_does_not_advance_the_lane() {
        let mut l = lane("code");
        l.validations = vec![green("nextest", HEAD)];
        let mut a = approval(HEAD);
        a.granted_at_ms = NOW - 10_000_000;
        l.approval = Some(a);
        let mut s = sup();
        s.approval_ttl_secs = 60;
        let action = plan(&[l], &chart(), &s, NOW).pop().unwrap();
        assert!(!action.is_mutating());
    }

    #[test]
    fn validation_green_needs_every_declared_task_not_merely_one() {
        // Reachable whenever `validate_on_exit` is off, because then the
        // requirement check is the only guard. A stage running clippy AND
        // nextest must not advance on clippy alone.
        let mut p = chart();
        p.stages[0].validate = vec!["clippy".into(), "nextest".into()];
        p.stages[1].requires = vec!["validation:green".into()];
        let mut s = sup();
        s.validate_on_exit = false;

        let mut l = lane("code");
        l.validations = vec![green("clippy", HEAD)];
        let action = plan(&[l.clone()], &p, &s, NOW).pop().unwrap();
        assert!(
            !action.is_mutating(),
            "one green task of two authorized a transition: {action:?}"
        );

        l.validations.push(green("nextest", HEAD));
        assert_eq!(plan(&[l], &p, &s, NOW).pop().unwrap().token(), "advance");
    }

    #[test]
    fn an_environment_error_never_satisfies_validation_green() {
        // "not a failure" is not "passed".
        let mut p = chart();
        p.stages[1].requires = vec!["validation:green".into()];
        let mut l = lane("code");
        l.validations = vec![ValidationRecord {
            task: "nextest".into(),
            class: ValidationClass::EnvironmentError,
            commit_sha: HEAD.into(),
            attempts: MAX_VALIDATION_ATTEMPTS,
        }];
        let action = plan(&[l], &p, &sup(), NOW).pop().unwrap();
        assert!(!action.is_mutating());
    }

    #[test]
    fn an_incomplete_handoff_is_reported_as_broken_not_as_awaiting_a_person() {
        let mut l = lane("code");
        l.validations = vec![green("nextest", HEAD)];
        l.artifact_tracked = false;
        l.approval = Some(approval(HEAD));
        match one(l) {
            SuperviseAction::Escalate { why, .. } => {
                assert_eq!(why.token(), "handoff-incomplete");
                assert!(!why.wants_primary());
            }
            other => panic!("expected Escalate, got {other:?}"),
        }
    }

    // --- transitions ---------------------------------------------------------

    #[test]
    fn a_stage_is_never_dispatched_twice() {
        let mut l = lane("code");
        l.validations = vec![green("nextest", HEAD)];
        l.approval = Some(approval(HEAD));
        l.child_stages = vec!["review".into()];
        assert_eq!(one(l).token(), "hold");
    }

    #[test]
    fn an_unclosed_stage_does_not_advance() {
        let mut l = lane("code");
        l.status = AgentDispatchStatus::WaitingHuman;
        l.validations = vec![green("nextest", HEAD)];
        l.approval = Some(approval(HEAD));
        let action = one(l);
        assert!(!action.is_mutating(), "a parked row advanced: {action:?}");
    }

    #[test]
    fn a_parked_row_is_still_validated_because_that_is_what_unparks_it() {
        let mut l = lane("code");
        l.status = AgentDispatchStatus::WaitingHuman;
        assert_eq!(one(l).token(), "validate");
    }

    #[test]
    fn a_terminal_row_a_person_closed_is_left_alone() {
        for status in [
            AgentDispatchStatus::Failed,
            AgentDispatchStatus::Abandoned,
            AgentDispatchStatus::Merged,
        ] {
            let mut l = lane("code");
            l.status = status;
            assert_eq!(one(l).token(), "hold", "{status:?} was not left alone");
        }
    }

    #[test]
    fn a_next_that_names_nothing_escalates_rather_than_guessing() {
        let mut p = chart();
        p.stages[0].next = Some("adversarial".into());
        let mut l = lane("code");
        l.validations = vec![green("nextest", HEAD)];
        match plan(&[l], &p, &sup(), NOW).pop().unwrap() {
            SuperviseAction::Escalate { why, .. } => assert_eq!(why.token(), "stage-missing"),
            other => panic!("expected Escalate, got {other:?}"),
        }
    }

    #[test]
    fn a_row_whose_stage_left_the_chart_is_held_not_guessed_at() {
        let mut l = lane("investigate"); // not in `chart()`
        l.validations.clear();
        assert_eq!(one(l).token(), "hold");
    }

    // --- landing -------------------------------------------------------------

    #[test]
    fn a_terminal_stage_enqueues_rather_than_landing_itself() {
        let mut l = lane("review");
        l.approval = Some(approval(HEAD));
        match one(l) {
            SuperviseAction::Enqueue {
                commit_sha,
                authorized_by,
                ..
            } => {
                assert_eq!(commit_sha, HEAD);
                assert!(authorized_by.met().contains(&Requirement::Approval));
            }
            other => panic!("expected Enqueue, got {other:?}"),
        }
    }

    #[test]
    fn a_terminal_stage_without_approval_does_not_enqueue() {
        let action = one(lane("review"));
        assert!(!action.is_mutating());
        assert_eq!(action.token(), "escalate");
    }

    #[test]
    fn an_already_enqueued_lane_is_not_enqueued_twice() {
        let mut l = lane("review");
        l.enqueued = true;
        l.approval = Some(approval(HEAD));
        assert_eq!(one(l).token(), "hold");
    }

    // --- the switches --------------------------------------------------------

    #[test]
    fn switching_advance_off_stops_transitions_but_not_observation() {
        let mut s = sup();
        s.advance = false;
        let mut l = lane("code");
        l.approval = Some(approval(HEAD));
        // Validation still runs …
        assert_eq!(
            plan(&[l.clone()], &chart(), &s, NOW).pop().unwrap().token(),
            "validate"
        );
        // … and once green, nothing moves.
        l.validations = vec![green("nextest", HEAD)];
        let action = plan(&[l], &chart(), &s, NOW).pop().unwrap();
        assert_eq!(action.token(), "hold");
        assert!(!action.is_mutating());
    }

    #[test]
    fn switching_land_off_stops_enqueueing() {
        let mut s = sup();
        s.land = false;
        let mut l = lane("review");
        l.approval = Some(approval(HEAD));
        assert!(!plan(&[l], &chart(), &s, NOW).pop().unwrap().is_mutating());
    }

    #[test]
    fn switching_validation_off_skips_straight_to_the_gate() {
        let mut s = sup();
        s.validate_on_exit = false;
        let mut l = lane("code");
        l.approval = Some(approval(HEAD));
        // No validation record at all, yet the requirement set here does not
        // include `validation:green`, so the transition is authorized.
        assert_eq!(
            plan(&[l], &chart(), &s, NOW).pop().unwrap().token(),
            "advance"
        );
    }

    // --- shape ---------------------------------------------------------------

    #[test]
    fn every_lane_gets_exactly_one_action_in_input_order() {
        // A plan that silently omits a lane is indistinguishable from one that
        // never saw it.
        let lanes = vec![lane("code"), lane("review"), lane("code")];
        let rows: Vec<i64> = lanes.iter().map(|l| l.row).collect();
        let actions = plan(&lanes, &chart(), &sup(), NOW);
        assert_eq!(actions.len(), lanes.len());
        assert_eq!(
            actions.iter().map(SuperviseAction::row).collect::<Vec<_>>(),
            rows
        );
    }

    #[test]
    fn planning_is_idempotent_over_unchanged_facts() {
        let lanes = vec![lane("code"), lane("review")];
        let a = plan(&lanes, &chart(), &sup(), NOW);
        let b = plan(&lanes, &chart(), &sup(), NOW);
        assert_eq!(a, b);
    }

    #[test]
    fn an_ungated_transition_says_so_rather_than_rendering_as_nothing() {
        // Found by running the planner against a real chart: a stage that
        // declares no `requires` advances authorized by an empty set, and a
        // bare `join(", ")` printed `authorized by: ` — which reads as a
        // rendering bug rather than as what it is, the most consequential thing
        // a chart can say. The empty case must be spelled out.
        let mut p = chart();
        p.stages[1].requires.clear();
        let mut l = lane("code");
        l.validations = vec![green("nextest", HEAD)];
        match plan(&[l], &p, &sup(), NOW).pop().unwrap() {
            SuperviseAction::Advance { authorized_by, .. } => {
                assert!(authorized_by.is_ungated());
                assert!(
                    authorized_by.describe().contains("ungated"),
                    "an ungated transition rendered as {:?}",
                    authorized_by.describe()
                );
                assert!(!authorized_by.describe().trim().is_empty());
            }
            other => panic!("expected Advance, got {other:?}"),
        }
    }

    #[test]
    fn a_mutating_action_carries_exactly_the_requirements_its_target_declared() {
        // The real invariant: `authorized_by` is neither more nor less than the
        // set the operator wrote down and the planner then verified. Swept over
        // a matrix so a future variant cannot slip through.
        let mut cases = Vec::new();
        for approved in [None, Some(approval(HEAD)), Some(approval(OLD))] {
            for tracked in [true, false] {
                for validated in [true, false] {
                    let mut l = lane("code");
                    l.approval = approved.clone();
                    l.artifact_tracked = tracked;
                    if validated {
                        l.validations = vec![green("nextest", HEAD)];
                    }
                    cases.push(l.clone());
                    l.stage = "review".into();
                    cases.push(l);
                }
            }
        }
        let p = chart();
        let s = sup();
        for action in plan(&cases, &p, &s, NOW) {
            let expected: Vec<Requirement> = match &action {
                // Advancing INTO `review` is gated by `review`'s own requires.
                SuperviseAction::Advance { next_stage, .. } => p
                    .stage(next_stage)
                    .expect("next stage is configured")
                    .parsed_requires(),
                // Landing is gated by the chart-wide land gate.
                SuperviseAction::Enqueue { .. } => s.parsed_land_requires(),
                _ => continue,
            };
            let got = match &action {
                SuperviseAction::Advance { authorized_by, .. }
                | SuperviseAction::Enqueue { authorized_by, .. } => authorized_by,
                _ => unreachable!(),
            };
            assert_eq!(
                got.met(),
                expected.as_slice(),
                "a mutating action's authorization did not match what its target declared: {action:?}"
            );
        }
    }

    #[test]
    fn a_short_sha_matches_a_full_one_in_both_directions() {
        let mut l = lane("code");
        l.validations = vec![green("nextest", "4babb09")];
        l.approval = Some(approval("4babb09"));
        assert_eq!(one(l).token(), "advance");
    }

    #[test]
    fn an_empty_recorded_commit_never_matches_the_current_tip() {
        // A prefix comparison against "" matches everything; that would make
        // every unrecorded validation look current.
        let mut l = lane("code");
        l.validations = vec![green("nextest", "")];
        assert_eq!(one(l).token(), "validate");
    }

    #[test]
    fn every_action_and_reason_has_a_distinct_token() {
        let actions = [
            SuperviseAction::Validate {
                row: 1,
                tasks: vec![],
                commit_sha: String::new(),
            },
            SuperviseAction::Advance {
                row: 1,
                next_stage: String::new(),
                authorized_by: Authorization::default(),
            },
            SuperviseAction::Enqueue {
                row: 1,
                commit_sha: String::new(),
                authorized_by: Authorization::default(),
            },
            SuperviseAction::Escalate {
                row: 1,
                why: EscalationReason::StageMissing {
                    name: String::new(),
                },
            },
            SuperviseAction::Hold { row: 1, why: "" },
        ];
        let mut t: Vec<&str> = actions.iter().map(SuperviseAction::token).collect();
        t.sort_unstable();
        let n = t.len();
        t.dedup();
        assert_eq!(n, t.len(), "two actions share a token");

        let reasons = [
            EscalationReason::ValidationFailed {
                task: "t".into(),
                class: ValidationClass::TestFailure,
            },
            EscalationReason::ValidationUnresolved {
                task: "t".into(),
                class: ValidationClass::EnvironmentError,
            },
            EscalationReason::AwaitingApproval {
                stage: "s".into(),
                commit_sha: HEAD.into(),
            },
            EscalationReason::HandoffIncomplete {
                missing: Requirement::ParentReport,
            },
            EscalationReason::StageMissing { name: "n".into() },
        ];
        let mut rt: Vec<&str> = reasons.iter().map(EscalationReason::token).collect();
        rt.sort_unstable();
        let rn = rt.len();
        rt.dedup();
        assert_eq!(rn, rt.len(), "two reasons share a token");
        for r in &reasons {
            assert!(!r.explain().is_empty(), "{r:?} explains nothing");
        }
        // Exactly one reason is a request for judgement.
        assert_eq!(reasons.iter().filter(|r| r.wants_primary()).count(), 1);
    }
}
