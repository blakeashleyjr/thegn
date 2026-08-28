//! The sidebar's derived pipeline **folders**: under each workspace, one
//! `Pipelines` folder holding one folder per pipeline the dispatch roster
//! knows — named from the roster's `issue_id` — with every worktree that
//! pipeline's roster rows reference inside it.
//!
//! Pure — a fold over roster rows already in memory. It runs on the hydration
//! thread beside the other roster derivations
//! (`monitor_pipeline::{stage_badges, summary, stage_blocked}`), so it opens no
//! DB, spawns nothing and adds no wake source.
//!
//! The one structural rule worth stating up front: **every roster row
//! participates, whatever its status**. The roster is SQLite state that
//! outlives the sessions and the UI process, so the folders survive a restart
//! and a finished lane stays until its rows are removed — the point of the
//! fold is that a pipeline's worktrees are findable by default, not only while
//! its agents are live. (A lane with no rows does not exist; there is nothing
//! to reap and nothing persisted — the lanes are a view of the roster, never
//! state of their own.)

use thegn_core::issue::AgentDispatch;

/// One derived lane folder: all the roster's rows for one issue (or, absent an
/// issue id, for one worktree).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Lane {
    /// Stable identity — the row's `issue_id`, else its worktree basename.
    /// Keys the collapse state, so it must not change as the lane advances.
    pub key: String,
    /// The folder's name. The lane is **named from the roster's issue id**
    /// (degrading to the worktree basename when the rows carry none); its
    /// worktrees hang below it as leaves, so the label needs no suffix.
    pub label: String,
    /// Every worktree the lane's roster rows reference — rows of **any**
    /// status — deduped by path, oldest reference first.
    pub worktrees: Vec<LaneWorktree>,
}

/// One worktree a lane's roster rows reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LaneWorktree {
    /// Full worktree path — the identity the sidebar resolves a jump against.
    pub path: String,
    /// `path`'s basename — what the leaf row shows.
    pub name: String,
    /// The earliest dispatch in this lane referencing the worktree (ms): the
    /// leaf order is the order work arrived in it.
    pub at_ms: i64,
}

