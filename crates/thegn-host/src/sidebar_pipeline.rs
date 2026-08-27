//! The sidebar's dynamic pipeline **lane folders**: one derived folder per
//! issue/worktree lane that currently has live agent-dispatch rows, its agents
//! under it, and each agent's worktree under that.
//!
//! Pure — a fold over roster rows already in memory. It runs on the hydration
//! thread beside the other three roster derivations
//! (`monitor_pipeline::{stage_badges, summary, stage_blocked}`), so it opens no
//! DB, spawns nothing and adds no wake source.
//!
//! The one structural rule worth stating up front: **a lane exists only while
//! it has active rows** ([`AgentDispatchStatus::is_active`]). Terminal rows are
//! dropped before grouping, so a finished lane disappears on its own — there is
//! no reaper, no lane table, and nothing to garbage-collect. The lanes are a
//! view of the roster, never state of their own.

use thegn_core::issue::{AgentDispatch, AgentDispatchStatus};

/// One derived lane folder: the live work on one issue (or, absent an issue id,
/// on one worktree).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Lane {
    /// Stable identity — the row's `issue_id`, else its worktree basename.
    /// Keys the collapse state, so it must not change as the lane advances.
    pub key: String,
    /// What the folder shows: `{issue_id} · {worktree}`, or the bare worktree
    /// when the rows carry no issue id. Truncation is the render side's job.
    pub label: String,
    /// The lane's active rows, in stage order (see [`lanes`]).
    pub agents: Vec<LaneAgent>,
}

/// One active roster row inside a lane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LaneAgent {
    /// Roster row id — the lane-local identity (a worktree can repeat).
    pub id: i64,
    /// The row's `[[pipeline.stages]]` name; empty for a non-pipeline dispatch.
    pub stage: String,
    pub agent_name: String,
    pub status: AgentDispatchStatus,
    pub worktree_path: String,
    /// `worktree_path`'s basename — what the leaf row shows.
    pub worktree: String,
    pub dispatched_at_ms: i64,
}

/// Fold the roster into the sidebar's lane folders.
///
/// - Only [`AgentDispatchStatus::is_active`] rows participate; a lane with no
///   active row is not emitted at all (the appear/vanish rule).
/// - **Lane key**: the row's `issue_id` when non-blank, else the basename of
///   its `worktree_path`. A row with neither is skipped — it has no identity to
///   file under, and inventing one would merge unrelated work.
/// - **Lane label**: `{issue_id} · {worktree}`, or the bare worktree with no
///   issue id (and the bare issue id when the row carries no worktree path).
///   The worktree is the lane's **earliest** active row's, so the name is
///   stable as the lane advances from stage to stage.
/// - **Lane order**: earliest active `dispatched_at_ms` first (the order work
///   started — the same reading `monitor_pipeline::ordered_rows` uses),
///   tie-broken by key so the tree never reshuffles frame to frame.
/// - **Agent order**: configured `stage_order` first, then any unnamed stage by
///   name, then `dispatched_at_ms`, then row id.
pub(crate) fn lanes(dispatches: &[AgentDispatch], stage_order: &[String]) -> Vec<Lane> {
    // Grouped by key, in first-seen order; the real ordering happens below.
    let mut keys: Vec<String> = Vec::new();
    let mut by_key: std::collections::HashMap<String, Vec<&AgentDispatch>> =
        std::collections::HashMap::new();
    for d in dispatches.iter().filter(|d| d.status.is_active()) {
        let Some(key) = lane_key(d) else {
            continue;
        };
        let slot = by_key.entry(key.clone()).or_insert_with(|| {
            keys.push(key.clone());
            Vec::new()
        });
        slot.push(d);
    }

    let mut lanes: Vec<Lane> = keys
        .into_iter()
        .filter_map(|key| {
            let mut rows = by_key.remove(&key)?;
            if rows.is_empty() {
                return None;
            }
            // Earliest active row names the lane (and orders it).
            rows.sort_by_key(|d| (d.dispatched_at_ms, d.id));
            let head = rows[0];
            let label = lane_label(head);
            rows.sort_by_key(|d| agent_sort_key(d, stage_order));
            Some(Lane {
                key,
                label,
                agents: rows.into_iter().map(lane_agent).collect(),
            })
        })
        .collect();

    lanes.sort_by(|a, b| {
        let started = |l: &Lane| {
            l.agents
                .iter()
                .map(|a| a.dispatched_at_ms)
                .min()
                .unwrap_or(0)
        };
        started(a).cmp(&started(b)).then_with(|| a.key.cmp(&b.key))
    });
    lanes
}

/// The lane a row files under: its issue id, else its worktree basename.
/// `None` when it has neither — the row has no identity a folder could carry.
fn lane_key(d: &AgentDispatch) -> Option<String> {
    let issue = d.issue_id.trim();
    if !issue.is_empty() {
        return Some(issue.to_string());
    }
    let wt = thegn_core::util::basename(d.worktree_path.trim());
    (!wt.is_empty()).then(|| wt.to_string())
}

