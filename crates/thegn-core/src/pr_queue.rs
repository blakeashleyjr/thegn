//! The PR queue's decision layer — what is blocking a queued pull request, and
//! what (if anything) thegn should do about it.
//!
//! Pure: no network, no DB, no subprocess. That is deliberate rather than
//! stylistic. On a shared repo the interesting behavior is not "can we call the
//! API" but "may we act at all" — don't stomp a teammate's push, don't merge
//! unreviewed work, don't touch someone else's pull request. Every one of those
//! is a *decision*, so putting them here makes them table-testable instead of an
//! emergent property of driver control flow (and carries the core's 95% gate).
//!
//! The driver in `thegn-host/src/pr_driver.rs` only executes what [`decide`]
//! returns.

use crate::agent_task::TaskKind;
use crate::config_pr_queue::{PrMergeMode, PrQueueConfig, PrWatchKind};
use crate::forge::model::{Bucket, CheckRun, PrStatus, check_bucket};
use std::fmt;

/// Prove that the fetched PR is the same repository and number that was
/// explicitly bound when the queue row was created.
pub fn attest_pr_identity(
    bound_url: Option<&str>,
    number: u64,
    fetched_url: &str,
    fetched_number: u64,
) -> Result<(), String> {
    let readd = format!("run `thegn pr queue rebind --pr {number}` to verify it");
    let Some(bound_url) = bound_url else {
        return Err(format!(
            "queued PR identity is unbound; fetched identity is {fetched_url} (#{fetched_number}); {readd}"
        ));
    };
    let Some(bound) = crate::forge::model::repo_identity_from_pr_url(bound_url) else {
        return Err(format!(
            "queued PR identity {bound_url} is invalid; fetched identity is {fetched_url} (#{fetched_number}); {readd}"
        ));
    };
    if !pr_url_number_matches(bound_url, number) {
        return Err(format!(
            "queued PR identity {bound_url} does not identify pull request #{number}; {readd}"
        ));
    }
    if fetched_number != number {
        return Err(format!(
            "queued PR identity {bound_url} (#{number}) differs from fetched identity {fetched_url} (#{fetched_number}); {readd}"
        ));
    }
    let Some(fetched) = crate::forge::model::repo_identity_from_pr_url(fetched_url) else {
        return Err(format!(
            "queued PR identity {bound_url} (#{number}) differs from invalid fetched identity {fetched_url}; {readd}"
        ));
    };
    if !pr_url_number_matches(fetched_url, fetched_number) {
        return Err(format!(
            "fetched PR identity {fetched_url} does not identify pull request #{fetched_number}; {readd}"
        ));
    }
    if !bound.matches(&fetched) {
        return Err(format!(
            "queued PR identity {}:{} (#{number}) differs from fetched identity {}:{} (#{fetched_number}); {readd}",
            bound.host, bound.path, fetched.host, fetched.path
        ));
    }
    Ok(())
}

/// Immutable queue-row facts captured before a forge read. Equality with a
/// fresh row fences any explicit rebind that happened while the read ran.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueRowSnapshot {
    pub number: u64,
    pub key: String,
    pub pr_url: Option<String>,
    pub forge: String,
    pub worktree: Option<String>,
    pub branch: String,
    pub checkout_gen: i64,
}

/// Revalidate the persisted queue authorization immediately before an effect.
pub fn attest_queue_row(
    snapshot: &QueueRowSnapshot,
    current: Option<&crate::db::PrQueueRow>,
) -> Result<(), String> {
    let Some(current) = current else {
        return Err(format!(
            "queued PR row disappeared; run `thegn pr queue rebind --pr {}`",
            snapshot.number
        ));
    };
    if current.key != snapshot.key
        || current.pr_url != snapshot.pr_url
        || current.forge != snapshot.forge
        || current.worktree != snapshot.worktree
        || current.branch != snapshot.branch
        || current.checkout_gen != snapshot.checkout_gen
        || matches!(current.status.as_str(), "merged" | "closed")
    {
        return Err(format!(
            "queued PR identity or checkout changed; run `thegn pr queue rebind --pr {}`",
            current.number
        ));
    }
    Ok(())
}

pub(crate) fn pr_url_number_matches(url: &str, expected: u64) -> bool {
    let Some((_, path)) = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .and_then(|rest| rest.split_once('/'))
    else {
        return false;
    };
    let mut parts = path.trim_end_matches('/').split('/');
    let Some(last) = parts.next_back() else {
        return false;
    };
    let Some(route) = parts.next_back() else {
        return false;
    };
    matches!(route, "pull" | "pulls" | "merge_requests")
        && last.parse::<u64>().is_ok_and(|number| number == expected)
}

/// What is keeping a queued pull request from merging.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Blocker {
    /// Nothing — it satisfies every configured gate.
    None,
    /// It is a draft. Never merged, never agent-fixed; the author isn't done.
    Draft,
    /// One or more checks failed. Carries the failing check names.
    Ci(Vec<String>),
    /// Some checks are still running. Not actionable — just wait.
    ChecksPending,
    /// Conflicts with, or has fallen behind, the base branch.
    Conflict,
    /// A reviewer asked for changes.
    ChangesRequested,
    /// Approval is required and hasn't been given yet.
    AwaitingReview,
    /// The pull request is no longer open (merged or closed elsewhere).
    Closed,
}

impl Blocker {
    /// The stable wire word recorded in the DB row's `blocker` column.
    pub fn as_str(&self) -> &'static str {
        match self {
            Blocker::None => "none",
            Blocker::Draft => "draft",
            Blocker::Ci(_) => "ci",
            Blocker::ChecksPending => "checks_pending",
            Blocker::Conflict => "conflict",
            Blocker::ChangesRequested => "changes_requested",
            Blocker::AwaitingReview => "awaiting_review",
            Blocker::Closed => "closed",
        }
    }

    /// The `[pr_queue] watch` class this blocker belongs to, if an agent could
    /// ever act on it. `None` means no agent can help (a draft is the author's
    /// call; pending checks just need time).
    pub fn watch_kind(&self) -> Option<PrWatchKind> {
        match self {
            Blocker::Ci(_) => Some(PrWatchKind::Ci),
            Blocker::Conflict => Some(PrWatchKind::Conflict),
            Blocker::ChangesRequested => Some(PrWatchKind::Review),
            _ => None,
        }
    }

    /// The agent task kind for this blocker, if one applies.
    pub fn task_kind(&self) -> Option<TaskKind> {
        match self {
            Blocker::Ci(_) => Some(TaskKind::PrCiFailure),
            Blocker::Conflict => Some(TaskKind::PrConflict),
            Blocker::ChangesRequested => Some(TaskKind::PrReview),
            _ => None,
        }
    }
}

/// A queued pull request's persisted status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrqStatus {
    Watching,
    BlockedCi,
    BlockedConflict,
    BlockedReview,
    AgentRunning,
    Ready,
    Merging,
    Merged,
    NeedsHuman,
    Closed,
}

