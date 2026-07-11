//! The attention model: "what needs the user next?"
//!
//! Every worktree gets a tiered [`AttentionScore`] derived from the signals the
//! app already tracks — the activity FSM ([`crate::activity`]), unread
//! notifications, the PR cache, CI runs, and the merge queue. The score drives
//! the sidebar's Attention sort, the jump-to-next-attention action, and the
//! statusbar "needs you" chip.
//!
//! Everything here is pure: the host maps model/DB state into
//! [`AttentionInputs`] and this module does all the branching, so the tier
//! table lives in one tested place. Ordering churn is tamed by
//! [`stable_order`]: a fresh sort is only adopted when a worktree's *tier*
//! changed (or membership changed) — timestamp ticks and cache refreshes never
//! reshuffle rows.

use serde::{Deserialize, Serialize};

use crate::notification::NotificationKind;

/// Urgency tier, most urgent first (`Ord`: `Blocked < Failure < …`, so
/// ascending sorts put the most urgent first).
///
/// | tier | meaning |
/// |------|---------|
/// | `Blocked` | a process is stopped waiting on the user (agent asked for input, queue needs a human) |
/// | `Failure` | something failed and needs a decision (agent/test/CI failures, conflicts, changes requested) |
/// | `Waiting` | work finished and awaits the user (agent went idle, agent done) |
/// | `Ready`   | one-keystroke wins (PR green+approved, queue entry ready to land) |
/// | `Working` | in progress, no action needed (agent busy, CI running, queue folding) |
/// | `Idle`    | nothing pending |
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
pub enum AttentionTier {
    Blocked,
    Failure,
    Waiting,
    Ready,
    Working,
    #[default]
    Idle,
}

/// Why a worktree carries its tier — the single most urgent signal, surfaced
/// as the row's reason hint and the chip drill-down line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AttentionReason {
    // Blocked
    AgentNeedsInput,
    QueueNeedsHuman,
    // Failure
    AgentFailed,
    TestsFailed,
    ProcessFailed,
    LogError,
    CiFailed,
    PrConflict,
    ChangesRequested,
    GateFailed,
    Deferred,
    // Waiting
    AgentWaiting,
    AgentDone,
    StillStuck,
    // Ready
    ReadyToLand,
    QueueReady,
    // Working
    AgentWorking,
    Building,
    CiRunning,
    Integrating,
    // Idle
    Idle,
}

impl AttentionReason {
    /// Short human label for the row detail line / drill-down.
    pub fn label(self) -> &'static str {
        match self {
            Self::AgentNeedsInput => "agent needs input",
            Self::QueueNeedsHuman => "merge queue needs you",
            Self::AgentFailed => "agent failed",
            Self::TestsFailed => "tests failed",
            Self::ProcessFailed => "process failed",
            Self::LogError => "log errors",
            Self::CiFailed => "CI failed",
            Self::PrConflict => "PR has conflicts",
            Self::ChangesRequested => "changes requested",
            Self::GateFailed => "merge gate failed",
            Self::Deferred => "merge deferred",
            Self::AgentWaiting => "agent finished",
            Self::AgentDone => "agent done",
            Self::StillStuck => "still waiting",
            Self::ReadyToLand => "ready to land",
            Self::QueueReady => "queued, ready",
            Self::AgentWorking => "working",
            Self::Building => "setting up",
            Self::CiRunning => "CI running",
            Self::Integrating => "integrating",
            Self::Idle => "idle",
        }
    }
}

/// A worktree's resolved urgency: tier, a within-tier sub-rank (signal
/// precedence), the winning reason, and when the signal started (unix seconds)
/// — `None` when the source only has fetch-time, in which case ordering falls
/// back to home-first/manual position downstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttentionScore {
    pub tier: AttentionTier,
    pub sub: u8,
    pub reason: AttentionReason,
    pub since: Option<i64>,
}