/// `{issue_id} · {worktree}` — degrading to whichever half the row actually
/// has. The separator comes from the glyph ladder, never a literal.
fn lane_label(d: &AgentDispatch) -> String {
    let issue = d.issue_id.trim();
    let wt = thegn_core::util::basename(d.worktree_path.trim());
    match (issue.is_empty(), wt.is_empty()) {
        (false, false) => format!("{issue} {} {wt}", crate::caps::active_glyphs().middot),
        (false, true) => issue.to_string(),
        _ => wt.to_string(),
    }
}

fn lane_agent(d: &AgentDispatch) -> LaneAgent {
    LaneAgent {
        id: d.id,
        stage: stage_of(d).to_string(),
        agent_name: d.agent_name.clone(),
        status: d.status,
        worktree_path: d.worktree_path.clone(),
        worktree: thegn_core::util::basename(&d.worktree_path).to_string(),
        dispatched_at_ms: d.dispatched_at_ms,
    }
}

fn stage_of(d: &AgentDispatch) -> &str {
    d.stage.as_deref().map(str::trim).unwrap_or_default()
}

/// Configured stage order first (a stage `stage_order` doesn't name sorts after
/// the named ones, by name), then start time, then row id.
fn agent_sort_key(d: &AgentDispatch, stage_order: &[String]) -> (usize, String, i64, i64) {
    let stage = stage_of(d);
    let rank = stage_order
        .iter()
        .position(|s| s == stage)
        .unwrap_or(usize::MAX);
    (rank, stage.to_string(), d.dispatched_at_ms, d.id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dispatch(
        id: i64,
        issue: &str,
        worktree: &str,
        stage: Option<&str>,
        status: AgentDispatchStatus,
        at_ms: i64,
    ) -> AgentDispatch {
        AgentDispatch {
            id,
            issue_id: issue.to_string(),
            worktree_path: worktree.to_string(),
            agent_name: "claude".into(),
            dispatched_at_ms: at_ms,
            status,
            stage: stage.map(str::to_string),
            parent_id: None,
            session_id: None,
            artifact_path: None,
        }
    }

    fn order() -> Vec<String> {
        vec!["architect".into(), "code".into(), "review".into()]
    }

    #[test]
    fn a_lane_appears_while_it_has_active_rows() {
        let rows = vec![dispatch(
            1,
            "THE-74",
            "/w/tg-the-74",
            Some("code"),
            AgentDispatchStatus::Running,
            1_000,
        )];
        let out = lanes(&rows, &order());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].key, "THE-74");
        assert_eq!(out[0].agents.len(), 1);
        assert_eq!(out[0].agents[0].worktree, "tg-the-74");
    }

    #[test]
    fn a_lane_vanishes_when_its_last_row_goes_terminal() {
        for terminal in [
            AgentDispatchStatus::Done,
            AgentDispatchStatus::Failed,
            AgentDispatchStatus::Merged,
            AgentDispatchStatus::Abandoned,
        ] {
            let rows = vec![dispatch(
                1,
                "THE-74",
                "/w/tg-the-74",
                Some("code"),
                terminal,
                1_000,
            )];
            assert!(
                lanes(&rows, &order()).is_empty(),
                "{terminal:?} must not keep a lane alive"
            );
        }
    }

    #[test]
    fn a_terminal_row_drops_out_of_a_still_live_lane() {
        let rows = vec![
            dispatch(
                1,
                "THE-74",
                "/w/tg-the-74",
                Some("architect"),
                AgentDispatchStatus::Done,
                1_000,
            ),
            dispatch(
                2,
                "THE-74",
                "/w/tg-the-74",
                Some("code"),
                AgentDispatchStatus::Running,
                2_000,
            ),
        ];
        let out = lanes(&rows, &order());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].agents.len(), 1);
        assert_eq!(out[0].agents[0].id, 2);
    }

    #[test]
    fn label_joins_the_issue_and_the_worktree() {
        let rows = vec![dispatch(
            1,
            "THE-74",
            "/w/tg-the-74-pipeline",
            Some("code"),
            AgentDispatchStatus::Running,
            1_000,
        )];
        let mid = crate::caps::active_glyphs().middot;
        assert_eq!(
            lanes(&rows, &order())[0].label,
            format!("THE-74 {mid} tg-the-74-pipeline")
        );
    }

    #[test]
    fn label_falls_back_to_the_worktree_without_an_issue_id() {
        let rows = vec![dispatch(
            1,
            "  ",
            "/w/tg-loose",
            None,
            AgentDispatchStatus::Queued,
            1_000,
        )];
        let out = lanes(&rows, &order());
        assert_eq!(out[0].key, "tg-loose", "a blank issue id falls back");
        assert_eq!(out[0].label, "tg-loose");
    }

    #[test]
    fn label_keeps_the_earliest_rows_worktree_as_the_lane_advances() {
        let rows = vec![
            dispatch(
                2,
                "THE-74",
                "/w/tg-chunk-2",
                Some("code"),
                AgentDispatchStatus::Running,
                2_000,
            ),
            dispatch(
                1,
                "THE-74",
                "/w/tg-the-74",
                Some("architect"),
                AgentDispatchStatus::WaitingHuman,
                1_000,
            ),
        ];
        let mid = crate::caps::active_glyphs().middot;
        assert_eq!(
            lanes(&rows, &order())[0].label,
            format!("THE-74 {mid} tg-the-74")
        );
    }

    #[test]
    fn a_row_with_no_issue_id_and_no_worktree_is_skipped() {
        let rows = vec![dispatch(
            1,
            "",
            "",
            Some("code"),
            AgentDispatchStatus::Running,
            1_000,
        )];
        assert!(lanes(&rows, &order()).is_empty());
    }

    #[test]
    fn two_issues_are_two_lanes_oldest_first() {
        let rows = vec![
            dispatch(
                1,
                "THE-9",
                "/w/tg-nine",
                Some("code"),
                AgentDispatchStatus::Running,
                5_000,
            ),
            dispatch(
                2,
                "THE-74",
                "/w/tg-the-74",
                Some("code"),
                AgentDispatchStatus::Running,
                1_000,
            ),
        ];
        let out = lanes(&rows, &order());
        assert_eq!(out.len(), 2, "distinct issues never merge");
        assert_eq!(out[0].key, "THE-74", "oldest lane first");
        assert_eq!(out[1].key, "THE-9");
    }

    #[test]
    fn lanes_that_started_together_are_ordered_by_key() {
        let rows = vec![
            dispatch(
                1,
                "THE-9",
                "/w/b",
                Some("code"),
                AgentDispatchStatus::Running,
                1_000,
            ),
            dispatch(
                2,
                "THE-74",
                "/w/a",
                Some("code"),
                AgentDispatchStatus::Running,
                1_000,
            ),
        ];
        let out = lanes(&rows, &order());
        assert_eq!(out[0].key, "THE-74");
        assert_eq!(out[1].key, "THE-9");
    }

    #[test]
    fn agents_sort_by_configured_stage_then_start_then_id() {
        let rows = vec![
            dispatch(
                3,
                "THE-74",
                "/w/a",
                Some("review"),
                AgentDispatchStatus::Queued,
                9_000,
            ),
            dispatch(
                4,
                "THE-74",
                "/w/b",
                Some("code"),
                AgentDispatchStatus::Running,
                4_000,
            ),
            dispatch(
                5,
                "THE-74",
                "/w/c",
                Some("code"),
                AgentDispatchStatus::Running,
                3_000,
            ),
            dispatch(
                1,
                "THE-74",
                "/w/d",
                Some("architect"),
                AgentDispatchStatus::PrOpen,
                8_000,
            ),
        ];
        let ids: Vec<i64> = lanes(&rows, &order())[0]
            .agents
            .iter()
            .map(|a| a.id)
            .collect();
        assert_eq!(ids, vec![1, 5, 4, 3]);
    }

    #[test]
    fn an_unconfigured_stage_sorts_after_the_named_ones_by_name() {
        let rows = vec![
            dispatch(
                1,
                "THE-74",
                "/w/a",
                Some("zeta"),
                AgentDispatchStatus::Running,
                1_000,
            ),
            dispatch(
                2,
                "THE-74",
                "/w/a",
                None,
                AgentDispatchStatus::Running,
                1_000,
            ),
            dispatch(
                3,
                "THE-74",
                "/w/a",
                Some("review"),
                AgentDispatchStatus::Running,
                1_000,
            ),
        ];
        let out = lanes(&rows, &order());
        let stages: Vec<&str> = out[0].agents.iter().map(|a| a.stage.as_str()).collect();
        assert_eq!(stages, vec!["review", "", "zeta"]);
    }

    #[test]
    fn one_worktree_may_repeat_across_a_lanes_agents() {
        let rows = vec![
            dispatch(
                1,
                "THE-74",
                "/w/tg-the-74",
                Some("architect"),
                AgentDispatchStatus::WaitingHuman,
                1_000,
            ),
            dispatch(
                2,
                "THE-74",
                "/w/tg-the-74",
                Some("code"),
                AgentDispatchStatus::Running,
                2_000,
            ),
        ];
        let out = lanes(&rows, &order());
        assert_eq!(out[0].agents.len(), 2, "each row keeps its own leaf");
        assert!(out[0].agents.iter().all(|a| a.worktree == "tg-the-74"));
    }

    #[test]
    fn rows_without_a_configured_stage_order_still_group() {
        let rows = vec![dispatch(
            1,
            "THE-74",
            "/w/tg-the-74",
            Some("code"),
            AgentDispatchStatus::Spawning,
            1_000,
        )];
        let out = lanes(&rows, &[]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].agents[0].stage, "code");
    }
}