impl PrqStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            PrqStatus::Watching => "watching",
            PrqStatus::BlockedCi => "blocked_ci",
            PrqStatus::BlockedConflict => "blocked_conflict",
            PrqStatus::BlockedReview => "blocked_review",
            PrqStatus::AgentRunning => "agent_running",
            PrqStatus::Ready => "ready",
            PrqStatus::Merging => "merging",
            PrqStatus::Merged => "merged",
            PrqStatus::NeedsHuman => "needs_human",
            PrqStatus::Closed => "closed",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "watching" => PrqStatus::Watching,
            "blocked_ci" => PrqStatus::BlockedCi,
            "blocked_conflict" => PrqStatus::BlockedConflict,
            "blocked_review" => PrqStatus::BlockedReview,
            "agent_running" => PrqStatus::AgentRunning,
            "ready" => PrqStatus::Ready,
            "merging" => PrqStatus::Merging,
            "merged" => PrqStatus::Merged,
            "needs_human" => PrqStatus::NeedsHuman,
            "closed" => PrqStatus::Closed,
            _ => return None,
        })
    }

    /// Settled states — the queue stops acting on these until something external
    /// changes (a push, a review, a human).
    pub fn is_settled(self) -> bool {
        matches!(
            self,
            PrqStatus::Merged | PrqStatus::Closed | PrqStatus::NeedsHuman
        )
    }

    /// The status a blocker maps onto while it is unresolved.
    pub fn for_blocker(b: &Blocker) -> PrqStatus {
        match b {
            Blocker::None => PrqStatus::Ready,
            Blocker::Closed => PrqStatus::Closed,
            Blocker::Ci(_) => PrqStatus::BlockedCi,
            Blocker::Conflict => PrqStatus::BlockedConflict,
            Blocker::ChangesRequested | Blocker::AwaitingReview => PrqStatus::BlockedReview,
            // A draft or an in-flight check is just "not yet" — the author or CI
            // will move it without thegn doing anything.
            Blocker::Draft | Blocker::ChecksPending => PrqStatus::Watching,
        }
    }
}

/// What the driver should do with one queued pull request this pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueueAction {
    /// Nothing to do — record the status and move on.
    Wait,
    /// Dispatch the agent for this blocker.
    DispatchAgent(TaskKind),
    /// Hand the merge to the forge's own auto-merge.
    EnableAutoMerge,
    /// Merge it directly.
    Merge,
    /// It is mergeable but `merge_mode = "ready"` — tell the user, don't act.
    MarkReady,
    /// Stop and ask for a human, with the reason.
    NeedsHuman(String),
    /// It left the queue's remit (merged or closed on the forge).
    Drop,
}

/// The per-row state `decide` needs that isn't on `PrStatus`.
#[derive(Debug, Clone, Default)]
pub struct PrQueueFacts {
    /// The worktree the entry was queued from, if any. Absent ⇒ no checkout for
    /// an agent to work in.
    pub worktree: Option<String>,
    /// Agent-dispatch cycles already spent on this pull request.
    pub agent_attempts: u32,
    /// The head OID thegn last observed (or produced).
    pub last_head_oid: Option<String>,
    /// Whether the session's user authored this pull request.
    pub is_own: bool,
    /// Whether an agent command actually resolved. Without one, blockers are
    /// reported but never fixed.
    pub agent_available: bool,
}

/// Classify what is blocking a pull request. Reads only fields already carried
/// by `PrStatus`, so no extra fetching is needed to decide.
///
/// Order matters and encodes intent: a closed PR is out of scope, a draft is the
/// author's business, a red check is more actionable than a stale base, and
/// "awaiting review" is reported last because it is the normal resting state of
/// a healthy pull request.
pub fn classify(pr: &PrStatus, cfg: &PrQueueConfig) -> Blocker {
    if !pr.state.eq_ignore_ascii_case("OPEN") {
        return Blocker::Closed;
    }
    if pr.is_draft {
        return Blocker::Draft;
    }

    let failing = failing_checks(&pr.status_check_rollup);
    if cfg.require_checks && !failing.is_empty() {
        return Blocker::Ci(failing);
    }

    if is_conflicted(&pr.mergeable, &pr.merge_state_status) {
        return Blocker::Conflict;
    }

    if changes_requested(pr.review_decision.as_deref()) {
        return Blocker::ChangesRequested;
    }

    // Pending checks are only a blocker once nothing else is wrong — otherwise a
    // half-finished run would mask a failure that is already visible.
    if cfg.require_checks && has_pending(&pr.status_check_rollup) {
        return Blocker::ChecksPending;
    }

    if cfg.require_approval && !is_approved(pr.review_decision.as_deref()) {
        return Blocker::AwaitingReview;
    }

    Blocker::None
}

/// Names of the checks that failed.
fn failing_checks(rollup: &[CheckRun]) -> Vec<String> {
    rollup
        .iter()
        .filter(|c| check_bucket(c) == Bucket::Fail)
        .map(|c| {
            if c.name.trim().is_empty() {
                c.workflow_name.clone().unwrap_or_else(|| "check".into())
            } else {
                c.name.clone()
            }
        })
        .collect()
}

fn has_pending(rollup: &[CheckRun]) -> bool {
    rollup.iter().any(|c| check_bucket(c) == Bucket::Pending)
}

/// Whether the pull request cannot merge because of its base.
///
/// `mergeable = CONFLICTING` is the unambiguous signal. `merge_state_status` adds
/// the cases GitHub reports separately: `DIRTY` (conflicts) and `BEHIND` (needs
/// the base merged in first, under a strict-status branch rule). `BLOCKED` is
/// deliberately NOT treated as a conflict — it means a *rule* is unmet (a
/// missing review, a required check), which the review/CI arms already cover and
/// which rebasing would not fix.
fn is_conflicted(mergeable: &str, merge_state: &str) -> bool {
    mergeable.eq_ignore_ascii_case("CONFLICTING")
        || merge_state.eq_ignore_ascii_case("DIRTY")
        || merge_state.eq_ignore_ascii_case("BEHIND")
}

fn changes_requested(decision: Option<&str>) -> bool {
    decision.is_some_and(|d| d.eq_ignore_ascii_case("CHANGES_REQUESTED"))
}

fn is_approved(decision: Option<&str>) -> bool {
    decision.is_some_and(|d| d.eq_ignore_ascii_case("APPROVED"))
}

/// Whether a pull request's attempt budget should refill.
///
/// Only a head thegn did **not** produce counts. If the agent's own push refilled
/// the budget, a looping agent would retry forever — the budget exists precisely
/// to bound that. `last_head_oid` is stamped with whatever thegn last saw *after*
/// its own dispatch, so a differing OID means someone else moved the branch.
pub fn attempts_reset(prev_head: Option<&str>, new_head: &str, cfg: &PrQueueConfig) -> bool {
    if !cfg.reset_attempts_on_push || new_head.is_empty() {
        return false;
    }
    match prev_head {
        // First sighting is not a "push" — it's just the first look.
        None => false,
        Some(prev) => !prev.is_empty() && prev != new_head,
    }
}

/// Whether the branch moved under thegn — someone else pushed.
pub fn foreign_push(prev_head: Option<&str>, new_head: &str) -> bool {
    match prev_head {
        None => false,
        Some(prev) => !prev.is_empty() && !new_head.is_empty() && prev != new_head,
    }
}

