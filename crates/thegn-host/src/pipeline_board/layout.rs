//! The board's **pure** geometry: which stage columns exist, in what order, how
//! wide they are, which rows hang off which edge, and which rows have blown
//! their advisory budget.
//!
//! Nothing here imports `termwiz`, a `Surface`, the model or a clock — it takes
//! `now_ms`. That is the whole point: the rules that can be wrong in ways a
//! screenshot won't show (column precedence, the columns↔stacked boundary, edge
//! classification, the stall predicate) live in a function with unit tests, and
//! [`super::view`] only turns the answer into glyphs.
//!
//! # Doctrine
//!
//! `concurrency` and `timeout_secs` are **advisory** (see
//! `thegn_core::config_pipeline`): this module DISPLAYS them and derives a
//! visual stall cue from one of them. No code path here advances a stage,
//! enforces a limit or times a row out.

use std::collections::{BTreeMap, BTreeSet};

use thegn_core::config_pipeline::PipelineStage;

use crate::monitor_pipeline::{PipelineRow, UNSTAGED, stage_sequence};

/// Narrowest a stage column may be before the board gives up on side-by-side.
///
/// Below this a column holds a status glyph, an edge mark and about eight cells
/// of worktree name — at which point the left-to-right reading it exists for is
/// gone and a stacked list is simply more honest.
pub(crate) const MIN_COL_W: usize = 22;

/// How the board is laid out at the current width.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    /// Stage columns side by side — the pipeline reads left to right.
    Columns,
    /// One stage group after another down the page, for a terminal too narrow
    /// to give every column [`MIN_COL_W`].
    Stacked,
}

/// A stage column's header facts. All from `[[pipeline.stages]]`; `live`/`total`
/// are counted off the roster.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct StageHead {
    pub name: String,
    /// The `[[agents]]`/`[[tools]]` entry configured to run the stage. Empty
    /// for a stage that exists only on the roster.
    pub agent: String,
    /// Rows of this stage that are still live ([`is_active`]).
    ///
    /// [`is_active`]: thegn_core::issue::AgentDispatchStatus::is_active
    pub live: usize,
    /// EVERY row of this stage, counted before hide-finished drops any — a
    /// header that under-reported while rows were hidden would be a lie.
    pub total: usize,
    /// Advisory per-stage worker budget. Displayed, never enforced.
    pub concurrency: u32,
    /// Advisory per-row budget in seconds; `0` = none. Drives [`stalled`].
    pub timeout_secs: u64,
    /// The stage this one flows into. `None` = terminal (or unconfigured).
    pub next: Option<String>,
    /// Whether the stage is configured at all, or only present on the roster.
    pub configured: bool,
}

/// How a row connects to its parent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Edge {
    /// No parent to draw. The renderer still spends a cell on it, so every
    /// column stays exactly aligned.
    None,
    /// The parent sits in the PREVIOUS stage column — the left-to-right flow.
    Inbound,
    /// The parent sits in THIS column: a fan-out inside one stage, drawn with
    /// tree connectors at [`PipelineRow::depth`]. `last` picks the corner.
    Child { last: bool },
}

/// One drawable board row: the folded roster row plus what the board knows
/// about it that the fold cannot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BoardRow {
    pub row: PipelineRow,
    pub edge: Edge,
    /// This row is some other row's [`Edge::Inbound`] parent — it carries an
    /// outbound tick so the flow reads in both directions.
    pub outbound: bool,
    /// Live past its stage's advisory `timeout_secs`. A cue, never an action.
    pub stalled: bool,
}

/// One stage column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Column {
    pub head: StageHead,
    /// Rows in fold order, after hide-finished.
    pub rows: Vec<BoardRow>,
}

/// The laid-out board.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Board {
    pub mode: Mode,
    pub columns: Vec<Column>,
    /// Width of one stage column in [`Mode::Columns`] (the full width in
    /// [`Mode::Stacked`]). Never zero unless the board itself has no width.
    pub col_w: usize,
}

impl Board {
    /// The tallest column — how many body lines [`Mode::Columns`] needs.
    pub fn tallest(&self) -> usize {
        self.columns.iter().map(|c| c.rows.len()).max().unwrap_or(0)
    }

    /// Total rows on the board, after hide-finished.
    #[allow(dead_code)] // read by tests
    pub fn row_count(&self) -> usize {
        self.columns.iter().map(|c| c.rows.len()).sum()
    }
}

