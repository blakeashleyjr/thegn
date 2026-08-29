//! Slot-claim policy — the pure half of "may this dispatch be created?".
//!
//! # Why a claim exists at all
//!
//! [`crate::config_pipeline`] states the doctrine: `[[pipeline.stages]]` is
//! **structure, not judgment**, and thegn does not run a scheduler. That stands.
//! What this module adds is not judgment — it is *arithmetic the supervisor was
//! already supposed to do and cannot do atomically from outside*:
//!
//! * counting a stage's occupied slots, and
//! * noticing that an equivalent row already exists.
//!
//! A supervising agent doing this with `dispatch list` then `dispatch put` has a
//! read-modify-write race with itself (two monitors) and with its own restarts.
//! Worse, on 2026-08-29 it did the count from **live daemon sessions** rather
//! than from rows, so every worker that had exited into an unclosed row looked
//! like free capacity: 33 issues ran against a configured budget of 9, and one
//! worktree accumulated eight successive dispatches.
//!
//! So the rule is unchanged — the Lead still decides *what* to dispatch and
//! *when* — but *whether a slot exists* becomes one atomic, checkable answer.
//!
//! Everything here is pure: rows in, decision out. The transaction that makes
//! the decision binding lives in `db_dispatch::claim_dispatch`, which re-runs
//! this function inside the write lock so the check and the insert cannot be
//! split.

use crate::issue::{AgentDispatch, AgentDispatchStatus};

/// What a supervisor is asking to create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimRequest {
    /// Tracker id, e.g. `linear:THE-19`.
    pub issue_id: String,
    /// The `[[pipeline.stages]]` name.
    pub stage: String,
    /// Worktree the worker will run in.
    pub worktree_path: String,
    /// The handoff artifact this row will produce. This is the field that
    /// distinguishes legitimate parallel work from a duplicate: three coders on
    /// `chunk-1/2/3-done.md` are three different jobs, while a second row
    /// writing an artifact an open row already owns is a re-dispatch.
    pub artifact_path: Option<String>,
    /// The chunk file this row works under, when the stage fans out.
    pub chunk_path: Option<String>,
}

/// Why a claim was refused, or that it was granted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimDecision {
    /// A slot exists and no equivalent row is open.
    Grant,
    /// An equivalent row is already open — same issue, stage, worktree and
    /// artifact. Carries the row id so the caller can point at it.
    DuplicateOf {
        /// The open row this request would duplicate.
        id: i64,
        /// Whether that row's worker has already exited (it is stale, not
        /// live) — the caller should reconcile it rather than dispatch again.
        exited: bool,
    },
    /// The stage's `concurrency` budget is already fully occupied.
    AtCapacity {
        /// Rows currently occupying a slot in this stage.
        occupied: usize,
        /// The configured budget.
        limit: u32,
        /// How many of `occupied` are exited-but-unclosed — the actionable
        /// number, because reconciling those is what frees capacity.
        stale: usize,
    },
}

impl ClaimDecision {
    /// Whether the dispatch may proceed.
    pub fn granted(&self) -> bool {
        matches!(self, Self::Grant)
    }

    /// Operator-facing refusal text, empty for [`Self::Grant`].
    pub fn reason(&self) -> String {
        match self {
            Self::Grant => String::new(),
            Self::DuplicateOf { id, exited: true } => format!(
                "row {id} already covers this issue/stage/worktree/artifact and its worker has \
                 EXITED without the row being closed. Dispatching again would duplicate finished \
                 work — reconcile row {id} first (`thegn dispatch verify {id}`, then \
                 `set-status done|failed`). Override with `--allow-duplicate <reason>` only if \
                 this really is separate work."
            ),
            Self::DuplicateOf { id, exited: false } => format!(
                "row {id} already covers this issue/stage/worktree/artifact and its worker is \
                 still live. Override with `--allow-duplicate <reason>` only if this really is \
                 separate work — give the new row its own artifact path if so."
            ),
            Self::AtCapacity {
                occupied,
                limit,
                stale,
            } => {
                let tail = if *stale > 0 {
                    format!(
                        " {stale} of them have already EXITED without being closed — reconcile \
                         those to free capacity rather than raising the budget."
                    )
                } else {
                    String::new()
                };
                format!("stage is at capacity: {occupied} row(s) occupy a budget of {limit}.{tail}")
            }
        }
    }
}

/// Whether a row still occupies a slot: any non-terminal status, whether or not
/// its worker has exited.
///
/// **The exited case is the whole point.** A row whose worker finished but whose
/// status nobody advanced is not free capacity — it is unreconciled work. Any
/// accounting that drops it (counting live sessions, say) will over-dispatch.
fn occupies(row: &AgentDispatch) -> bool {
    row.status.is_active()
}

/// Whether a row has been recorded as exited (schema v63). Absence of a stamp
/// means unknown, never exited — see [`crate::pipeline_run::row_liveness`].
fn exited(row: &AgentDispatch) -> bool {
    row.exit_code.is_some() || row.exited_at_ms.is_some()
}