/// Decide what to do about one queued pull request.
///
/// Every team-safety rule lives here rather than in the driver, so each is a
/// single testable branch.
pub fn decide(blocker: &Blocker, facts: &PrQueueFacts, cfg: &PrQueueConfig) -> QueueAction {
    match blocker {
        // Left the queue's remit entirely.
        Blocker::Closed => QueueAction::Drop,

        // Not thegn's business: the author hasn't marked it ready, or CI is
        // still running. Both resolve without us.
        Blocker::Draft | Blocker::ChecksPending => QueueAction::Wait,

        // Mergeable under every configured gate.
        Blocker::None => match cfg.merge_mode {
            PrMergeMode::AutoMerge => QueueAction::EnableAutoMerge,
            PrMergeMode::Thegn => QueueAction::Merge,
            PrMergeMode::Ready => QueueAction::MarkReady,
        },

        // Nobody has reviewed it yet. An agent cannot conjure an approval, and
        // nagging is not thegn's job — surface it and wait.
        Blocker::AwaitingReview => QueueAction::Wait,

        // The three fixable classes.
        b => {
            let Some(kind) = b.task_kind() else {
                return QueueAction::Wait;
            };

            // Not enabled for this blocker class — still tracked and displayed,
            // just never auto-fixed.
            match b.watch_kind() {
                Some(w) if !cfg.watches(w) => return QueueAction::Wait,
                _ => {}
            }

            // No agent resolved: report the blocker, don't pretend to fix it.
            if !facts.agent_available {
                return QueueAction::Wait;
            }

            // Writing to a pull request you didn't author is not ours to do.
            if cfg.own_prs_only && !facts.is_own {
                return QueueAction::Wait;
            }

            // The agent works in a checkout. Without one, say so explicitly
            // rather than silently skipping the row forever.
            let Some(_) = facts.worktree.as_deref().filter(|w| !w.trim().is_empty()) else {
                return QueueAction::NeedsHuman(
                    "no local worktree for this PR — check the branch out to let an agent fix it"
                        .into(),
                );
            };

            if facts.agent_attempts >= cfg.agent_max_attempts {
                return QueueAction::NeedsHuman(format!(
                    "agent could not unblock this PR in {} attempt(s)",
                    cfg.agent_max_attempts
                ));
            }

            QueueAction::DispatchAgent(kind)
        }
    }
}

/// Apply the shared safety policy required before dispatching a PR agent.
///
/// Callers still establish facts that require I/O (such as authorship proof and
/// current checkout state); this function makes the policy decision from those
/// facts and the persisted queue row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentAdmissionRefusal {
    UnknownStatus(String),
    RowState(&'static str),
    Disabled,
    Unwatched(PrWatchKind),
    ForeignPush,
    NoAgent,
    NotOwn,
    Policy,
    NeedsHuman(String),
    Closed,
    Blocker,
    PromptInvalid { kind: TaskKind },
    IsolationRefused(String),
}

/// Maximum lifetime of a PR-agent claim, shared with merge-queue claims.
pub const PR_AGENT_MAX_LEASE_MS: i64 = crate::store::MERGE_CLAIM_MAX_LEASE_MS;
pub const PR_AGENT_LEASE_LOST: &str = "PR agent lease lost";

pub struct PrAgentAdmit<'a> {
    pub key: &'a str,
    pub expected_attempts: u32,
    pub expected_checkout_gen: i64,
    pub max_attempts: u32,
    pub head: &'a str,
    pub blocker: &'a str,
    pub detail: &'a str,
    pub lease_until_ms: i64,
    pub owner_hint: Option<&'a str>,
    pub ci_candidate: Option<&'a crate::ci_log::CiLogCandidate>,
}

#[derive(Clone)]
pub struct PrAgentLease {
    pub key: String,
    pub generation: i64,
    pub attempt: u32,
    pub lease_until_ms: i64,
    pub(crate) owner_token: String,
}

impl fmt::Debug for PrAgentLease {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PrAgentLease")
            .field("key", &self.key)
            .field("generation", &self.generation)
            .field("attempt", &self.attempt)
            .field("lease_until_ms", &self.lease_until_ms)
            .field("owner_token", &"[redacted]")
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrAgentRefusal {
    Missing,
    InvalidLease,
    Settled {
        status: String,
    },
    Held {
        generation: i64,
        lease_until_ms: i64,
    },
    Stale {
        reason: String,
    },
    BudgetExhausted {
        attempts: u32,
        max: u32,
    },
    DuplicateCandidate,
}

impl fmt::Display for PrAgentRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing => f.write_str("the PR queue row no longer exists; not dispatched"),
            Self::InvalidLease => {
                f.write_str("the requested PR-agent lease is invalid; not dispatched")
            }
            Self::Settled { status } => {
                write!(f, "the PR queue row is settled ({status}); not dispatched")
            }
            Self::Held { generation, .. } => write!(
                f,
                "another PR agent holds this PR (generation {generation}); not dispatched"
            ),
            Self::Stale { reason } => write!(
                f,
                "the queue row changed since it was read ({reason}); not dispatched"
            ),
            Self::BudgetExhausted { attempts, max } => write!(
                f,
                "the PR-queue agent budget is exhausted ({attempts}/{max})"
            ),
            Self::DuplicateCandidate => f.write_str("this CI candidate was already dispatched"),
        }
    }
}