impl Default for AttentionScore {
    fn default() -> Self {
        Self {
            tier: AttentionTier::Idle,
            sub: u8::MAX,
            reason: AttentionReason::Idle,
            since: None,
        }
    }
}

/// A user's acknowledgement of a worktree's needs-you signal: the exact
/// `(reason, since)` that was showing when they quieted it. Persisted per
/// worktree so the "Needs you" nag stays silenced across restarts — but only
/// for *that episode* (see [`AttentionScore::is_acked_by`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttentionAck {
    pub reason: AttentionReason,
    pub since: Option<i64>,
}

impl AttentionScore {
    /// Does this worktree need the user (tier ≤ `Waiting`)? Drives the chip
    /// count and the jump-to-next set.
    pub fn needs_user(&self) -> bool {
        self.tier <= AttentionTier::Waiting
    }

    /// Is this score still covered by a prior acknowledgement? True only when
    /// the reason **and** episode (`since`) are identical to the ack. A new
    /// episode (advanced `since`) or a changed reason re-fires — so acking
    /// "still waiting" quiets *this* wait, but a fresh failure later re-nags.
    pub fn is_acked_by(&self, ack: &AttentionAck) -> bool {
        self.reason == ack.reason && self.since == ack.since
    }

    /// Ascending sort key: `(tier, sub, since)` — most urgent tier first, then
    /// signal precedence, then **longest-waiting first** (oldest `since`;
    /// signals without a real timestamp sort after timestamped peers).
    pub fn sort_key(&self) -> (u8, u8, i64) {
        (self.tier as u8, self.sub, self.since.unwrap_or(i64::MAX))
    }
}

/// What the activity FSM says about the worktree (see [`crate::activity`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ActivityKind {
    #[default]
    None,
    /// CPU advancing — an agent/build is working.
    Active,
    /// Worktree mid-creation/provisioning.
    Loading,
    /// Went idle after activity; the user hasn't looked yet (filled red dot).
    Waiting,
    /// Seen but still idle (hollow red dot).
    Read,
}

/// One unread notification relevant to the worktree: `(kind, created_at)` in
/// unix seconds (the DB's `created_at_ms` misnomer — it holds seconds).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnreadNote {
    pub kind: NotificationKind,
    pub at: i64,
}

/// The attention-relevant facts of an **open** PR (callers skip closed/merged).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PrFacts {
    pub is_draft: bool,
    /// `mergeable == "CONFLICTING"`.
    pub conflicting: bool,
    /// `mergeable == "MERGEABLE"`.
    pub mergeable: bool,
    /// `review_decision == "CHANGES_REQUESTED"`.
    pub changes_requested: bool,
    /// `review_decision == "APPROVED"`.
    pub approved: bool,
    pub checks_failed: u32,
    pub checks_pending: u32,
    pub checks_total: u32,
}

impl PrFacts {
    /// Derive the facts from a cached [`crate::github::PrStatus`]. Returns
    /// `None` for non-open PRs — a merged/closed PR carries no attention.
    pub fn from_status(pr: &crate::github::PrStatus) -> Option<Self> {
        if !pr.state.eq_ignore_ascii_case("open") {
            return None;
        }
        let decision = pr.review_decision.as_deref().unwrap_or("");
        Some(Self {
            is_draft: pr.is_draft,
            conflicting: pr.mergeable.eq_ignore_ascii_case("conflicting"),
            mergeable: pr.mergeable.eq_ignore_ascii_case("mergeable"),
            changes_requested: decision.eq_ignore_ascii_case("changes_requested"),
            approved: decision.eq_ignore_ascii_case("approved"),
            checks_failed: pr.checks.failed,
            checks_pending: pr.checks.pending,
            checks_total: pr.checks.total,
        })
    }

    /// Green board: every check settled and passing (and there is at least one).
    fn checks_green(&self) -> bool {
        self.checks_total > 0 && self.checks_failed == 0 && self.checks_pending == 0
    }
}