/// Fold the roster into the sidebar's lane folders.
///
/// - **Every row participates** — `queued`, `running`, `merged`, `failed`, all
///   of them. The roster is a ledger, not a live-session view, so a lane
///   survives a restart and outlives its last active dispatch.
/// - **Lane key / name**: the row's `issue_id` when non-blank, else the
///   basename of its `worktree_path`. A row with neither is skipped — it has
///   no identity to file under, and inventing one would merge unrelated work.
/// - **Worktrees**: distinct by full path; ordered by the earliest dispatch
///   that references them, tie-broken by name, so the tree never reshuffles
///   frame to frame.
/// - **Lane order**: earliest `dispatched_at_ms` of the lane's rows first
///   (the order work started — the same reading
///   `monitor_pipeline::ordered_rows` uses), tie-broken by key.
pub(crate) fn lanes(dispatches: &[AgentDispatch]) -> Vec<Lane> {
    // Grouped by key, in first-seen order; the real ordering happens below.
    let mut keys: Vec<String> = Vec::new();
    let mut by_key: std::collections::HashMap<String, Vec<&AgentDispatch>> =
        std::collections::HashMap::new();
    for d in dispatches {
        let Some(key) = lane_key(d) else {
            continue;
        };
        let slot = by_key.entry(key.clone()).or_insert_with(|| {
            keys.push(key.clone());
            Vec::new()
        });
        slot.push(d);
    }

    let mut lanes: Vec<(i64, Lane)> = keys
        .into_iter()
        .filter_map(|key| {
            let rows = by_key.remove(&key)?;
            if rows.is_empty() {
                return None;
            }
            // Distinct worktrees, keeping the earliest reference to each.
            let mut seen: Vec<LaneWorktree> = Vec::new();
            for d in &rows {
                let path = d.worktree_path.trim();
                if path.is_empty() {
                    continue;
                }
                if let Some(w) = seen.iter_mut().find(|w| w.path == path) {
                    w.at_ms = w.at_ms.min(d.dispatched_at_ms);
                } else {
                    seen.push(LaneWorktree {
                        path: path.to_string(),
                        name: thegn_core::util::basename(path).to_string(),
                        at_ms: d.dispatched_at_ms,
                    });
                }
            }
            seen.sort_by(|a, b| a.at_ms.cmp(&b.at_ms).then_with(|| a.name.cmp(&b.name)));
            let earliest = rows.iter().map(|d| d.dispatched_at_ms).min().unwrap_or(0);
            Some((
                earliest,
                Lane {
                    label: key.clone(),
                    key,
                    worktrees: seen,
                },
            ))
        })
        .collect();

    lanes.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.key.cmp(&b.1.key)));
    lanes.into_iter().map(|(_, lane)| lane).collect()
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

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::issue::AgentDispatchStatus;

    fn dispatch(
        id: i64,
        issue: &str,
        worktree: &str,
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
            stage: None,
            parent_id: None,
            session_id: None,
            artifact_path: None,
            note: None,
        }
    }

    fn key_of(lanes: &[Lane]) -> Vec<&str> {
        lanes.iter().map(|l| l.key.as_str()).collect()
    }

    fn wt_names(lane: &Lane) -> Vec<&str> {
        lane.worktrees.iter().map(|w| w.name.as_str()).collect()
    }

    // The directive's core property: the folders are derived from the roster's
    // rows of ANY status, and the roster is SQLite state — so the same fold
    // over the same rows after a restart yields the same folders. A lane does
    // not vanish when its last dispatch goes terminal.
    #[test]
    fn a_lane_survives_its_rows_going_terminal() {
        let rows = vec![dispatch(
            1,
            "THE-74",
            "/wt/tg-the-74",
            AgentDispatchStatus::Merged,
            1_000,
        )];
        let out = lanes(&rows);
        assert_eq!(key_of(&out), vec!["THE-74"], "a finished lane stays");
        assert_eq!(wt_names(&out[0]), vec!["tg-the-74"]);
    }

    #[test]
    fn rows_of_every_status_file_into_the_same_lane() {
        let rows = vec![
            dispatch(1, "THE-74", "/wt/a", AgentDispatchStatus::Queued, 1_000),
            dispatch(2, "THE-74", "/wt/a", AgentDispatchStatus::Running, 2_000),
            dispatch(3, "THE-74", "/wt/b", AgentDispatchStatus::Failed, 3_000),
            dispatch(4, "THE-74", "/wt/b", AgentDispatchStatus::Done, 4_000),
        ];
        let out = lanes(&rows);
        assert_eq!(key_of(&out), vec!["THE-74"]);
        assert_eq!(wt_names(&out[0]), vec!["a", "b"], "one leaf per path");
    }

    #[test]
    fn the_lane_is_named_from_the_rosters_issue_id() {
        let rows = vec![dispatch(
            1,
            "linear:T-99",
            "/wt/tg-t-99",
            AgentDispatchStatus::Running,
            1_000,
        )];
        let out = lanes(&rows);
        assert_eq!(out[0].key, "linear:T-99");
        assert_eq!(out[0].label, "linear:T-99");
    }

    #[test]
    fn a_blank_issue_id_degrades_to_the_worktree_basename() {
        let rows = vec![dispatch(
            1,
            "   ",
            "/wt/tg-no-issue",
            AgentDispatchStatus::Running,
            1_000,
        )];
        let out = lanes(&rows);
        assert_eq!(out[0].key, "tg-no-issue");
        assert_eq!(out[0].label, "tg-no-issue");
    }

    #[test]
    fn a_row_with_no_identity_at_all_is_skipped() {
        let rows = vec![dispatch(1, "", "", AgentDispatchStatus::Running, 1_000)];
        assert!(lanes(&rows).is_empty(), "nothing to file it under");
    }

    #[test]
    fn two_lanes_never_merge() {
        let rows = vec![
            dispatch(1, "THE-74", "/wt/a", AgentDispatchStatus::Running, 2_000),
            dispatch(2, "THE-9", "/wt/b", AgentDispatchStatus::Running, 1_000),
        ];
        // THE-9 started first, so it leads — the order work started.
        assert_eq!(key_of(&lanes(&rows)), vec!["THE-9", "THE-74"]);
    }

    #[test]
    fn a_worktree_repeated_across_rows_is_one_leaf() {
        let rows = vec![
            dispatch(
                1,
                "THE-74",
                "/wt/tg-the-74",
                AgentDispatchStatus::Merged,
                5_000,
            ),
            dispatch(
                2,
                "THE-74",
                "/wt/tg-the-74",
                AgentDispatchStatus::Running,
                1_000,
            ),
        ];
        let out = lanes(&rows);
        assert_eq!(wt_names(&out[0]), vec!["tg-the-74"], "deduped by path");
        // The leaf keeps its EARLIEST reference, not its latest.
        assert_eq!(out[0].worktrees[0].at_ms, 1_000);
    }

    #[test]
    fn worktrees_order_by_their_earliest_dispatch_then_name() {
        let rows = vec![
            dispatch(1, "THE-74", "/wt/zzz", AgentDispatchStatus::Done, 1_000),
            dispatch(2, "THE-74", "/wt/aaa", AgentDispatchStatus::Running, 2_000),
            dispatch(3, "THE-74", "/wt/aaa", AgentDispatchStatus::Queued, 3_000),
        ];
        let out = lanes(&rows);
        // zzz was referenced first, even though aaa's name sorts lower.
        assert_eq!(wt_names(&out[0]), vec!["zzz", "aaa"]);
    }

    #[test]
    fn lanes_order_by_earliest_dispatch_then_key() {
        let rows = vec![
            dispatch(1, "THE-74", "/wt/a", AgentDispatchStatus::Running, 2_000),
            dispatch(2, "THE-74", "/wt/b", AgentDispatchStatus::Running, 5_000),
            dispatch(3, "THE-9", "/wt/c", AgentDispatchStatus::Running, 2_000),
        ];
        // Same earliest stamp → key breaks the tie deterministically.
        assert_eq!(key_of(&lanes(&rows)), vec!["THE-74", "THE-9"]);
    }

    #[test]
    fn an_empty_roster_yields_no_lanes() {
        assert!(lanes(&[]).is_empty());
    }

    #[test]
    fn a_lane_whose_rows_carry_no_worktree_still_exists() {
        // An issue id without a resolvable worktree path is still a lane; its
        // folder just has no leaves to show yet.
        let rows = vec![dispatch(
            1,
            "THE-74",
            "",
            AgentDispatchStatus::Running,
            1_000,
        )];
        let out = lanes(&rows);
        assert_eq!(key_of(&out), vec!["THE-74"]);
        assert!(out[0].worktrees.is_empty());
    }

    #[test]
    fn the_fold_is_stable_across_repeated_calls() {
        // Restart semantics: the same rows must fold to the same folders,
        // every time — no call-order or liveness dependence.
        let rows = vec![
            dispatch(1, "THE-74", "/wt/a", AgentDispatchStatus::Done, 3_000),
            dispatch(2, "THE-9", "/wt/b", AgentDispatchStatus::Running, 1_000),
            dispatch(3, "THE-74", "/wt/c", AgentDispatchStatus::Queued, 2_000),
        ];
        let first = lanes(&rows);
        let second = lanes(&rows);
        assert_eq!(first, second);
        assert_eq!(key_of(&first), vec!["THE-9", "THE-74"]);
    }
}
