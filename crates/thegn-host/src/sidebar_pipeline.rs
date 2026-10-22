//! The sidebar's derived pipeline **folders**: under each workspace, one
//! `Pipelines` folder holding one folder per pipeline the dispatch roster
//! knows — named from the roster's canonical issue identity — with every
//! worktree that pipeline's roster rows reference inside it. Unprefixed ids
//! join a matching provider lane only when that key identifies one provider.
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

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum LaneId {
    Issue {
        provider: Option<String>,
        key: String,
    },
    Raw(String),
}

/// One derived lane folder: all the roster's rows for one issue (or, absent an
/// issue id, for one worktree).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Lane {
    /// Stable canonical identity — the earliest canonical issue spelling,
    /// else the raw trimmed id or worktree basename. Keys collapse state.
    pub key: String,
    /// The folder's name. The lane is **named from the canonical issue id**
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
/// - **Lane key / name**: issue ids use canonical provider and case-insensitive
///   key identity. An unprefixed key joins a prefixed lane only when exactly
///   one provider has that key; ambiguous unprefixed ids stay separate. Invalid
///   ids retain their raw trimmed identity. Blank ids use the worktree basename.
///   A row with neither is skipped — it has no identity to file under.
/// - **Worktrees**: distinct by full path; ordered by the earliest dispatch
///   that references them, tie-broken by name, so the tree never reshuffles
///   frame to frame.
/// - **Lane order**: earliest `dispatched_at_ms` of the lane's rows first
///   (the order work started — the same reading
///   `monitor_pipeline::ordered_rows` uses), tie-broken by key.
pub(crate) fn lanes(dispatches: &[AgentDispatch]) -> Vec<Lane> {
    use std::collections::{HashMap, HashSet};

    // Count distinct providers per canonical key before assigning unprefixed
    // rows; an unprefixed id cannot choose between multiple providers.
    let mut providers_by_key: HashMap<String, HashSet<String>> = HashMap::new();
    for d in dispatches {
        if let Some(LaneId::Issue {
            provider: Some(provider),
            key,
        }) = lane_identity(d)
        {
            providers_by_key.entry(key).or_default().insert(provider);
        }
    }

    // Group in first-seen order. The final lane sort is independent of this
    // order, while retaining it makes row collection deterministic.
    let mut ids: Vec<LaneId> = Vec::new();
    let mut by_id: HashMap<LaneId, Vec<&AgentDispatch>> = HashMap::new();
    for d in dispatches {
        let Some(mut id) = lane_identity(d) else {
            continue;
        };
        if let LaneId::Issue {
            provider: None,
            key,
        } = &id
            && let Some(providers) = providers_by_key.get(key)
            && providers.len() == 1
        {
            id = LaneId::Issue {
                provider: providers.iter().next().cloned(),
                key: key.clone(),
            };
        }
        let slot = by_id.entry(id.clone()).or_insert_with(|| {
            ids.push(id.clone());
            Vec::new()
        });
        slot.push(d);
    }

    let mut lanes: Vec<(i64, Lane)> = ids
        .into_iter()
        .filter_map(|id| {
            let rows = by_id.remove(&id)?;
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
                        name: thegn_core::pipeline_report::display_cell(
                            thegn_core::util::basename(path),
                        ),
                        at_ms: d.dispatched_at_ms,
                    });
                }
            }
            seen.sort_by(|a, b| a.at_ms.cmp(&b.at_ms).then_with(|| a.name.cmp(&b.name)));
            let earliest = rows.iter().map(|d| d.dispatched_at_ms).min()?;
            let key = lane_display_key(&id, &rows);
            Some((
                earliest,
                Lane {
                    label: thegn_core::pipeline_report::display_cell(&key),
                    key,
                    worktrees: seen,
                },
            ))
        })
        .collect();

    lanes.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.key.cmp(&b.1.key)));
    lanes.into_iter().map(|(_, lane)| lane).collect()
}

/// The grouping identity before unprefixed-provider ambiguity is resolved.
fn lane_identity(d: &AgentDispatch) -> Option<LaneId> {
    let issue = d.issue_id.trim();
    if !issue.is_empty() {
        return Some(
            match thegn_core::pipeline_identity::canonical_issue_id(issue) {
                Ok(canonical) => {
                    let (provider, key) = thegn_core::pipeline_identity::split_issue_id(&canonical);
                    LaneId::Issue {
                        provider: provider.map(str::to_string),
                        key: key.to_ascii_uppercase(),
                    }
                }
                Err(_) => LaneId::Raw(issue.to_string()),
            },
        );
    }
    let wt = thegn_core::util::basename(d.worktree_path.trim());
    (!wt.is_empty()).then(|| LaneId::Raw(wt.to_string()))
}