/// A merge-queue entry's status vocabulary (see `merge_driver`). Persisted as
/// strings in the `merge_queue` table; [`MqStatus::parse`] is the one decoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MqStatus {
    Queued,
    Folding,
    Verifying,
    Landed,
    Ready,
    Deferred,
    GateFailed,
    AgentRunning,
    NeedsHuman,
}

impl MqStatus {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "queued" => Self::Queued,
            "folding" => Self::Folding,
            "verifying" => Self::Verifying,
            "landed" => Self::Landed,
            "ready" => Self::Ready,
            "deferred" => Self::Deferred,
            "gate_failed" => Self::GateFailed,
            "agent_running" => Self::AgentRunning,
            "needs_human" => Self::NeedsHuman,
            _ => return None,
        })
    }
}

/// The worktree's merge-queue entry: status + when it last changed (real
/// event time from the `updated_at` column).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MqFacts {
    pub status: MqStatus,
    pub updated_at: i64,
}

/// Everything [`score`] looks at for one worktree. The host fills this from
/// model/DB state; defaults are all "no signal".
#[derive(Debug, Clone, Default)]
pub struct AttentionInputs {
    pub activity: ActivityKind,
    /// When the activity state started (unix seconds): `quiet_since` for
    /// waiting/read, `busy_since` for active.
    pub activity_since: Option<i64>,
    /// Unread notifications scoped to this worktree.
    pub unread: Vec<UnreadNote>,
    /// The branch's open PR, if any is cached.
    pub pr: Option<PrFacts>,
    /// Latest cached CI run (outside any PR rollup) is failing / running.
    pub ci_failing: bool,
    pub ci_running: bool,
    /// The worktree's merge-queue entry, if any (callers skip `landed`).
    pub merge_queue: Option<MqFacts>,
    /// Uncommitted changes — never a tier of its own, only an idle sub-rank.
    pub dirty: bool,
}

/// Score one worktree: evaluate every signal, keep the most urgent
/// (min by `(tier, sub)`; the winner's timestamp becomes `since`).
pub fn score(inputs: &AttentionInputs) -> AttentionScore {
    use AttentionReason as R;
    use AttentionTier as T;

    let mut best: Option<AttentionScore> = None;
    let mut consider = |tier: T, sub: u8, reason: R, since: Option<i64>| {
        let cand = AttentionScore {
            tier,
            sub,
            reason,
            since,
        };
        if best.is_none_or(|b| (cand.tier, cand.sub) < (b.tier, b.sub)) {
            best = Some(cand);
        }
    };

    for n in &inputs.unread {
        let at = Some(n.at);
        match n.kind {
            NotificationKind::AgentAttention => consider(T::Blocked, 0, R::AgentNeedsInput, at),
            NotificationKind::AgentFailed => consider(T::Failure, 0, R::AgentFailed, at),
            NotificationKind::TestFailed => consider(T::Failure, 1, R::TestsFailed, at),
            NotificationKind::ProcessFailed => consider(T::Failure, 2, R::ProcessFailed, at),
            // LogError (szhost's own diagnostics) is deliberately quiet — it never
            // scores into any tier, so it can't put a worktree in "Needs you".
            NotificationKind::AgentDone => consider(T::Waiting, 1, R::AgentDone, at),
            _ => {}
        }
    }

    if let Some(mq) = inputs.merge_queue {
        let at = Some(mq.updated_at);
        match mq.status {
            MqStatus::NeedsHuman => consider(T::Blocked, 1, R::QueueNeedsHuman, at),
            MqStatus::GateFailed => consider(T::Failure, 7, R::GateFailed, at),
            MqStatus::Deferred => consider(T::Failure, 8, R::Deferred, at),
            MqStatus::Ready => consider(T::Ready, 1, R::QueueReady, at),
            MqStatus::Queued | MqStatus::Folding | MqStatus::Verifying | MqStatus::AgentRunning => {
                consider(T::Working, 3, R::Integrating, at)
            }
            MqStatus::Landed => {}
        }
    }

    if let Some(pr) = inputs.pr {
        // PR times are fetch-time only — no honest `since`.
        if pr.checks_failed > 0 {
            consider(T::Failure, 4, R::CiFailed, None);
        }
        if pr.conflicting {
            consider(T::Failure, 5, R::PrConflict, None);
        }
        if pr.changes_requested {
            consider(T::Failure, 6, R::ChangesRequested, None);
        }
        if !pr.is_draft && pr.approved && pr.mergeable && pr.checks_green() {
            consider(T::Ready, 0, R::ReadyToLand, None);
        }
        if pr.checks_pending > 0 {
            consider(T::Working, 2, R::CiRunning, None);
        }
    }
    if inputs.ci_failing {
        consider(T::Failure, 4, R::CiFailed, None);
    }
    if inputs.ci_running {
        consider(T::Working, 2, R::CiRunning, None);
    }

    match inputs.activity {
        ActivityKind::Waiting => consider(T::Waiting, 0, R::AgentWaiting, inputs.activity_since),
        // Seen-but-stuck sub-ranks below every unread Waiting signal.
        ActivityKind::Read => consider(T::Waiting, 2, R::StillStuck, inputs.activity_since),
        ActivityKind::Active => consider(T::Working, 0, R::AgentWorking, inputs.activity_since),
        ActivityKind::Loading => consider(T::Working, 1, R::Building, None),
        ActivityKind::None => {}
    }

    best.unwrap_or(AttentionScore {
        tier: T::Idle,
        // Dirty is a within-idle nudge, never a tier.
        sub: if inputs.dirty { 0 } else { 1 },
        reason: R::Idle,
        since: None,
    })
}