#[derive(Debug)]
pub enum PrAgentAdmission {
    Admitted(PrAgentLease),
    Refused(PrAgentRefusal),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrAgentRowSnapshot {
    pub status: String,
    pub agent_attempts: u32,
    pub checkout_gen: i64,
    pub claim_owner_present: bool,
    pub claim_generation: i64,
    pub claim_lease_until_ms: Option<i64>,
}

pub fn decide_pr_agent_admission(
    row: &PrAgentRowSnapshot,
    request: &PrAgentAdmit<'_>,
    now_ms: i64,
) -> Result<(), PrAgentRefusal> {
    let Some(status) = PrqStatus::parse(&row.status) else {
        return Err(PrAgentRefusal::Settled {
            status: row.status.clone(),
        });
    };
    if status.is_settled() {
        return Err(PrAgentRefusal::Settled {
            status: row.status.clone(),
        });
    }
    if row.claim_owner_present
        && row
            .claim_lease_until_ms
            .is_some_and(|until| until >= now_ms)
    {
        return Err(PrAgentRefusal::Held {
            generation: row.claim_generation,
            lease_until_ms: row.claim_lease_until_ms.unwrap_or(now_ms),
        });
    }
    if row.checkout_gen != request.expected_checkout_gen {
        return Err(PrAgentRefusal::Stale {
            reason: "checkout generation differs".into(),
        });
    }
    if row.agent_attempts != request.expected_attempts {
        return Err(PrAgentRefusal::Stale {
            reason: "attempt count differs".into(),
        });
    }
    if row.agent_attempts >= request.max_attempts {
        return Err(PrAgentRefusal::BudgetExhausted {
            attempts: row.agent_attempts,
            max: request.max_attempts,
        });
    }
    Ok(())
}

/// Attribute a post-run head only when local and forge observations agree and
/// typed ancestry proves it descends from the admitted head. Unknown provenance
/// is never attributed: teammate pushes, no push, local-only commits, force
/// pushes, and unknown ancestry all return `None`.
pub fn own_agent_head(
    admitted: &str,
    local_head: Option<&str>,
    remote_head: Option<&str>,
    local_descends: Option<bool>,
) -> Option<String> {
    let (Some(local), Some(remote)) = (local_head, remote_head) else {
        return None;
    };
    let full_oid = |oid: &str| {
        matches!(oid.len(), 40 | 64) && oid.bytes().all(|byte| byte.is_ascii_hexdigit())
    };
    (!admitted.is_empty()
        && local == remote
        && full_oid(local)
        && full_oid(remote)
        && local != admitted
        && local_descends == Some(true))
    .then(|| remote.to_owned())
}

impl fmt::Display for AgentAdmissionRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownStatus(status) => write!(
                f,
                "unknown PR queue status `{status}`; agent dispatch refused"
            ),
            Self::RowState(status) => {
                write!(f, "PR queue row is `{status}`; agent dispatch refused")
            }
            Self::Disabled => f.write_str("PR queue is disabled; agent dispatch refused"),
            Self::Unwatched(kind) => {
                let class = match kind {
                    PrWatchKind::Ci => "CI",
                    PrWatchKind::Conflict => "conflict",
                    PrWatchKind::Review => "review",
                };
                write!(
                    f,
                    "{class} is not in [pr_queue] watch; agent dispatch refused"
                )
            }
            Self::ForeignPush => f.write_str(
                "someone else pushed to this branch — not running an agent over their work",
            ),
            Self::NoAgent => f.write_str("no PR-queue agent is configured; agent dispatch refused"),
            Self::NotOwn => f.write_str("PR is not authored by this user; agent dispatch refused"),
            Self::Policy => f.write_str("PR queue policy does not permit agent dispatch"),
            Self::NeedsHuman(reason) => f.write_str(reason),
            Self::Closed => f.write_str("PR is closed; agent dispatch refused"),
            Self::Blocker => f.write_str("PR blocker does not permit agent dispatch"),
            Self::PromptInvalid { kind } => write!(
                f,
                "agent not dispatched: [pr_queue.prompts] template for {kind} is invalid"
            ),
            Self::IsolationRefused(reason) => f.write_str(reason),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IsolationAdmission {
    Admitted,
    Degraded(String),
    Refused(String),
}

pub fn admit_prepared_dispatch(
    kind: TaskKind,
    prompt_ok: bool,
    isolation: &IsolationAdmission,
) -> Result<(), AgentAdmissionRefusal> {
    if let IsolationAdmission::Refused(reason) = isolation {
        return Err(AgentAdmissionRefusal::IsolationRefused(reason.clone()));
    }
    if !prompt_ok {
        return Err(AgentAdmissionRefusal::PromptInvalid { kind });
    }
    Ok(())
}

pub fn admit_agent_dispatch(
    row_status: &str,
    blocker: &Blocker,
    facts: &PrQueueFacts,
    new_head: &str,
    cfg: &PrQueueConfig,
) -> Result<TaskKind, AgentAdmissionRefusal> {
    let status = PrqStatus::parse(row_status)
        .ok_or_else(|| AgentAdmissionRefusal::UnknownStatus(row_status.to_string()))?;
    if status.is_settled() || matches!(status, PrqStatus::AgentRunning | PrqStatus::Merging) {
        return Err(AgentAdmissionRefusal::RowState(status.as_str()));
    }
    if !cfg.enabled {
        return Err(AgentAdmissionRefusal::Disabled);
    }
    if let Some(kind) = blocker.watch_kind().filter(|kind| !cfg.watches(*kind)) {
        return Err(AgentAdmissionRefusal::Unwatched(kind));
    }
    match decide(blocker, facts, cfg) {
        QueueAction::DispatchAgent(kind) => {
            if foreign_push(facts.last_head_oid.as_deref(), new_head) && cfg.pause_on_foreign_push {
                Err(AgentAdmissionRefusal::ForeignPush)
            } else {
                Ok(kind)
            }
        }
        QueueAction::Wait if !facts.agent_available => Err(AgentAdmissionRefusal::NoAgent),
        QueueAction::Wait if cfg.own_prs_only && !facts.is_own => {
            Err(AgentAdmissionRefusal::NotOwn)
        }
        QueueAction::Wait => Err(AgentAdmissionRefusal::Policy),
        QueueAction::NeedsHuman(reason) => Err(AgentAdmissionRefusal::NeedsHuman(reason)),
        QueueAction::Drop => Err(AgentAdmissionRefusal::Closed),
        _ => Err(AgentAdmissionRefusal::Blocker),
    }
}

/// Convert an agent dispatch into the shared foreign-push hold when configured.
pub fn pause_for_foreign_push(
    action: QueueAction,
    moved: bool,
    cfg: &PrQueueConfig,
) -> QueueAction {
    if moved && cfg.pause_on_foreign_push && matches!(action, QueueAction::DispatchAgent(_)) {
        QueueAction::NeedsHuman(
            "someone else pushed to this branch — not running an agent over their work".into(),
        )
    } else {
        action
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_pr_queue::{PrMergeMethod, PrWatchKind};

    fn cfg() -> PrQueueConfig {
        PrQueueConfig {
            enabled: true,
            ..PrQueueConfig::default()
        }
    }

    #[test]
    fn pr_agent_admission_decision_table() {
        let request = PrAgentAdmit {
            key: "k",
            expected_attempts: 0,
            expected_checkout_gen: 0,
            max_attempts: 1,
            head: "h",
            blocker: "ci",
            detail: "d",
            lease_until_ms: 10_000,
            owner_hint: None,
            ci_candidate: None,
        };
        for status in [
            "watching",
            "blocked_ci",
            "blocked_conflict",
            "blocked_review",
            "agent_running",
            "ready",
            "merging",
            "merged",
            "needs_human",
            "closed",
            "unknown",
        ] {
            for (owner, expiry) in [
                (false, None),
                (true, None),
                (true, Some(100)),
                (true, Some(99)),
            ] {
                for (attempts, checkout, budget) in [(0, 0, 1), (1, 0, 1), (0, 1, 1), (0, 0, 0)] {
                    let row = PrAgentRowSnapshot {
                        status: status.into(),
                        agent_attempts: attempts,
                        checkout_gen: checkout,
                        claim_owner_present: owner,
                        claim_generation: 4,
                        claim_lease_until_ms: expiry,
                    };
                    let request = PrAgentAdmit {
                        max_attempts: budget,
                        ..request_for_test(&request)
                    };
                    let actual = decide_pr_agent_admission(&row, &request, 100).err();
                    let expected = match PrqStatus::parse(status) {
                        None => Some(PrAgentRefusal::Settled {
                            status: status.into(),
                        }),
                        Some(parsed_status) if parsed_status.is_settled() => {
                            Some(PrAgentRefusal::Settled {
                                status: parsed_status.as_str().into(),
                            })
                        }
                        Some(_) if owner && expiry.is_some_and(|until| until >= 100) => {
                            Some(PrAgentRefusal::Held {
                                generation: 4,
                                lease_until_ms: expiry.unwrap_or(100),
                            })
                        }
                        Some(_) if checkout != 0 => Some(PrAgentRefusal::Stale {
                            reason: "checkout generation differs".into(),
                        }),
                        Some(_) if attempts != 0 => Some(PrAgentRefusal::Stale {
                            reason: "attempt count differs".into(),
                        }),
                        Some(_) if attempts >= budget => Some(PrAgentRefusal::BudgetExhausted {
                            attempts,
                            max: budget,
                        }),
                        Some(_) => None,
                    };
                    assert_eq!(
                        actual, expected,
                        "status={status}, owner={owner}, expiry={expiry:?}, attempts={attempts}, checkout={checkout}, budget={budget}"
                    );
                }
            }
        }
    }

    fn request_for_test<'a>(request: &PrAgentAdmit<'a>) -> PrAgentAdmit<'a> {
        PrAgentAdmit {
            key: request.key,
            expected_attempts: request.expected_attempts,
            expected_checkout_gen: request.expected_checkout_gen,
            max_attempts: request.max_attempts,
            head: request.head,
            blocker: request.blocker,
            detail: request.detail,
            lease_until_ms: request.lease_until_ms,
            owner_hint: request.owner_hint,
            ci_candidate: request.ci_candidate,
        }
    }

