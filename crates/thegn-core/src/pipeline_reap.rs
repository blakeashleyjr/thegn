//! Reap policy — deciding what an *active roster row whose worker is gone*
//! actually is.
//!
//! # The gap this closes
//!
//! A dispatch row's `status` says what a supervisor last recorded. It does not
//! say whether anyone is still working. When those two drift apart the roster
//! stops describing reality, and on 2026-08-29 that drift reached 121 rows: the
//! supervisor counted live daemon sessions for capacity, every finished worker
//! looked like a free slot, and the pipeline re-dispatched into rows that were
//! already done.
//!
//! Reconciling it by hand takes three joins a person has to remember to do:
//! the roster, the daemon's live sessions, and the filesystem/git state of each
//! row's artifact. During the 2026-08-30 drain the supervisor performed that
//! join roughly fifteen times, once per silently-dead worker. This module is
//! that join, written down.
//!
//! # What it does and does not decide
//!
//! It classifies. It does not act, and it deliberately cannot close a row on
//! its own: [`ReapVerdict::CloseDone`] is a *recommendation* the caller applies
//! through the normal gated `set-status`, and the one genuinely ambiguous case
//! ([`ReapVerdict::NeedsDecision`]) is handed back to a human rather than
//! guessed at. That boundary is the same "structure, not judgment" line the
//! rest of the pipeline surface holds.
//!
//! Pure: rows and facts in, verdicts out. No daemon, no filesystem, no clock
//! beyond the one the caller passes.

use crate::issue::AgentDispatch;

/// What the caller has already established about one row's world.
///
/// The two facts a roster row cannot answer about itself: whether its worker is
/// still alive, and whether its promised artifact actually exists in git.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReapFacts {
    /// The row's `session_id` is present in the daemon's live set.
    ///
    /// **A daemon restart makes every prior session absent**, so this being
    /// false does not prove the worker crashed — only that nothing is running
    /// under that id now. That is precisely why the artifact facts below are
    /// consulted before any row is called finished or failed.
    pub session_live: bool,
    /// The row's artifact exists under its worktree.
    pub artifact_exists: bool,
    /// git tracks that artifact — an uncommitted file is not a handoff.
    pub artifact_tracked: bool,
    /// The row carries a worker report.
    pub report_present: bool,
}

/// What the row should become.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReapVerdict {
    /// A worker is still running here. Leave it alone.
    Live,
    /// The row is already terminal; nothing to reap.
    Closed,
    /// Worker gone, artifact committed, report filed. The handoff is complete
    /// and `set-status done` will pass its gate unforced.
    CloseDone,
    /// Worker gone and the artifact was never committed. There is no handoff,
    /// so the stage did not complete. Safe to record as failed with a reason.
    MarkFailed {
        /// Operator-facing reason, suitable for the row's note verbatim.
        why: &'static str,
    },
    /// Worker gone, artifact committed, but **no report**. The work is real and
    /// in git, yet the contract the done-gate enforces was not completed.
    ///
    /// This is deliberately not auto-closed. Closing it means either forcing
    /// past the gate or fabricating a report, and both are decisions a person
    /// should make with the artifact in front of them. Reaping surfaces it;
    /// judgement stays outside.
    NeedsDecision {
        /// Why it cannot be closed automatically.
        why: &'static str,
    },
}

impl ReapVerdict {
    /// Whether this verdict asks the caller to change the row.
    pub fn is_actionable(&self) -> bool {
        matches!(self, Self::CloseDone | Self::MarkFailed { .. })
    }

    /// A short token for tables and `--json`.
    pub fn token(&self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Closed => "closed",
            Self::CloseDone => "close-done",
            Self::MarkFailed { .. } => "mark-failed",
            Self::NeedsDecision { .. } => "needs-decision",
        }
    }
}

/// One row's reap decision, paired with the id so a caller can act on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reap {
    pub id: i64,
    pub verdict: ReapVerdict,
}