/// Hysteresis: adopt `fresh` (already in the desired order) only when it
/// *materially* differs from `prev` — a membership change or a tier change on
/// some worktree. Equal-tier movement (timestamp ticks, sub-rank churn,
/// fetch-time drift) keeps the previous order verbatim, so rows never dance
/// while the user is looking.
pub fn stable_order(
    prev: &[(String, AttentionTier)],
    fresh: &[(String, AttentionScore)],
) -> Vec<String> {
    let same_membership = prev.len() == fresh.len()
        && fresh
            .iter()
            .all(|(p, s)| prev.iter().any(|(q, t)| q == p && *t == s.tier));
    if same_membership {
        prev.iter().map(|(p, _)| p.clone()).collect()
    } else {
        fresh.iter().map(|(p, _)| p.clone()).collect()
    }
}

/// The next worktree needing the user after `current`, in `ordered` (already
/// urgency-sorted, `needs_user`-filtered) order — wraps, so repeated jumps
/// cycle. `current` absent (or `None`) lands on the most urgent.
pub fn next_attention<'a>(
    ordered: &'a [(String, AttentionScore)],
    current: Option<&str>,
) -> Option<&'a str> {
    if ordered.is_empty() {
        return None;
    }
    let cur_ix = current.and_then(|c| ordered.iter().position(|(p, _)| p == c));
    let ix = match cur_ix {
        Some(i) => (i + 1) % ordered.len(),
        None => 0,
    };
    Some(ordered[ix].0.as_str())
}

/// A workspace's rollup: its most urgent worktree's score (min by sort key).
/// `None` over an empty iterator.
pub fn rollup<'a>(scores: impl IntoIterator<Item = &'a AttentionScore>) -> Option<AttentionScore> {
    scores.into_iter().copied().min_by_key(|s| s.sort_key())
}

#[cfg(test)]
mod tests {
    use super::*;
    use AttentionReason as R;
    use AttentionTier as T;

    fn note(kind: NotificationKind, at: i64) -> UnreadNote {
        UnreadNote { kind, at }
    }