    fn facts() -> PrQueueFacts {
        PrQueueFacts {
            worktree: Some("/w/pr".into()),
            agent_attempts: 0,
            last_head_oid: Some("abc".into()),
            is_own: true,
            agent_available: true,
        }
    }

    fn check(name: &str, conclusion: Option<&str>, status: &str) -> CheckRun {
        CheckRun {
            name: name.into(),
            status: status.into(),
            conclusion: conclusion.map(String::from),
            state: None,
            workflow_name: None,
            details_url: None,
            started_at: None,
            completed_at: None,
        }
    }

    fn pr() -> PrStatus {
        PrStatus {
            number: 7,
            title: "t".into(),
            state: "OPEN".into(),
            url: "u".into(),
            author: None,
            is_draft: false,
            head_ref_name: "feat".into(),
            head_ref_oid: "abc".into(),
            base_ref_name: "main".into(),
            mergeable: "MERGEABLE".into(),
            merge_state_status: "CLEAN".into(),
            review_decision: Some("APPROVED".into()),
            status_check_rollup: vec![check("build", Some("SUCCESS"), "COMPLETED")],
            checks: Default::default(),
        }
    }

    // --- classify ----------------------------------------------------------

    #[test]
    fn a_healthy_pr_has_no_blocker() {
        assert_eq!(classify(&pr(), &cfg()), Blocker::None);
    }

    #[test]
    fn a_closed_or_merged_pr_is_out_of_scope() {
        for state in ["MERGED", "CLOSED", "merged"] {
            let mut p = pr();
            p.state = state.into();
            assert_eq!(classify(&p, &cfg()), Blocker::Closed, "{state}");
        }
    }

    #[test]
    fn a_draft_blocks_even_when_green_and_approved() {
        let mut p = pr();
        p.is_draft = true;
        assert_eq!(classify(&p, &cfg()), Blocker::Draft);
    }

    #[test]
    fn a_failed_check_is_a_ci_blocker_naming_the_check() {
        let mut p = pr();
        p.status_check_rollup = vec![
            check("build", Some("SUCCESS"), "COMPLETED"),
            check("clippy", Some("FAILURE"), "COMPLETED"),
        ];
        assert_eq!(classify(&p, &cfg()), Blocker::Ci(vec!["clippy".into()]));
    }

    #[test]
    fn a_nameless_check_falls_back_to_its_workflow() {
        let mut p = pr();
        let mut c = check("", Some("FAILURE"), "COMPLETED");
        c.workflow_name = Some("CI".into());
        p.status_check_rollup = vec![c];
        assert_eq!(classify(&p, &cfg()), Blocker::Ci(vec!["CI".into()]));
    }

    #[test]
    fn require_checks_off_ignores_red_and_pending_checks() {
        let mut c = cfg();
        c.require_checks = false;
        let mut p = pr();
        p.status_check_rollup = vec![check("clippy", Some("FAILURE"), "COMPLETED")];
        assert_eq!(classify(&p, &c), Blocker::None);
    }

    #[test]
    fn conflicting_and_behind_states_are_conflicts_but_blocked_is_not() {
        for (mergeable, state) in [
            ("CONFLICTING", "CLEAN"),
            ("MERGEABLE", "DIRTY"),
            ("MERGEABLE", "BEHIND"),
        ] {
            let mut p = pr();
            p.mergeable = mergeable.into();
            p.merge_state_status = state.into();
            assert_eq!(
                classify(&p, &cfg()),
                Blocker::Conflict,
                "{mergeable}/{state}"
            );
        }
        // BLOCKED means an unmet *rule*, not a bad merge base — rebasing would
        // not help, so it must not be reported as a conflict.
        let mut p = pr();
        p.merge_state_status = "BLOCKED".into();
        assert_eq!(classify(&p, &cfg()), Blocker::None);
    }

    #[test]
    fn changes_requested_outranks_awaiting_review() {
        let mut p = pr();
        p.review_decision = Some("CHANGES_REQUESTED".into());
        assert_eq!(classify(&p, &cfg()), Blocker::ChangesRequested);
    }

    #[test]
    fn a_red_check_outranks_a_conflict_and_a_review() {
        // The most actionable blocker wins, so the agent gets the useful task.
        let mut p = pr();
        p.status_check_rollup = vec![check("t", Some("FAILURE"), "COMPLETED")];
        p.merge_state_status = "DIRTY".into();
        p.review_decision = Some("CHANGES_REQUESTED".into());
        assert!(matches!(classify(&p, &cfg()), Blocker::Ci(_)));
    }

    #[test]
    fn pending_checks_do_not_mask_a_visible_failure() {
        let mut p = pr();
        p.status_check_rollup = vec![
            check("slow", None, "IN_PROGRESS"),
            check("fast", Some("FAILURE"), "COMPLETED"),
        ];
        assert!(
            matches!(classify(&p, &cfg()), Blocker::Ci(_)),
            "a known failure beats a half-finished run"
        );
        // With nothing failing, the pending run IS the blocker.
        p.status_check_rollup = vec![check("slow", None, "IN_PROGRESS")];
        assert_eq!(classify(&p, &cfg()), Blocker::ChecksPending);
    }

    #[test]
    fn missing_approval_holds_the_pr_only_when_required() {
        let mut p = pr();
        p.review_decision = Some("REVIEW_REQUIRED".into());
        assert_eq!(classify(&p, &cfg()), Blocker::AwaitingReview);
        p.review_decision = None;
        assert_eq!(classify(&p, &cfg()), Blocker::AwaitingReview);

        let mut c = cfg();
        c.require_approval = false;
        assert_eq!(classify(&p, &c), Blocker::None);
    }

    // --- decide: merging ----------------------------------------------------

    #[test]
    fn merge_mode_routes_a_green_pr() {
        let mut c = cfg();
        for (mode, want) in [
            (PrMergeMode::AutoMerge, QueueAction::EnableAutoMerge),
            (PrMergeMode::Thegn, QueueAction::Merge),
            (PrMergeMode::Ready, QueueAction::MarkReady),
        ] {
            c.merge_mode = mode;
            assert_eq!(decide(&Blocker::None, &facts(), &c), want, "{mode}");
        }
    }

    #[test]
    fn the_default_hands_merging_to_the_forge() {
        // The whole point: branch protection stays authoritative, so thegn's
        // view of "ready" can never race a server-side rule.
        assert_eq!(
            decide(&Blocker::None, &facts(), &PrQueueConfig::default()),
            QueueAction::EnableAutoMerge
        );
    }