/// Whether an existing row covers the same work as `req`.
///
/// Identity is issue + stage + worktree + artifact, and the artifact is what
/// makes parallel chunks expressible: the pipeline legitimately runs several
/// coders in one worktree at one stage, and they differ only by the file they
/// each produce. Two rows with *no* artifact on either side and the same
/// issue/stage/worktree are treated as the same job — that is the only shape
/// where there is nothing else to tell them apart.
fn same_work(row: &AgentDispatch, req: &ClaimRequest) -> bool {
    let norm = |s: &Option<String>| {
        s.as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
    };
    row.issue_id == req.issue_id
        && row.stage.as_deref().map(str::trim) == Some(req.stage.trim())
        && row.worktree_path == req.worktree_path
        && norm(&row.artifact_path) == norm(&req.artifact_path)
        && norm(&row.chunk_path) == norm(&req.chunk_path)
}

/// Decide one claim against the whole roster.
///
/// Duplicate detection runs before the capacity check so the operator sees the
/// *actionable* problem first: at a full stage where the duplicate is itself one
/// of the occupants, "row 289 is already doing this" is a better message than
/// "the stage is full".
///
/// `limit == 0` is treated as "no budget configured" and only the duplicate rule
/// applies — a zero budget is a config error caught by `validate_pipeline`, and
/// refusing every dispatch here would be a second, less legible report of it.
pub fn decide(rows: &[AgentDispatch], req: &ClaimRequest, limit: u32) -> ClaimDecision {
    if let Some(dup) = rows.iter().find(|r| occupies(r) && same_work(r, req)) {
        return ClaimDecision::DuplicateOf {
            id: dup.id,
            exited: exited(dup),
        };
    }
    if limit == 0 {
        return ClaimDecision::Grant;
    }
    let in_stage: Vec<&AgentDispatch> = rows
        .iter()
        .filter(|r| occupies(r) && r.stage.as_deref().map(str::trim) == Some(req.stage.trim()))
        .collect();
    if in_stage.len() >= limit as usize {
        return ClaimDecision::AtCapacity {
            occupied: in_stage.len(),
            limit,
            stale: in_stage.iter().filter(|r| exited(r)).count(),
        };
    }
    ClaimDecision::Grant
}