    #[test]
    fn idle_by_default_dirty_is_only_a_sub_rank() {
        let idle = score(&AttentionInputs::default());
        assert_eq!((idle.tier, idle.reason), (T::Idle, R::Idle));
        assert!(!idle.needs_user());

        let dirty = score(&AttentionInputs {
            dirty: true,
            ..Default::default()
        });
        assert_eq!(dirty.tier, T::Idle, "dirty alone must not raise a tier");
        assert!(
            dirty.sort_key() < idle.sort_key(),
            "…but nudges within idle"
        );
    }

    #[test]
    fn tier_precedence_blocked_beats_everything() {
        let s = score(&AttentionInputs {
            activity: ActivityKind::Active,
            unread: vec![
                note(NotificationKind::TestFailed, 50),
                note(NotificationKind::AgentAttention, 100),
            ],
            pr: Some(PrFacts {
                checks_failed: 3,
                ..Default::default()
            }),
            merge_queue: Some(MqFacts {
                status: MqStatus::GateFailed,
                updated_at: 10,
            }),
            ..Default::default()
        });
        assert_eq!((s.tier, s.reason), (T::Blocked, R::AgentNeedsInput));
        assert_eq!(s.since, Some(100));
        assert!(s.needs_user());
    }

    #[test]
    fn failure_signals_rank_by_sub() {
        // Agent failure outranks a CI failure which outranks a deferred fold.
        let agent = score(&AttentionInputs {
            unread: vec![note(NotificationKind::AgentFailed, 5)],
            ..Default::default()
        });
        let ci = score(&AttentionInputs {
            pr: Some(PrFacts {
                checks_failed: 1,
                ..Default::default()
            }),
            ..Default::default()
        });
        let deferred = score(&AttentionInputs {
            merge_queue: Some(MqFacts {
                status: MqStatus::Deferred,
                updated_at: 5,
            }),
            ..Default::default()
        });
        assert_eq!(agent.tier, T::Failure);
        assert_eq!(ci.tier, T::Failure);
        assert_eq!(deferred.tier, T::Failure);
        assert!(agent.sort_key() < ci.sort_key());
        assert!(ci.sort_key() < deferred.sort_key());
        assert_eq!(ci.reason, R::CiFailed);
        assert_eq!(deferred.reason, R::Deferred);
    }

    #[test]
    fn every_failure_notification_kind_lands_in_failure() {
        for (kind, reason) in [
            (NotificationKind::AgentFailed, R::AgentFailed),
            (NotificationKind::TestFailed, R::TestsFailed),
            (NotificationKind::ProcessFailed, R::ProcessFailed),
        ] {
            let s = score(&AttentionInputs {
                unread: vec![note(kind, 42)],
                ..Default::default()
            });
            assert_eq!((s.tier, s.reason, s.since), (T::Failure, reason, Some(42)));
        }
        // Non-attention kinds (assigned, mentioned, …) don't score at all.
        let s = score(&AttentionInputs {
            unread: vec![note(NotificationKind::Assigned, 42)],
            ..Default::default()
        });
        assert_eq!(s.tier, T::Idle);
    }

    #[test]
    fn log_error_is_quiet_and_never_needs_user() {
        // szhost's own log errors are diagnostics, not user attention: a LogError
        // must never score into a tier, so it can't drag a worktree into
        // "Needs you".
        let s = score(&AttentionInputs {
            unread: vec![note(NotificationKind::LogError, 42)],
            ..Default::default()
        });
        assert_eq!(s.tier, T::Idle);
        assert!(!s.needs_user());
    }

    #[test]
    fn waiting_read_sub_ranks_below_unread() {
        let waiting = score(&AttentionInputs {
            activity: ActivityKind::Waiting,
            activity_since: Some(500),
            ..Default::default()
        });
        assert_eq!(
            (waiting.tier, waiting.reason),
            (T::Waiting, R::AgentWaiting)
        );
        assert_eq!(waiting.since, Some(500));
        assert!(waiting.needs_user());

        let done = score(&AttentionInputs {
            unread: vec![note(NotificationKind::AgentDone, 500)],
            ..Default::default()
        });
        let read = score(&AttentionInputs {
            activity: ActivityKind::Read,
            activity_since: Some(1),
            ..Default::default()
        });
        assert_eq!(read.reason, R::StillStuck);
        assert!(waiting.sort_key() < done.sort_key());
        assert!(
            done.sort_key() < read.sort_key(),
            "seen-but-stuck ranks below unread signals even when older"
        );
    }