/// Has this row been live longer than its stage's advisory budget?
///
/// `timeout_secs == 0` means "no budget", so nothing is ever stalled under it.
/// A terminal (or queued-then-finished) row is never stalled either: only work
/// that is still supposedly happening can be late.
///
/// Pure and separately tested because it is the one place a `u64` seconds
/// budget meets an `i64` millisecond clock: the multiply saturates and the
/// conversion clamps, so an absurd `timeout_secs` reads as "never stalled"
/// rather than wrapping into "always stalled".
pub(crate) fn stalled(row: &PipelineRow, timeout_secs: u64, now_ms: i64) -> bool {
    if timeout_secs == 0 || !row.status.is_active() {
        return false;
    }
    let budget_ms = i64::try_from(timeout_secs.saturating_mul(1000)).unwrap_or(i64::MAX);
    now_ms.saturating_sub(row.dispatched_at_ms) > budget_ms
}

/// Lay the folded roster rows out as stage columns.
///
/// `rows` must be [`crate::monitor_pipeline::ordered_rows`]' output (it carries
/// the fold's grouping and depth); `stages` is `[[pipeline.stages]]` in
/// declaration order — the column order. `hide_finished` drops terminal rows
/// AFTER the header counts are taken, so a stage still reports how many rows it
/// really has.
pub(crate) fn board(
    rows: &[PipelineRow],
    stages: &[PipelineStage],
    width: usize,
    now_ms: i64,
    hide_finished: bool,
) -> Board {
    let configured: Vec<String> = stages
        .iter()
        .filter_map(|s| s.stage_name())
        .map(str::to_string)
        .collect();
    let present: BTreeSet<String> = rows.iter().map(|r| r.stage.clone()).collect();
    // `keep_empty`: a configured stage with no rows is still a column.
    let order = stage_sequence(&present, &configured, true);

    // stage name -> column index, and row id -> column index. Both are needed
    // before any edge can be classified, so this is a two-pass fold.
    let col_of: BTreeMap<&str, usize> = order
        .iter()
        .enumerate()
        .map(|(i, n)| (n.as_str(), i))
        .collect();
    let col_of_id: BTreeMap<i64, usize> = rows
        .iter()
        .filter_map(|r| col_of.get(r.stage.as_str()).map(|&c| (r.id, c)))
        .collect();

    // Which rows are somebody's inbound parent, and which are the LAST child of
    // their parent inside their own column (the tree corner).
    let mut outbound: BTreeSet<i64> = BTreeSet::new();
    let mut last_child: BTreeMap<i64, i64> = BTreeMap::new(); // parent -> last child id
    for r in rows {
        let (Some(parent), Some(&mine)) = (r.parent_id, col_of_id.get(&r.id)) else {
            continue;
        };
        match col_of_id.get(&parent) {
            Some(&pc) if pc + 1 == mine => {
                outbound.insert(parent);
            }
            // Same column AND the fold indented it: a genuine in-stage child.
            Some(&pc) if pc == mine && r.depth > 0 => {
                last_child.insert(parent, r.id);
            }
            _ => {}
        }
    }

    let mut columns: Vec<Column> = order
        .iter()
        .map(|name| {
            let stage = stages
                .iter()
                .find(|s| s.stage_name() == Some(name.as_str()));
            Column {
                head: StageHead {
                    name: name.clone(),
                    agent: stage
                        .map(|s| s.agent.trim().to_string())
                        .unwrap_or_default(),
                    live: 0,
                    total: 0,
                    concurrency: stage.map(|s| s.concurrency).unwrap_or(0),
                    timeout_secs: stage.map(|s| s.timeout_secs).unwrap_or(0),
                    next: stage.and_then(|s| s.next_name()).map(str::to_string),
                    configured: stage.is_some(),
                },
                rows: Vec::new(),
            }
        })
        .collect();

    for r in rows {
        let Some(&ci) = col_of.get(r.stage.as_str()) else {
            continue;
        };
        let head = &mut columns[ci].head;
        head.total += 1;
        if r.status.is_active() {
            head.live += 1;
        }
        if hide_finished && r.status.is_terminal() {
            continue;
        }
        let edge = match r.parent_id {
            Some(p) => match col_of_id.get(&p) {
                Some(&pc) if pc + 1 == ci => Edge::Inbound,
                Some(&pc) if pc == ci && r.depth > 0 => Edge::Child {
                    last: last_child.get(&p) == Some(&r.id),
                },
                _ => Edge::None,
            },
            None => Edge::None,
        };
        let timeout = columns[ci].head.timeout_secs;
        columns[ci].rows.push(BoardRow {
            edge,
            outbound: outbound.contains(&r.id),
            stalled: stalled(r, timeout, now_ms),
            row: r.clone(),
        });
    }

    // Side by side only while every column still earns its minimum. An empty
    // board has no columns to fit, so it stacks (there is nothing to read left
    // to right).
    let mode = if !columns.is_empty() && width >= MIN_COL_W.saturating_mul(columns.len()) {
        Mode::Columns
    } else {
        Mode::Stacked
    };
    let col_w = match mode {
        Mode::Columns => width / columns.len().max(1),
        Mode::Stacked => width,
    };
    Board {
        mode,
        columns,
        col_w,
    }
}