/// Choose a stable display spelling: the earliest prefixed row when present,
/// otherwise the earliest row. Dispatch id breaks timestamp ties.
fn lane_display_key(id: &LaneId, rows: &[&AgentDispatch]) -> String {
    match id {
        LaneId::Raw(raw) => raw.clone(),
        LaneId::Issue {
            provider: Some(provider),
            key,
        } => {
            let row = rows
                .iter()
                .filter(|d| {
                    let issue = d.issue_id.trim();
                    thegn_core::pipeline_identity::canonical_issue_id(issue).is_ok_and(
                        |canonical| {
                            let (row_provider, _) =
                                thegn_core::pipeline_identity::split_issue_id(&canonical);
                            row_provider == Some(provider.as_str())
                        },
                    )
                })
                .min_by_key(|d| (d.dispatched_at_ms, d.id));
            let Some(row) = row else {
                return format!("{provider}:{key}");
            };
            let (_, key) = thegn_core::pipeline_identity::split_issue_id(row.issue_id.trim());
            format!("{provider}:{key}")
        }
        LaneId::Issue {
            provider: None,
            key,
        } => {
            let Some(row) = rows.iter().min_by_key(|d| (d.dispatched_at_ms, d.id)) else {
                return key.clone();
            };
            match thegn_core::pipeline_identity::canonical_issue_id(row.issue_id.trim()) {
                Ok(canonical) => canonical,
                Err(_) => key.clone(),
            }
        }
    }
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
            chunk_path: None,
            report: None,
            exit_code: None,
            exited_at_ms: None,
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
    fn hostile_lane_labels_are_safe_while_identities_stay_raw() {
        let issue = "THE-1\r\n\x1b]52;c;QQ==\x07\u{202e}";
        let worktree = "/wt/a\nb\u{2028}c";
        let out = lanes(&[dispatch(
            1,
            issue,
            worktree,
            AgentDispatchStatus::Running,
            1_000,
        )]);
        assert_eq!(out[0].key, issue.trim());
        assert_eq!(out[0].worktrees[0].path, worktree);
        assert_safe_cell(&out[0].label);
        assert_safe_cell(&out[0].worktrees[0].name);

        let fallback_path = "/wt/a\nb\u{2028}c";
        let fallback = lanes(&[dispatch(
            2,
            "  ",
            fallback_path,
            AgentDispatchStatus::Running,
            2_000,
        )]);
        assert_eq!(fallback[0].key, "a\nb\u{2028}c");
        assert_eq!(fallback[0].worktrees[0].path, fallback_path);
        assert_safe_cell(&fallback[0].label);
        assert_safe_cell(&fallback[0].worktrees[0].name);
    }

    fn assert_safe_cell(value: &str) {
        assert!(
            value
                .chars()
                .all(|c| !c.is_control() && !thegn_core::pipeline_report::is_format_control(c)),
            "unsafe display cell: {value:?}"
        );
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

    #[test]
    fn provider_case_and_whitespace_aliases_fold_into_one_lane() {
        let rows = vec![
            dispatch(
                1,
                "LINEAR:THE-74",
                "/wt/a",
                AgentDispatchStatus::Running,
                1_000,
            ),
            dispatch(
                2,
                " linear:THE-74 ",
                "/wt/b",
                AgentDispatchStatus::Queued,
                2_000,
            ),
            dispatch(
                3,
                "linear:the-74",
                "/wt/c",
                AgentDispatchStatus::Done,
                3_000,
            ),
        ];

        let out = lanes(&rows);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].key, "linear:THE-74");
        assert_eq!(wt_names(&out[0]), vec!["a", "b", "c"]);
    }

    #[test]
    fn an_unprefixed_row_joins_the_only_provider_lane_for_its_key() {
        let rows = vec![
            dispatch(1, "THE-74", "/wt/a", AgentDispatchStatus::Running, 1_000),
            dispatch(
                2,
                "linear:THE-74",
                "/wt/b",
                AgentDispatchStatus::Queued,
                2_000,
            ),
        ];

        let out = lanes(&rows);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].key, "linear:THE-74");
        assert_eq!(wt_names(&out[0]), vec!["a", "b"]);
    }

    #[test]
    fn an_unprefixed_row_next_to_two_providers_stays_its_own_lane() {
        let rows = vec![
            dispatch(
                1,
                "jira:X-1",
                "/wt/jira",
                AgentDispatchStatus::Running,
                1_000,
            ),
            dispatch(
                2,
                "linear:X-1",
                "/wt/linear",
                AgentDispatchStatus::Running,
                2_000,
            ),
            dispatch(3, "X-1", "/wt/plain", AgentDispatchStatus::Running, 3_000),
        ];

        let out = lanes(&rows);
        assert_eq!(out.len(), 3);
        assert_eq!(key_of(&out), vec!["jira:X-1", "linear:X-1", "X-1"]);
    }

    #[test]
    fn distinct_providers_with_one_key_stay_distinct() {
        let rows = vec![
            dispatch(
                1,
                "jira:X-1",
                "/wt/jira",
                AgentDispatchStatus::Running,
                1_000,
            ),
            dispatch(
                2,
                "linear:X-1",
                "/wt/linear",
                AgentDispatchStatus::Running,
                2_000,
            ),
        ];

        assert_eq!(key_of(&lanes(&rows)), vec!["jira:X-1", "linear:X-1"]);
    }

    #[test]
    fn the_lane_key_is_stable_as_the_lane_advances() {
        let first = lanes(&[dispatch(
            1,
            "linear:THE-74",
            "/wt/a",
            AgentDispatchStatus::Running,
            1_000,
        )]);
        let advanced = lanes(&[
            dispatch(
                1,
                "linear:THE-74",
                "/wt/a",
                AgentDispatchStatus::Running,
                1_000,
            ),
            dispatch(
                2,
                "LINEAR:THE-74",
                "/wt/b",
                AgentDispatchStatus::Queued,
                2_000,
            ),
            dispatch(3, "THE-74", "/wt/c", AgentDispatchStatus::Done, 3_000),
        ]);

        assert_eq!(key_of(&first), vec!["linear:THE-74"]);
        assert_eq!(key_of(&advanced), vec!["linear:THE-74"]);
    }
}