    #[test]
    fn ready_to_land_requires_green_approved_mergeable_nondraft() {
        let ready = PrFacts {
            approved: true,
            mergeable: true,
            checks_total: 4,
            ..Default::default()
        };
        let s = score(&AttentionInputs {
            pr: Some(ready),
            ..Default::default()
        });
        assert_eq!((s.tier, s.reason), (T::Ready, R::ReadyToLand));
        assert!(!s.needs_user(), "ready is a win, not a demand");

        for spoiled in [
            PrFacts {
                is_draft: true,
                ..ready
            },
            PrFacts {
                approved: false,
                ..ready
            },
            PrFacts {
                mergeable: false,
                ..ready
            },
            PrFacts {
                checks_pending: 1,
                ..ready
            },
            PrFacts {
                checks_total: 0,
                ..ready
            },
        ] {
            let s = score(&AttentionInputs {
                pr: Some(spoiled),
                ..Default::default()
            });
            assert_ne!(s.tier, T::Ready, "{spoiled:?} must not be ready");
        }
    }

    #[test]
    fn queue_ready_and_conflict_and_changes_requested() {
        let s = score(&AttentionInputs {
            merge_queue: Some(MqFacts {
                status: MqStatus::Ready,
                updated_at: 9,
            }),
            ..Default::default()
        });
        assert_eq!(
            (s.tier, s.reason, s.since),
            (T::Ready, R::QueueReady, Some(9))
        );

        let s = score(&AttentionInputs {
            pr: Some(PrFacts {
                conflicting: true,
                ..Default::default()
            }),
            ..Default::default()
        });
        assert_eq!((s.tier, s.reason), (T::Failure, R::PrConflict));

        let s = score(&AttentionInputs {
            pr: Some(PrFacts {
                changes_requested: true,
                ..Default::default()
            }),
            ..Default::default()
        });
        assert_eq!((s.tier, s.reason), (T::Failure, R::ChangesRequested));
    }

    #[test]
    fn working_tier_from_activity_ci_and_queue() {
        let active = score(&AttentionInputs {
            activity: ActivityKind::Active,
            activity_since: Some(7),
            ..Default::default()
        });
        assert_eq!((active.tier, active.reason), (T::Working, R::AgentWorking));
        assert!(!active.needs_user());

        let loading = score(&AttentionInputs {
            activity: ActivityKind::Loading,
            ..Default::default()
        });
        assert_eq!(loading.reason, R::Building);

        let ci = score(&AttentionInputs {
            pr: Some(PrFacts {
                checks_pending: 2,
                checks_total: 2,
                ..Default::default()
            }),
            ..Default::default()
        });
        assert_eq!(ci.reason, R::CiRunning);
        let ci2 = score(&AttentionInputs {
            ci_running: true,
            ..Default::default()
        });
        assert_eq!(ci2.reason, R::CiRunning);

        for st in [
            MqStatus::Queued,
            MqStatus::Folding,
            MqStatus::Verifying,
            MqStatus::AgentRunning,
        ] {
            let s = score(&AttentionInputs {
                merge_queue: Some(MqFacts {
                    status: st,
                    updated_at: 1,
                }),
                ..Default::default()
            });
            assert_eq!((s.tier, s.reason), (T::Working, R::Integrating), "{st:?}");
        }
        // A landed row carries no attention.
        let s = score(&AttentionInputs {
            merge_queue: Some(MqFacts {
                status: MqStatus::Landed,
                updated_at: 1,
            }),
            ..Default::default()
        });
        assert_eq!(s.tier, T::Idle);
    }