    #[test]
    fn a_draft_is_never_merged() {
        let mut c = cfg();
        c.merge_mode = PrMergeMode::Thegn;
        assert_eq!(decide(&Blocker::Draft, &facts(), &c), QueueAction::Wait);
    }

    #[test]
    fn an_unapproved_pr_is_never_merged_or_agent_fixed() {
        assert_eq!(
            decide(&Blocker::AwaitingReview, &facts(), &cfg()),
            QueueAction::Wait,
            "an agent cannot conjure an approval"
        );
    }

    #[test]
    fn a_closed_pr_is_dropped() {
        assert_eq!(
            decide(&Blocker::Closed, &facts(), &cfg()),
            QueueAction::Drop
        );
    }

    // --- decide: agent gating ----------------------------------------------

    #[test]
    fn each_fixable_blocker_dispatches_its_own_task_kind() {
        for (b, kind) in [
            (Blocker::Ci(vec!["t".into()]), TaskKind::PrCiFailure),
            (Blocker::Conflict, TaskKind::PrConflict),
            (Blocker::ChangesRequested, TaskKind::PrReview),
        ] {
            assert_eq!(
                decide(&b, &facts(), &cfg()),
                QueueAction::DispatchAgent(kind),
                "{b:?}"
            );
        }
    }

    #[test]
    fn an_unwatched_blocker_class_is_tracked_but_never_fixed() {
        let mut c = cfg();
        c.watch = vec![PrWatchKind::Ci];
        assert!(matches!(
            decide(&Blocker::Ci(vec!["t".into()]), &facts(), &c),
            QueueAction::DispatchAgent(_)
        ));
        assert_eq!(decide(&Blocker::Conflict, &facts(), &c), QueueAction::Wait);
        assert_eq!(
            decide(&Blocker::ChangesRequested, &facts(), &c),
            QueueAction::Wait
        );
    }

    #[test]
    fn no_configured_agent_reports_the_blocker_instead_of_pretending() {
        let f = PrQueueFacts {
            agent_available: false,
            ..facts()
        };
        assert_eq!(decide(&Blocker::Conflict, &f, &cfg()), QueueAction::Wait);
    }

    #[test]
    fn someone_elses_pr_is_watched_but_never_written_to() {
        let f = PrQueueFacts {
            is_own: false,
            ..facts()
        };
        assert_eq!(
            decide(&Blocker::Conflict, &f, &cfg()),
            QueueAction::Wait,
            "own_prs_only must stop the agent"
        );
        // ...unless the user explicitly opts in.
        let mut c = cfg();
        c.own_prs_only = false;
        assert!(matches!(
            decide(&Blocker::Conflict, &f, &c),
            QueueAction::DispatchAgent(_)
        ));
    }