/// The statuses a claim counts as occupying a slot, for documentation and for
/// the CLI's help text. Mirrors [`AgentDispatchStatus::is_active`].
pub fn occupying_statuses() -> Vec<AgentDispatchStatus> {
    [
        AgentDispatchStatus::Queued,
        AgentDispatchStatus::Spawning,
        AgentDispatchStatus::Running,
        AgentDispatchStatus::WaitingHuman,
        AgentDispatchStatus::PrOpen,
    ]
    .into_iter()
    .filter(|s| s.is_active())
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::issue::AgentDispatchStatus as S;

    fn row(id: i64, issue: &str, stage: &str, wt: &str, artifact: Option<&str>) -> AgentDispatch {
        AgentDispatch {
            id,
            issue_id: issue.to_string(),
            worktree_path: wt.to_string(),
            agent_name: "coder".into(),
            dispatched_at_ms: id,
            status: S::Running,
            stage: Some(stage.to_string()),
            parent_id: None,
            session_id: Some(format!("s{id}")),
            artifact_path: artifact.map(str::to_string),
            note: None,
            chunk_path: None,
            report: None,
            exit_code: None,
            exited_at_ms: None,
        }
    }

    fn req(issue: &str, stage: &str, wt: &str, artifact: Option<&str>) -> ClaimRequest {
        ClaimRequest {
            issue_id: issue.to_string(),
            stage: stage.to_string(),
            worktree_path: wt.to_string(),
            artifact_path: artifact.map(str::to_string),
            chunk_path: None,
        }
    }

    #[test]
    fn parallel_chunks_in_one_worktree_are_not_duplicates() {
        // The pipeline's real shape: three coders, one worktree, one stage,
        // three artifacts. A naive issue+stage+worktree key would refuse two of
        // them — which is why identity includes the artifact.
        let rows = vec![
            row(
                1,
                "linear:THE-19",
                "code",
                "/wt/19",
                Some("chunk-1-done.md"),
            ),
            row(
                2,
                "linear:THE-19",
                "code",
                "/wt/19",
                Some("chunk-2-done.md"),
            ),
        ];
        let d = decide(
            &rows,
            &req("linear:THE-19", "code", "/wt/19", Some("chunk-3-done.md")),
            3,
        );
        assert_eq!(d, ClaimDecision::Grant, "{}", d.reason());
    }

    #[test]
    fn a_re_dispatch_of_the_same_artifact_is_refused() {
        let rows = vec![row(
            7,
            "linear:THE-19",
            "code",
            "/wt/19",
            Some("chunk-1-done.md"),
        )];
        let d = decide(
            &rows,
            &req("linear:THE-19", "code", "/wt/19", Some("chunk-1-done.md")),
            3,
        );
        assert_eq!(
            d,
            ClaimDecision::DuplicateOf {
                id: 7,
                exited: false
            }
        );
        assert!(d.reason().contains("row 7"), "{}", d.reason());
    }

    #[test]
    fn an_exited_unclosed_row_refuses_the_redispatch_and_says_to_reconcile() {
        // The incident in one assertion: the worker exited, nobody closed the
        // row, and the Lead came back to dispatch the same work again.
        let mut r = row(289, "linear:THE-19", "code", "/wt/19", Some("chunk-1.md"));
        r.exit_code = Some(0);
        r.exited_at_ms = Some(1_000);
        let d = decide(
            &[r],
            &req("linear:THE-19", "code", "/wt/19", Some("chunk-1.md")),
            3,
        );
        assert_eq!(
            d,
            ClaimDecision::DuplicateOf {
                id: 289,
                exited: true
            }
        );
        let why = d.reason();
        assert!(why.contains("EXITED"), "{why}");
        assert!(why.contains("dispatch verify 289"), "{why}");
    }

    #[test]
    fn exited_but_unclosed_rows_still_consume_the_stage_budget() {
        // The accounting bug that drove the runaway: these two rows' workers are
        // gone, so a session-based count sees a free stage. They are unreconciled
        // work, so the claim must still refuse — and must say that reconciling
        // them, not raising the budget, is the fix.
        let mut rows = vec![
            row(1, "linear:A-1", "code", "/wt/a", Some("a.md")),
            row(2, "linear:A-2", "code", "/wt/b", Some("b.md")),
        ];
        for r in &mut rows {
            r.exit_code = Some(0);
            r.exited_at_ms = Some(1_000);
        }
        let d = decide(&rows, &req("linear:A-3", "code", "/wt/c", Some("c.md")), 2);
        assert_eq!(
            d,
            ClaimDecision::AtCapacity {
                occupied: 2,
                limit: 2,
                stale: 2
            }
        );
        let why = d.reason();
        assert!(why.contains("already EXITED"), "{why}");
        assert!(why.contains("free capacity"), "{why}");
    }

    #[test]
    fn capacity_is_counted_per_stage_not_across_the_roster() {
        let rows = vec![
            row(1, "linear:A-1", "architect", "/wt/a", Some("d.md")),
            row(2, "linear:A-2", "architect", "/wt/b", Some("d.md")),
        ];
        // architect is full at 2...
        assert!(matches!(
            decide(
                &rows,
                &req("linear:A-3", "architect", "/wt/c", Some("d.md")),
                2
            ),
            ClaimDecision::AtCapacity { .. }
        ));
        // ...but that says nothing about `code`.
        assert_eq!(
            decide(&rows, &req("linear:A-3", "code", "/wt/c", Some("d.md")), 3),
            ClaimDecision::Grant
        );
    }

    #[test]
    fn terminal_rows_free_their_slot() {
        let mut rows = vec![
            row(1, "linear:A-1", "code", "/wt/a", Some("a.md")),
            row(2, "linear:A-2", "code", "/wt/b", Some("b.md")),
        ];
        rows[0].status = S::Done;
        rows[1].status = S::Failed;
        // Both closed ⇒ the stage is empty again, and a re-dispatch of the very
        // same artifact is no longer a duplicate (the previous attempt is over).
        assert_eq!(
            decide(&rows, &req("linear:A-1", "code", "/wt/a", Some("a.md")), 2),
            ClaimDecision::Grant
        );
    }

    #[test]
    fn a_zero_budget_defers_to_config_validation_and_only_checks_duplicates() {
        let rows = vec![row(1, "linear:A-1", "code", "/wt/a", Some("a.md"))];
        assert_eq!(
            decide(&rows, &req("linear:A-2", "code", "/wt/b", Some("b.md")), 0),
            ClaimDecision::Grant
        );
        assert!(matches!(
            decide(&rows, &req("linear:A-1", "code", "/wt/a", Some("a.md")), 0),
            ClaimDecision::DuplicateOf { .. }
        ));
    }

    #[test]
    fn artifactless_rows_of_one_stage_and_worktree_are_the_same_job() {
        // Nothing distinguishes them, so a second one is a re-dispatch.
        let rows = vec![row(1, "linear:A-1", "review", "/wt/a", None)];
        assert!(matches!(
            decide(&rows, &req("linear:A-1", "review", "/wt/a", None), 2),
            ClaimDecision::DuplicateOf { id: 1, .. }
        ));
    }

    #[test]
    fn occupying_statuses_match_the_is_active_closed_set() {
        // Pins the two definitions together: if `is_active` ever changes, this
        // list must be revisited rather than silently drifting.
        let got = occupying_statuses();
        assert_eq!(got.len(), 5, "{got:?}");
        assert!(got.iter().all(|s| s.is_active()));
    }
}