    #[test]
    fn ci_failing_cache_scores_like_pr_check_failure() {
        let s = score(&AttentionInputs {
            ci_failing: true,
            ..Default::default()
        });
        assert_eq!((s.tier, s.reason), (T::Failure, R::CiFailed));
    }

    #[test]
    fn longest_waiting_first_within_tier() {
        let old = score(&AttentionInputs {
            unread: vec![note(NotificationKind::AgentAttention, 100)],
            ..Default::default()
        });
        let new = score(&AttentionInputs {
            unread: vec![note(NotificationKind::AgentAttention, 900)],
            ..Default::default()
        });
        assert!(old.sort_key() < new.sort_key(), "older waits sort first");
        // No timestamp sorts after timestamped peers of the same (tier, sub).
        let no_ts = AttentionScore { since: None, ..old };
        assert!(old.sort_key() < no_ts.sort_key());
    }

    #[test]
    fn pr_facts_from_status() {
        let mut pr: crate::github::PrStatus = serde_json::from_str(
            r#"{"number":7,"title":"t","state":"OPEN","url":"u","isDraft":false,
                "mergeable":"MERGEABLE","mergeStateStatus":"CLEAN",
                "reviewDecision":"APPROVED","statusCheckRollup":[
                  {"name":"ci","status":"COMPLETED","conclusion":"SUCCESS"}]}"#,
        )
        .unwrap();
        pr.recompute_checks();
        let f = PrFacts::from_status(&pr).unwrap();
        assert!(f.approved && f.mergeable && !f.conflicting && !f.changes_requested);
        assert!(f.checks_green());
        assert_eq!((f.checks_failed, f.checks_total), (0, 1));

        pr.state = "MERGED".into();
        assert!(PrFacts::from_status(&pr).is_none(), "non-open PRs drop out");