/// Classify one row.
///
/// Order matters: a terminal row is never reaped, a live worker is never
/// touched, and only then do the artifact facts decide. `artifact_exists`
/// without `artifact_tracked` counts as *no handoff* — an uncommitted file is
/// exactly the state a worker leaves behind when it dies mid-stage, and it is
/// also what a worker leaves when its sandbox forbade the commit (THE-91), so
/// it must never read as success.
pub fn classify(row: &AgentDispatch, facts: &ReapFacts) -> ReapVerdict {
    if !row.status.is_active() {
        return ReapVerdict::Closed;
    }
    if facts.session_live {
        return ReapVerdict::Live;
    }
    match (facts.artifact_tracked, facts.report_present) {
        (true, true) => ReapVerdict::CloseDone,
        (true, false) => ReapVerdict::NeedsDecision {
            why: "worker is gone and its artifact IS committed, but no report was filed — \
                  read the artifact and close it yourself; auto-closing would mean forcing \
                  the gate or inventing a report",
        },
        (false, _) if facts.artifact_exists => ReapVerdict::MarkFailed {
            why: "worker is gone and its artifact exists but is NOT tracked by git — an \
                  uncommitted artifact is not a handoff (a worker whose sandbox forbade the \
                  commit leaves exactly this state; see THE-91)",
        },
        (false, _) => ReapVerdict::MarkFailed {
            why: "worker is gone and its artifact was never written — the stage produced \
                  nothing",
        },
    }
}

/// Classify a whole roster against the live session set.
///
/// `facts_for` supplies the per-row filesystem/git facts; the caller gathers
/// them because only the host can stat a worktree. Rows are returned in input
/// order so a caller can zip them back.
pub fn plan<F>(rows: &[AgentDispatch], mut facts_for: F) -> Vec<Reap>
where
    F: FnMut(&AgentDispatch) -> ReapFacts,
{
    rows.iter()
        .map(|r| Reap {
            id: r.id,
            verdict: classify(r, &facts_for(r)),
        })
        .collect()
}

/// Count each verdict kind — the one-line summary a supervisor reads first.
pub fn summarize(reaps: &[Reap]) -> ReapSummary {
    let mut s = ReapSummary::default();
    for r in reaps {
        match r.verdict {
            ReapVerdict::Live => s.live += 1,
            ReapVerdict::Closed => s.closed += 1,
            ReapVerdict::CloseDone => s.close_done += 1,
            ReapVerdict::MarkFailed { .. } => s.mark_failed += 1,
            ReapVerdict::NeedsDecision { .. } => s.needs_decision += 1,
        }
    }
    s
}

/// Verdict tallies.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReapSummary {
    pub live: usize,
    pub closed: usize,
    pub close_done: usize,
    pub mark_failed: usize,
    pub needs_decision: usize,
}