/// The `UNSTAGED` column is never configured, so it never carries stage facts.
/// Re-exported for the view's header rail, which labels it differently.
pub(crate) fn is_unstaged(head: &StageHead) -> bool {
    head.name == UNSTAGED
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::issue::{AgentDispatch, AgentDispatchStatus};

    fn stage(name: &str, next: Option<&str>) -> PipelineStage {
        PipelineStage {
            name: name.into(),
            agent: format!("{name}-agent"),
            concurrency: 2,
            timeout_secs: 60,
            next: next.map(str::to_string),
            ..PipelineStage::default()
        }
    }

    fn d(id: i64, st: Option<&str>, parent: Option<i64>, at: i64) -> AgentDispatch {
        AgentDispatch {
            id,
            issue_id: format!("THE-{id}"),
            worktree_path: format!("/wt/w{id}"),
            agent_name: format!("a{id}"),
            dispatched_at_ms: at,
            status: AgentDispatchStatus::Running,
            stage: st.map(str::to_string),
            parent_id: parent,
            session_id: None,
            artifact_path: None,
            note: None,
            chunk_path: None,
            report: None,
        }
    }

    fn fold(rows: &[AgentDispatch], stages: &[PipelineStage], now_ms: i64) -> Vec<PipelineRow> {
        let names: Vec<String> = stages
            .iter()
            .filter_map(|s| s.stage_name())
            .map(str::to_string)
            .collect();
        crate::monitor_pipeline::ordered_rows(rows, &names, now_ms)
    }

    fn names(b: &Board) -> Vec<&str> {
        b.columns.iter().map(|c| c.head.name.as_str()).collect()
    }

    #[test]
    fn columns_follow_config_order_then_unknown_stages_then_unstaged() {
        let stages = [stage("architect", Some("code")), stage("code", None)];
        let rows = fold(
            &[
                d(1, Some("zeta"), None, 10),
                d(2, None, None, 20),
                d(3, Some("code"), None, 30),
            ],
            &stages,
            0,
        );
        let b = board(&rows, &stages, 200, 0, false);
        assert_eq!(names(&b), vec!["architect", "code", "zeta", UNSTAGED]);
    }

    #[test]
    fn a_configured_stage_with_no_rows_is_still_a_column() {
        // The org chart is what the board draws; a stage nobody has reached yet
        // vanishing would read as a misconfiguration.
        let stages = [stage("architect", Some("code")), stage("code", None)];
        let b = board(&[], &stages, 200, 0, false);
        assert_eq!(names(&b), vec!["architect", "code"]);
        assert!(b.columns.iter().all(|c| c.rows.is_empty()));
        assert_eq!(b.columns[0].head.total, 0);
        assert_eq!(b.columns[0].head.next.as_deref(), Some("code"));
        assert_eq!(b.columns[0].head.agent, "architect-agent");
        assert_eq!(b.columns[0].head.concurrency, 2);
        assert!(b.columns[0].head.configured);
    }

    #[test]
    fn an_empty_board_with_no_config_has_no_columns_and_stacks() {
        let b = board(&[], &[], 200, 0, false);
        assert!(b.columns.is_empty());
        assert_eq!(b.mode, Mode::Stacked);
        assert_eq!(b.row_count(), 0);
        assert_eq!(b.tallest(), 0);
    }

    #[test]
    fn mode_flips_exactly_at_min_col_w_times_the_column_count() {
        let stages = [stage("a", Some("b")), stage("b", None)];
        let exact = MIN_COL_W * 2;
        assert_eq!(board(&[], &stages, exact, 0, false).mode, Mode::Columns);
        assert_eq!(
            board(&[], &stages, exact - 1, 0, false).mode,
            Mode::Stacked,
            "one cell short of the budget must stack"
        );
        // Zero width can never be columns.
        assert_eq!(board(&[], &stages, 0, 0, false).mode, Mode::Stacked);
        // …and the per-column width is the honest split.
        assert_eq!(board(&[], &stages, exact, 0, false).col_w, MIN_COL_W);
        assert_eq!(board(&[], &stages, 0, 0, false).col_w, 0);
    }

    #[test]
    fn a_parent_in_the_previous_column_is_an_inbound_edge_and_ticks_its_parent() {
        let stages = [stage("architect", Some("code")), stage("code", None)];
        let rows = fold(
            &[
                d(1, Some("architect"), None, 10),
                d(2, Some("code"), Some(1), 20),
            ],
            &stages,
            0,
        );
        let b = board(&rows, &stages, 200, 0, false);
        assert_eq!(b.columns[0].rows[0].edge, Edge::None);
        assert!(
            b.columns[0].rows[0].outbound,
            "the architect row must show the flow out"
        );
        assert_eq!(b.columns[1].rows[0].edge, Edge::Inbound);
        assert!(!b.columns[1].rows[0].outbound);
    }

    #[test]
    fn a_parent_in_the_same_column_draws_tree_connectors_and_marks_the_last_child() {
        let stages = [stage("code", None)];
        let rows = fold(
            &[
                d(1, Some("code"), None, 10),
                d(2, Some("code"), Some(1), 20),
                d(3, Some("code"), Some(1), 30),
            ],
            &stages,
            0,
        );
        let b = board(&rows, &stages, 200, 0, false);
        let edges: Vec<Edge> = b.columns[0].rows.iter().map(|r| r.edge).collect();
        assert_eq!(
            edges,
            vec![
                Edge::None,
                Edge::Child { last: false },
                Edge::Child { last: true },
            ]
        );
        assert!(!b.columns[0].rows[0].outbound, "same column is not a flow");
    }

    #[test]
    fn a_parent_two_columns_back_or_gone_is_no_edge_at_all() {
        // Only the IMMEDIATELY previous column is a flow; a longer hop (or a
        // pruned parent) must not draw a connector to nothing.
        let stages = [stage("a", None), stage("b", None), stage("c", None)];
        let rows = fold(
            &[
                d(1, Some("a"), None, 10),
                d(2, Some("c"), Some(1), 20),
                d(3, Some("b"), Some(999), 30),
            ],
            &stages,
            0,
        );
        let b = board(&rows, &stages, 200, 0, false);
        assert_eq!(b.columns[2].rows[0].edge, Edge::None);
        assert_eq!(b.columns[1].rows[0].edge, Edge::None);
        assert!(!b.columns[0].rows[0].outbound);
    }

    #[test]
    fn hide_finished_drops_rows_but_never_the_header_count() {
        let stages = [stage("code", None)];
        let mut done = d(2, Some("code"), None, 20);
        done.status = AgentDispatchStatus::Done;
        let rows = fold(&[d(1, Some("code"), None, 10), done], &stages, 0);
        let shown = board(&rows, &stages, 200, 0, false);
        assert_eq!(shown.columns[0].rows.len(), 2);
        let hidden = board(&rows, &stages, 200, 0, true);
        assert_eq!(hidden.columns[0].rows.len(), 1);
        assert_eq!(hidden.columns[0].rows[0].row.id, 1);
        assert_eq!(
            (hidden.columns[0].head.total, hidden.columns[0].head.live),
            (2, 1),
            "the header still reports the truth about the stage"
        );
    }

    #[test]
    fn stall_needs_an_active_row_a_budget_and_an_elapsed_one() {
        let stages = [stage("code", None)]; // timeout_secs = 60
        let rows = fold(&[d(1, Some("code"), None, 0)], &stages, 0);
        let r = &rows[0];
        assert!(!stalled(r, 60, 60_000), "exactly at budget is not over it");
        assert!(stalled(r, 60, 60_001));
        assert!(!stalled(r, 0, i64::MAX), "no budget, never stalled");
        // An absurd budget saturates rather than wrapping into "always late".
        assert!(!stalled(r, u64::MAX, i64::MAX));

        let mut fin = rows[0].clone();
        fin.status = AgentDispatchStatus::Done;
        assert!(!stalled(&fin, 60, i64::MAX), "a finished row is not late");
        // A clock behind the row reads as fresh, never as a huge negative age.
        assert!(!stalled(r, 60, -1));
    }

    #[test]
    fn the_stall_cue_rides_the_stage_that_owns_the_row() {
        let stages = [stage("code", None)];
        let rows = fold(&[d(1, Some("code"), None, 0)], &stages, 500_000);
        assert!(board(&rows, &stages, 200, 500_000, false).columns[0].rows[0].stalled);
        // An UNSTAGED row has no configured budget, so it can never stall.
        let orphan = fold(&[d(9, None, None, 0)], &stages, 500_000);
        let b = board(&orphan, &stages, 200, 500_000, false);
        let un = b
            .columns
            .iter()
            .find(|c| is_unstaged(&c.head))
            .expect("unstaged column");
        assert!(!un.rows[0].stalled);
        assert!(!un.head.configured);
    }
}