        pr.state = "OPEN".into();
        pr.mergeable = "CONFLICTING".into();
        pr.review_decision = Some("CHANGES_REQUESTED".into());
        let f = PrFacts::from_status(&pr).unwrap();
        assert!(f.conflicting && f.changes_requested && !f.mergeable && !f.approved);
    }

    #[test]
    fn mq_status_parses_full_vocabulary() {
        for (s, v) in [
            ("queued", MqStatus::Queued),
            ("folding", MqStatus::Folding),
            ("verifying", MqStatus::Verifying),
            ("landed", MqStatus::Landed),
            ("ready", MqStatus::Ready),
            ("deferred", MqStatus::Deferred),
            ("gate_failed", MqStatus::GateFailed),
            ("agent_running", MqStatus::AgentRunning),
            ("needs_human", MqStatus::NeedsHuman),
        ] {
            assert_eq!(MqStatus::parse(s), Some(v));
        }
        assert_eq!(MqStatus::parse("bogus"), None);
        // needs_human blocks.
        let s = score(&AttentionInputs {
            merge_queue: Some(MqFacts {
                status: MqStatus::NeedsHuman,
                updated_at: 3,
            }),
            ..Default::default()
        });
        assert_eq!((s.tier, s.reason), (T::Blocked, R::QueueNeedsHuman));
    }

    fn sc(tier: AttentionTier, since: Option<i64>) -> AttentionScore {
        AttentionScore {
            tier,
            sub: 0,
            reason: R::Idle,
            since,
        }
    }

    #[test]
    fn stable_order_holds_on_equal_tiers_resorts_on_change() {
        let prev = vec![("a".to_string(), T::Waiting), ("b".to_string(), T::Idle)];
        // Same tiers, different fresh order (a timestamp moved): keep prev.
        let fresh = vec![
            ("b".to_string(), sc(T::Idle, Some(1))),
            ("a".to_string(), sc(T::Waiting, Some(2))),
        ];
        assert_eq!(stable_order(&prev, &fresh), vec!["a", "b"]);

        // A tier changed: adopt fresh.
        let fresh = vec![
            ("b".to_string(), sc(T::Blocked, None)),
            ("a".to_string(), sc(T::Waiting, None)),
        ];
        assert_eq!(stable_order(&prev, &fresh), vec!["b", "a"]);

        // Membership changed: adopt fresh.
        let fresh = vec![
            ("a".to_string(), sc(T::Waiting, None)),
            ("c".to_string(), sc(T::Idle, None)),
        ];
        assert_eq!(stable_order(&prev, &fresh), vec!["a", "c"]);

        // Empty prev (first hydration): adopt fresh.
        let fresh = vec![("x".to_string(), sc(T::Idle, None))];
        assert_eq!(stable_order(&[], &fresh), vec!["x"]);
    }

    #[test]
    fn next_attention_cycles_and_handles_edges() {
        assert_eq!(next_attention(&[], None), None);
        let ordered = vec![
            ("a".to_string(), sc(T::Blocked, None)),
            ("b".to_string(), sc(T::Failure, None)),
            ("c".to_string(), sc(T::Waiting, None)),
        ];
        // No current (or current not in the set): most urgent first.
        assert_eq!(next_attention(&ordered, None), Some("a"));
        assert_eq!(next_attention(&ordered, Some("zzz")), Some("a"));
        // From a member: the next one, wrapping.
        assert_eq!(next_attention(&ordered, Some("a")), Some("b"));
        assert_eq!(next_attention(&ordered, Some("c")), Some("a"));
    }

    #[test]
    fn rollup_picks_most_urgent() {
        assert_eq!(rollup(std::iter::empty()), None);
        let scores = [
            sc(T::Idle, None),
            sc(T::Failure, Some(50)),
            sc(T::Failure, Some(10)),
            sc(T::Working, None),
        ];
        let r = rollup(scores.iter()).unwrap();
        assert_eq!((r.tier, r.since), (T::Failure, Some(10)));
    }

    #[test]
    fn labels_are_short_and_non_empty() {
        for r in [
            R::AgentNeedsInput,
            R::QueueNeedsHuman,
            R::AgentFailed,
            R::TestsFailed,
            R::ProcessFailed,
            R::LogError,
            R::CiFailed,
            R::PrConflict,
            R::ChangesRequested,
            R::GateFailed,
            R::Deferred,
            R::AgentWaiting,
            R::AgentDone,
            R::StillStuck,
            R::ReadyToLand,
            R::QueueReady,
            R::AgentWorking,
            R::Building,
            R::CiRunning,
            R::Integrating,
            R::Idle,
        ] {
            assert!(!r.label().is_empty());
            assert!(
                r.label().len() <= 24,
                "{r:?} label too long for a detail line"
            );
        }
    }

    #[test]
    fn ack_covers_same_episode_but_refires_on_change() {
        let s = AttentionScore {
            tier: T::Waiting,
            sub: 2,
            reason: R::StillStuck,
            since: Some(500),
        };
        // Exact (reason, since) match → acked.
        let ack = AttentionAck {
            reason: R::StillStuck,
            since: Some(500),
        };
        assert!(s.is_acked_by(&ack));
        // A new wait episode (advanced `since`) re-fires.
        let newer = AttentionScore {
            since: Some(900),
            ..s
        };
        assert!(!newer.is_acked_by(&ack));
        // A different reason re-fires even at the same timestamp.
        let other = AttentionScore {
            reason: R::CiFailed,
            ..s
        };
        assert!(!other.is_acked_by(&ack));
        // Timestamp-less signals (CI/PR) match on `None == None`.
        let ci = AttentionScore {
            tier: T::Failure,
            sub: 4,
            reason: R::CiFailed,
            since: None,
        };
        let ci_ack = AttentionAck {
            reason: R::CiFailed,
            since: None,
        };
        assert!(ci.is_acked_by(&ci_ack));
    }

    #[test]
    fn default_score_is_idle_and_sorts_last() {
        let d = AttentionScore::default();
        assert_eq!(d.tier, T::Idle);
        assert!(!d.needs_user());
        assert!(score(&AttentionInputs::default()).sort_key() < d.sort_key());
    }
}