impl ReapSummary {
    /// Rows a reap would change.
    pub fn actionable(&self) -> usize {
        self.close_done + self.mark_failed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::issue::AgentDispatchStatus as S;

    fn row(id: i64, status: S) -> AgentDispatch {
        AgentDispatch {
            id,
            issue_id: "linear:THE-1".into(),
            worktree_path: "/wt/a".into(),
            agent_name: "coder".into(),
            dispatched_at_ms: id,
            status,
            stage: Some("code".into()),
            parent_id: None,
            session_id: Some(format!("s{id}")),
            artifact_path: Some("a.md".into()),
            note: None,
            chunk_path: None,
            report: None,
            exit_code: None,
            exited_at_ms: None,
        }
    }

    const GONE_COMMITTED_REPORTED: ReapFacts = ReapFacts {
        session_live: false,
        artifact_exists: true,
        artifact_tracked: true,
        report_present: true,
    };

    #[test]
    fn a_live_worker_is_never_reaped() {
        let f = ReapFacts {
            session_live: true,
            ..GONE_COMMITTED_REPORTED
        };
        assert_eq!(classify(&row(1, S::Running), &f), ReapVerdict::Live);
        assert!(!classify(&row(1, S::Running), &f).is_actionable());
    }

    #[test]
    fn a_terminal_row_is_never_reaped_however_dead_its_worker() {
        for st in [S::Done, S::Failed, S::Abandoned, S::Merged] {
            assert_eq!(
                classify(&row(1, st), &GONE_COMMITTED_REPORTED),
                ReapVerdict::Closed,
                "{st:?}"
            );
        }
    }

    #[test]
    fn gone_with_a_committed_artifact_and_a_report_closes_clean() {
        // The only case that may auto-close: the gate would pass unforced.
        assert_eq!(
            classify(&row(1, S::Running), &GONE_COMMITTED_REPORTED),
            ReapVerdict::CloseDone
        );
    }

    #[test]
    fn gone_with_a_committed_artifact_but_no_report_needs_a_human() {
        // The 2026-08-29 shape, and the one the supervisor hand-resolved ~15
        // times: real work in git, contract incomplete. Auto-closing means
        // forcing the gate or inventing a report — neither is the tool's call.
        let f = ReapFacts {
            report_present: false,
            ..GONE_COMMITTED_REPORTED
        };
        let v = classify(&row(1, S::Running), &f);
        assert!(matches!(v, ReapVerdict::NeedsDecision { .. }), "{v:?}");
        assert!(!v.is_actionable(), "a human decides this one");
        assert_eq!(v.token(), "needs-decision");
    }

    #[test]
    fn an_uncommitted_artifact_is_not_a_handoff() {
        // THE-91: a worker whose sandbox forbade `git commit` leaves the file
        // on disk and untracked. That must read as failure, never success.
        let f = ReapFacts {
            session_live: false,
            artifact_exists: true,
            artifact_tracked: false,
            report_present: false,
        };
        match classify(&row(1, S::Running), &f) {
            ReapVerdict::MarkFailed { why } => {
                assert!(
                    why.contains("not tracked") || why.contains("NOT tracked"),
                    "{why}"
                );
                assert!(why.contains("THE-91"), "{why}");
            }
            other => panic!("expected MarkFailed, got {other:?}"),
        }
    }

    #[test]
    fn gone_with_no_artifact_at_all_is_a_plain_failure() {
        let f = ReapFacts {
            session_live: false,
            artifact_exists: false,
            artifact_tracked: false,
            report_present: false,
        };
        match classify(&row(1, S::Running), &f) {
            ReapVerdict::MarkFailed { why } => assert!(why.contains("never written"), "{why}"),
            other => panic!("expected MarkFailed, got {other:?}"),
        }
    }

    #[test]
    fn a_report_without_a_committed_artifact_still_fails() {
        // A worker that reported success but never committed has not handed
        // anything off; the report alone must not rescue it.
        let f = ReapFacts {
            session_live: false,
            artifact_exists: true,
            artifact_tracked: false,
            report_present: true,
        };
        assert!(matches!(
            classify(&row(1, S::Running), &f),
            ReapVerdict::MarkFailed { .. }
        ));
    }

    #[test]
    fn plan_and_summary_describe_a_whole_roster() {
        let rows = vec![
            row(1, S::Running), // live
            row(2, S::Running), // close-done
            row(3, S::Running), // needs-decision
            row(4, S::Running), // mark-failed
            row(5, S::Done),    // closed
        ];
        let out = plan(&rows, |r| match r.id {
            1 => ReapFacts {
                session_live: true,
                ..GONE_COMMITTED_REPORTED
            },
            2 => GONE_COMMITTED_REPORTED,
            3 => ReapFacts {
                report_present: false,
                ..GONE_COMMITTED_REPORTED
            },
            _ => ReapFacts {
                session_live: false,
                artifact_exists: false,
                artifact_tracked: false,
                report_present: false,
            },
        });
        assert_eq!(out.len(), 5);
        assert_eq!(out[0].id, 1);
        let s = summarize(&out);
        assert_eq!(s.live, 1);
        assert_eq!(s.close_done, 1);
        assert_eq!(s.needs_decision, 1);
        assert_eq!(s.mark_failed, 1);
        assert_eq!(s.closed, 1);
        assert_eq!(
            s.actionable(),
            2,
            "only close-done and mark-failed change rows"
        );
    }
}