    #[test]
    fn a_pr_with_no_worktree_asks_for_a_human_rather_than_going_quiet() {
        for wt in [None, Some(String::new()), Some("   ".to_string())] {
            let f = PrQueueFacts {
                worktree: wt.clone(),
                ..facts()
            };
            match decide(&Blocker::Conflict, &f, &cfg()) {
                QueueAction::NeedsHuman(msg) => {
                    assert!(msg.contains("worktree"), "{msg}");
                }
                other => panic!("expected NeedsHuman for {wt:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn an_exhausted_attempt_budget_asks_for_a_human() {
        let f = PrQueueFacts {
            agent_attempts: 2,
            ..facts()
        };
        match decide(&Blocker::Ci(vec!["t".into()]), &f, &cfg()) {
            QueueAction::NeedsHuman(msg) => assert!(msg.contains("2 attempt")),
            other => panic!("{other:?}"),
        }
        // One short of the budget still dispatches.
        let f = PrQueueFacts {
            agent_attempts: 1,
            ..facts()
        };
        assert!(matches!(
            decide(&Blocker::Ci(vec!["t".into()]), &f, &cfg()),
            QueueAction::DispatchAgent(_)
        ));
    }

    // --- attempt budget / foreign pushes ------------------------------------

    #[test]
    fn the_budget_refills_only_when_the_branch_actually_moved() {
        let c = cfg();
        assert!(attempts_reset(Some("abc"), "def", &c), "a new head refills");
        assert!(
            !attempts_reset(Some("abc"), "abc", &c),
            "same head does not"
        );
        // The first sighting is not a push.
        assert!(!attempts_reset(None, "abc", &c));
        assert!(!attempts_reset(Some(""), "abc", &c));
        // An empty new head is a fetch failure, not a move.
        assert!(!attempts_reset(Some("abc"), "", &c));
    }

    #[test]
    fn the_budget_never_refills_when_the_user_turned_it_off() {
        let mut c = cfg();
        c.reset_attempts_on_push = false;
        assert!(!attempts_reset(Some("abc"), "def", &c));
    }

    #[test]
    fn a_moved_head_is_a_foreign_push_only_when_both_are_known() {
        assert!(foreign_push(Some("abc"), "def"));
        assert!(!foreign_push(Some("abc"), "abc"));
        assert!(!foreign_push(None, "abc"), "first sighting is not a push");
        assert!(!foreign_push(Some(""), "abc"));
        assert!(
            !foreign_push(Some("abc"), ""),
            "an unknown new head is a fetch failure, not a teammate"
        );
    }

    #[test]
    fn shared_agent_admission_combines_status_watch_and_foreign_push_policy() {
        let statuses = [
            PrqStatus::Watching,
            PrqStatus::BlockedCi,
            PrqStatus::BlockedConflict,
            PrqStatus::BlockedReview,
            PrqStatus::AgentRunning,
            PrqStatus::Ready,
            PrqStatus::Merging,
            PrqStatus::Merged,
            PrqStatus::NeedsHuman,
            PrqStatus::Closed,
        ];
        let blocker = Blocker::Ci(vec!["build".into()]);
        for status in statuses {
            for watched in [true, false] {
                for (previous, pause, moved) in [
                    (Some("abc"), true, true),
                    (Some("abc"), false, true),
                    (None, true, false),
                    (Some("abc"), true, false),
                ] {
                    let mut config = cfg();
                    if !watched {
                        config.watch = vec![PrWatchKind::Conflict];
                    }
                    config.pause_on_foreign_push = pause;
                    let facts = PrQueueFacts {
                        last_head_oid: previous.map(str::to_string),
                        ..facts()
                    };
                    let head = if moved { "def" } else { "abc" };
                    let got =
                        admit_agent_dispatch(status.as_str(), &blocker, &facts, head, &config);
                    let status_allows = !status.is_settled()
                        && !matches!(status, PrqStatus::AgentRunning | PrqStatus::Merging);
                    let expected = watched && status_allows && (!moved || !pause);
                    assert_eq!(
                        got == Ok(TaskKind::PrCiFailure),
                        expected,
                        "status={}, watched={watched}, moved={moved}, pause={pause}: {got:?}",
                        status.as_str()
                    );
                    if !status_allows {
                        assert_eq!(got, Err(AgentAdmissionRefusal::RowState(status.as_str())));
                    } else if !watched {
                        assert_eq!(got, Err(AgentAdmissionRefusal::Unwatched(PrWatchKind::Ci)));
                    }
                    if moved && pause && watched && status_allows {
                        assert_eq!(got, Err(AgentAdmissionRefusal::ForeignPush));
                    }
                }
            }
        }
        assert_eq!(
            admit_agent_dispatch("unknown", &blocker, &facts(), "abc", &cfg()),
            Err(AgentAdmissionRefusal::UnknownStatus("unknown".into()))
        );
        let mut disabled = cfg();
        disabled.enabled = false;
        assert_eq!(
            admit_agent_dispatch("blocked_ci", &blocker, &facts(), "abc", &disabled),
            Err(AgentAdmissionRefusal::Disabled)
        );

        let mut unavailable = facts();
        unavailable.agent_available = false;
        assert_eq!(
            admit_agent_dispatch("blocked_ci", &blocker, &unavailable, "abc", &cfg()),
            Err(AgentAdmissionRefusal::NoAgent)
        );
        let mut not_own = facts();
        not_own.is_own = false;
        assert_eq!(
            admit_agent_dispatch("blocked_ci", &blocker, &not_own, "abc", &cfg()),
            Err(AgentAdmissionRefusal::NotOwn)
        );
        let mut no_checkout = facts();
        no_checkout.worktree = None;
        assert!(
            admit_agent_dispatch("blocked_ci", &blocker, &no_checkout, "abc", &cfg())
                .unwrap_err()
                .to_string()
                .contains("worktree")
        );
        let mut over_budget = facts();
        over_budget.agent_attempts = cfg().agent_max_attempts;
        assert!(
            admit_agent_dispatch("blocked_ci", &blocker, &over_budget, "abc", &cfg())
                .unwrap_err()
                .to_string()
                .contains("attempt")
        );
        assert!(
            admit_agent_dispatch("watching", &Blocker::Draft, &facts(), "abc", &cfg())
                .unwrap_err()
                .to_string()
                .contains("does not permit")
        );
        assert!(
            admit_agent_dispatch("watching", &Blocker::Closed, &facts(), "abc", &cfg())
                .unwrap_err()
                .to_string()
                .contains("closed")
        );
        assert!(admit_agent_dispatch("ready", &Blocker::None, &facts(), "abc", &cfg()).is_err());

        let mut ci_only = cfg();
        ci_only.watch = vec![PrWatchKind::Ci];
        assert!(
            admit_agent_dispatch(
                "blocked_conflict",
                &Blocker::Conflict,
                &facts(),
                "abc",
                &ci_only
            )
            .unwrap_err()
            .to_string()
            .contains("conflict is not in [pr_queue] watch")
        );
        assert!(
            admit_agent_dispatch(
                "blocked_review",
                &Blocker::ChangesRequested,
                &facts(),
                "abc",
                &ci_only
            )
            .unwrap_err()
            .to_string()
            .contains("review is not in [pr_queue] watch")
        );
    }

    #[test]
    fn typed_admission_refusal_display_matches_legacy_strings() {
        let cases = [
            (
                AgentAdmissionRefusal::UnknownStatus("mystery".into()),
                "unknown PR queue status `mystery`; agent dispatch refused",
            ),
            (
                AgentAdmissionRefusal::RowState("merged"),
                "PR queue row is `merged`; agent dispatch refused",
            ),
            (
                AgentAdmissionRefusal::Disabled,
                "PR queue is disabled; agent dispatch refused",
            ),
            (
                AgentAdmissionRefusal::Unwatched(PrWatchKind::Ci),
                "CI is not in [pr_queue] watch; agent dispatch refused",
            ),
            (
                AgentAdmissionRefusal::Unwatched(PrWatchKind::Conflict),
                "conflict is not in [pr_queue] watch; agent dispatch refused",
            ),
            (
                AgentAdmissionRefusal::Unwatched(PrWatchKind::Review),
                "review is not in [pr_queue] watch; agent dispatch refused",
            ),
            (
                AgentAdmissionRefusal::ForeignPush,
                "someone else pushed to this branch — not running an agent over their work",
            ),
            (
                AgentAdmissionRefusal::NoAgent,
                "no PR-queue agent is configured; agent dispatch refused",
            ),
            (
                AgentAdmissionRefusal::NotOwn,
                "PR is not authored by this user; agent dispatch refused",
            ),
            (
                AgentAdmissionRefusal::Policy,
                "PR queue policy does not permit agent dispatch",
            ),
            (
                AgentAdmissionRefusal::NeedsHuman("human review required".into()),
                "human review required",
            ),
            (
                AgentAdmissionRefusal::Closed,
                "PR is closed; agent dispatch refused",
            ),
            (
                AgentAdmissionRefusal::Blocker,
                "PR blocker does not permit agent dispatch",
            ),
            (
                AgentAdmissionRefusal::PromptInvalid {
                    kind: TaskKind::PrCiFailure,
                },
                "agent not dispatched: [pr_queue.prompts] template for pr_ci_failure is invalid",
            ),
            (
                AgentAdmissionRefusal::IsolationRefused("sandbox unavailable".into()),
                "sandbox unavailable",
            ),
        ];
        for (refusal, expected) in cases {
            assert_eq!(refusal.to_string(), expected, "{refusal:?}");
        }
    }

    #[test]
    fn prepared_dispatch_table() {
        let kind = TaskKind::PrCiFailure;
        for prompt_ok in [true, false] {
            for isolation in [
                IsolationAdmission::Admitted,
                IsolationAdmission::Degraded("degraded".into()),
                IsolationAdmission::Refused("floor refused".into()),
            ] {
                let got = admit_prepared_dispatch(kind, prompt_ok, &isolation);
                let expected = match (&isolation, prompt_ok) {
                    (IsolationAdmission::Refused(reason), _) => {
                        Err(AgentAdmissionRefusal::IsolationRefused(reason.clone()))
                    }
                    (_, false) => Err(AgentAdmissionRefusal::PromptInvalid { kind }),
                    _ => Ok(()),
                };
                assert_eq!(
                    got, expected,
                    "prompt_ok={prompt_ok}, isolation={isolation:?}"
                );
            }
        }
    }

    #[test]
    fn shared_foreign_push_pause_only_holds_agent_dispatches_when_enabled() {
        let dispatch = QueueAction::DispatchAgent(TaskKind::PrCiFailure);
        let held = pause_for_foreign_push(dispatch.clone(), true, &cfg());
        assert!(
            matches!(held, QueueAction::NeedsHuman(reason) if reason.contains("someone else pushed"))
        );
        assert_eq!(
            pause_for_foreign_push(dispatch.clone(), false, &cfg()),
            dispatch
        );
        let mut config = cfg();
        config.pause_on_foreign_push = false;
        assert_eq!(
            pause_for_foreign_push(dispatch.clone(), true, &config),
            dispatch
        );
        assert_eq!(
            pause_for_foreign_push(QueueAction::Wait, true, &cfg()),
            QueueAction::Wait
        );
    }

    // --- status vocabulary --------------------------------------------------

    #[test]
    fn every_status_round_trips_and_settles_correctly() {
        let all = [
            PrqStatus::Watching,
            PrqStatus::BlockedCi,
            PrqStatus::BlockedConflict,
            PrqStatus::BlockedReview,
            PrqStatus::AgentRunning,
            PrqStatus::Ready,
            PrqStatus::Merging,
            PrqStatus::Merged,
            PrqStatus::NeedsHuman,
            PrqStatus::Closed,
        ];
        for s in all {
            assert_eq!(PrqStatus::parse(s.as_str()), Some(s), "{}", s.as_str());
        }
        assert_eq!(PrqStatus::parse("nope"), None);
        // Settled = the queue stops acting until something external changes.
        assert!(PrqStatus::Merged.is_settled());
        assert!(PrqStatus::Closed.is_settled());
        assert!(PrqStatus::NeedsHuman.is_settled());
        assert!(!PrqStatus::BlockedCi.is_settled());
        assert!(!PrqStatus::Ready.is_settled());
    }

    #[test]
    fn each_blocker_maps_to_a_status_and_a_wire_word() {
        for (b, st, word) in [
            (Blocker::None, PrqStatus::Ready, "none"),
            (Blocker::Draft, PrqStatus::Watching, "draft"),
            (Blocker::Ci(vec![]), PrqStatus::BlockedCi, "ci"),
            (
                Blocker::ChecksPending,
                PrqStatus::Watching,
                "checks_pending",
            ),
            (Blocker::Conflict, PrqStatus::BlockedConflict, "conflict"),
            (
                Blocker::ChangesRequested,
                PrqStatus::BlockedReview,
                "changes_requested",
            ),
            (
                Blocker::AwaitingReview,
                PrqStatus::BlockedReview,
                "awaiting_review",
            ),
            (Blocker::Closed, PrqStatus::Closed, "closed"),
        ] {
            assert_eq!(PrqStatus::for_blocker(&b), st, "{b:?}");
            assert_eq!(b.as_str(), word);
        }
    }

    #[test]
    fn only_fixable_blockers_carry_a_task_kind_and_watch_class() {
        for b in [
            Blocker::None,
            Blocker::Draft,
            Blocker::ChecksPending,
            Blocker::AwaitingReview,
            Blocker::Closed,
        ] {
            assert_eq!(b.task_kind(), None, "{b:?}");
            assert_eq!(b.watch_kind(), None, "{b:?}");
        }
        assert_eq!(Blocker::Conflict.watch_kind(), Some(PrWatchKind::Conflict));
        assert_eq!(Blocker::Ci(vec![]).task_kind(), Some(TaskKind::PrCiFailure));
    }

    #[test]
    fn every_dispatched_task_kind_is_a_pr_kind() {
        // A merge-queue kind here would hand the agent "do NOT push", which is
        // exactly backwards for a pull request.
        for b in [
            Blocker::Ci(vec![]),
            Blocker::Conflict,
            Blocker::ChangesRequested,
        ] {
            assert!(b.task_kind().unwrap().is_pr(), "{b:?}");
        }
    }

    #[test]
    fn merge_method_default_is_squash() {
        assert_eq!(PrQueueConfig::default().merge_method, PrMergeMethod::Squash);
    }

    #[test]
    fn pr_identity_attestation_requires_matching_bound_repository_and_number() {
        let bound = "https://github.com/acme/repo/pull/7";
        let actual = "https://github.com/acme/repo/pull/7";
        assert_eq!(attest_pr_identity(Some(bound), 7, actual, 7), Ok(()));
        for (binding, url, number, expected) in [
            (None, actual, 7, "unbound"),
            (Some("not a URL"), actual, 7, "invalid"),
            (
                Some("https://github.com/acme/repo/pull/8"),
                actual,
                7,
                "pull request #7",
            ),
            (
                Some("https://github.com/acme/repo/issues/7"),
                actual,
                7,
                "pull request #7",
            ),
            (Some(bound), actual, 8, "differs"),
            (
                Some(bound),
                "https://github.com/acme/repo/pull/8",
                7,
                "pull request #7",
            ),
            (
                Some(bound),
                "https://github.com/acme/repo/issues/7",
                7,
                "pull request #7",
            ),
            (Some(bound), "bad URL", 7, "invalid fetched"),
            (
                Some(bound),
                "https://github.com/other/repo/pull/7",
                7,
                "other/repo",
            ),
            (
                Some(bound),
                "https://gitlab.com/acme/repo/pull/7",
                7,
                "gitlab.com",
            ),
        ] {
            let message = attest_pr_identity(binding, 7, url, number).unwrap_err();
            assert!(message.contains(expected), "{message}");
            assert!(
                message.contains("thegn pr queue rebind --pr 7"),
                "{message}"
            );
        }
    }

    #[test]
    fn attest_queue_row_table() {
        let row = crate::db::PrQueueRow {
            key: "root#github:github.com/acme/repo#7".into(),
            repo_root: "root".into(),
            number: 7,
            worktree: Some("/wt".into()),
            branch: "feature".into(),
            base_branch: "main".into(),
            forge: "github".into(),
            pr_url: Some("https://github.com/acme/repo/pull/7".into()),
            forge_host: Some("github.com".into()),
            project: Some("acme/repo".into()),
            checkout_gen: 4,
            status: "watching".into(),
            blocker: None,
            detail: None,
            agent_attempts: 1,
            ci_rerun_head: None,
            last_head_oid: None,
            queued_at: 0,
            updated_at: 0,
        };
        let snapshot = QueueRowSnapshot {
            number: row.number,
            key: row.key.clone(),
            pr_url: row.pr_url.clone(),
            forge: row.forge.clone(),
            worktree: row.worktree.clone(),
            branch: row.branch.clone(),
            checkout_gen: row.checkout_gen,
        };
        assert_eq!(attest_queue_row(&snapshot, Some(&row)), Ok(()));
        assert!(attest_queue_row(&snapshot, None).is_err());
        for changed in [
            crate::db::PrQueueRow {
                checkout_gen: 5,
                ..row.clone()
            },
            crate::db::PrQueueRow {
                worktree: Some("/other".into()),
                ..row.clone()
            },
            crate::db::PrQueueRow {
                pr_url: Some("https://github.com/else/repo/pull/7".into()),
                ..row.clone()
            },
            crate::db::PrQueueRow {
                forge: "gitlab".into(),
                ..row.clone()
            },
            crate::db::PrQueueRow {
                status: "merged".into(),
                ..row.clone()
            },
        ] {
            assert!(attest_queue_row(&snapshot, Some(&changed)).is_err());
        }
    }

    #[test]
    fn own_agent_head_requires_a_pushed_descendant_equal_to_the_forge_head() {
        let a = "a".repeat(40);
        let b = "b".repeat(40);
        let c = "c".repeat(64);
        assert_eq!(
            own_agent_head(&a, Some(&b), Some(&b), Some(true)),
            Some(b.clone())
        );
        assert_eq!(
            own_agent_head(&a, Some(&c), Some(&c), Some(true)),
            Some(c.clone())
        );
        for candidate in [
            own_agent_head(&a, Some(&b), Some(&c), Some(true)),
            own_agent_head(&a, Some(&b), None, Some(true)),
            own_agent_head(&a, Some(&a), Some(&a), Some(true)),
            own_agent_head(&a, Some(&b), Some(&b), Some(false)),
            own_agent_head(&a, Some(&b), Some(&b), None),
            own_agent_head(&a, Some("z".repeat(40).as_str()), Some(&b), Some(true)),
            own_agent_head(&a, Some(&"b".repeat(39)), Some(&"b".repeat(39)), Some(true)),
            own_agent_head("", Some(&b), Some(&b), Some(true)),
        ] {
            assert_eq!(candidate, None);
        }
    }
}
