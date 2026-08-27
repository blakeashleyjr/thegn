//! Tests for the standalone pipeline board.
//!
//! Two invariants get disproportionate attention because they fail silently
//! rather than loudly:
//!
//! - **The scroll clamp.** `body_rows` must equal `rows - CHROME_ROWS`, or the
//!   tail of a tall board becomes unreachable with no visible symptom.
//! - **The cursor is a row ID.** The roster is re-sampled under the user; an
//!   index cursor would keep pointing at *a* row while silently changing which
//!   agent `↵` lands on.

use super::*;
use crate::telemetry::TelemetryHistory;
use thegn_core::issue::{AgentDispatch, AgentDispatchStatus};

const NOW_MS: i64 = 1_000_000;

fn hist() -> TelemetryHistory {
    TelemetryHistory::default()
}

fn ctx_at<'a>(h: &'a TelemetryHistory, screen: Rect) -> StatusCtx<'a> {
    let mut c = StatusCtx::new_for_test_on(h, screen);
    c.now_ms = NOW_MS;
    c
}

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

fn stages() -> Vec<PipelineStage> {
    vec![
        stage("architect", Some("code")),
        stage("code", Some("review")),
        stage("review", None),
    ]
}

fn d(id: i64, st: Option<&str>, parent: Option<i64>) -> AgentDispatch {
    AgentDispatch {
        id,
        issue_id: format!("THE-{id}"),
        worktree_path: format!("/wt/w{id}"),
        agent_name: format!("a{id}"),
        dispatched_at_ms: NOW_MS - 5_000,
        status: AgentDispatchStatus::Running,
        stage: st.map(str::to_string),
        parent_id: parent,
        session_id: Some(format!("s-{id}")),
        artifact_path: None,
    }
}

fn roster(rows: Vec<AgentDispatch>) -> DispatchRoster {
    DispatchRoster {
        rows,
        stage_order: stages().iter().map(|s| s.name.clone()).collect(),
    }
}

/// The standard fixture: architect(1) → code(2, 3) with 3 a chunk of 2.
fn full_roster() -> DispatchRoster {
    roster(vec![
        d(1, Some("architect"), None),
        d(2, Some("code"), Some(1)),
        d(3, Some("code"), Some(2)),
    ])
}

fn open_on(screen: Rect, r: &DispatchRoster) -> (PipelineBoard, TelemetryHistory) {
    let h = hist();
    let b = PipelineBoard::open(r, &stages(), &ctx_at(&h, screen));
    (b, h)
}

fn open_board() -> (PipelineBoard, TelemetryHistory) {
    open_on(Rect::full(140, 40), &full_roster())
}

fn key(b: &mut PipelineBoard, k: KeyCode) -> BoardOutcome {
    b.handle_key(&k, Modifiers::NONE)
}

fn ch(b: &mut PipelineBoard, c: char) -> BoardOutcome {
    key(b, KeyCode::Char(c))
}

fn render_text(b: &PipelineBoard, w: usize, h: usize) -> String {
    let mut s = Surface::new(w, h);
    b.render(&mut s, Rect::full(w, h));
    s.screen_chars_to_string()
}

// --- Chrome / geometry ---------------------------------------------------

#[test]
fn body_rows_matches_the_reserved_chrome_exactly() {
    // The clamp bug with no visible symptom: `body_rows` must be the interior
    // minus the rail and the footer, or the tail of a long board is unreachable.
    let (b, _h) = open_board();
    assert_eq!(b.body_rows, b.rows - CHROME_ROWS);
    assert_eq!(CHROME_ROWS, view::RAIL_ROWS + 1);
}

#[test]
fn the_box_is_sized_the_way_the_layer_will_clamp_it() {
    // `dims` and `layer::box_dims` must agree, or the scroll clamp measures
    // against a viewport the layer then shrinks.
    for screen in [Rect::full(140, 40), Rect::full(80, 24), Rect::full(60, 18)] {
        let (b, _h) = open_on(screen, &full_roster());
        let boxr = b.box_rect(screen).expect("a box fits");
        assert_eq!(boxr.cols, b.cols + 4, "border + pad, at {screen:?}");
        assert_eq!(boxr.rows, b.rows + 2, "top/bottom border, at {screen:?}");
        assert!(boxr.x + boxr.cols <= screen.cols);
        assert!(boxr.y + boxr.rows <= screen.rows);
    }
}

#[test]
fn the_rail_the_body_and_the_legend_are_all_drawn() {
    let (b, _h) = open_board();
    let text = render_text(&b, 140, 40);
    assert!(text.contains("pipeline"), "box title missing: {text}");
    for want in ["architect", "code", "review"] {
        assert!(text.contains(want), "stage `{want}` missing: {text}");
    }
    assert!(
        text.contains("architect-agent"),
        "agent rail missing: {text}"
    );
    assert!(text.contains("w1"), "a row's worktree missing: {text}");
    // Every bound key is legible in the footer.
    for k in ["kj", "hl", "enter", "spc", "esc"] {
        assert!(text.contains(k), "legend key `{k}` missing: {text}");
    }
}

#[test]
fn a_configured_but_empty_stage_is_still_a_visible_column() {
    let (b, _h) = open_on(Rect::full(140, 40), &roster(vec![]));
    assert_eq!(b.board.columns.len(), 3);
    let text = render_text(&b, 140, 40);
    for want in ["architect", "code", "review"] {
        assert!(text.contains(want), "empty stage `{want}` hidden: {text}");
    }
}

// --- Navigation ----------------------------------------------------------

#[test]
fn the_cursor_starts_on_the_first_row_of_the_first_column() {
    let (b, _h) = open_board();
    assert_eq!(b.col, 0);
    assert_eq!(b.cursor, Some(1));
}

#[test]
fn left_and_right_walk_stage_columns_and_up_down_walk_rows() {
    let (mut b, _h) = open_board();
    // architect has one row; code has two.
    ch(&mut b, 'j');
    assert_eq!(b.cursor, Some(1), "a one-row column clamps at both ends");
    ch(&mut b, 'l');
    assert_eq!((b.col, b.cursor), (1, Some(2)));
    key(&mut b, KeyCode::DownArrow);
    assert_eq!(b.cursor, Some(3));
    key(&mut b, KeyCode::UpArrow);
    assert_eq!(b.cursor, Some(2));
    key(&mut b, KeyCode::RightArrow);
    assert_eq!((b.col, b.cursor), (2, None), "review is empty");
    key(&mut b, KeyCode::RightArrow);
    assert_eq!(b.col, 2, "the last column clamps");
    ch(&mut b, 'h');
    assert_eq!((b.col, b.cursor), (1, Some(2)));
}

#[test]
fn the_cursor_follows_the_row_id_when_the_roster_moves_under_it() {
    let (mut b, h) = open_board();
    ch(&mut b, 'l');
    key(&mut b, KeyCode::DownArrow);
    assert_eq!(b.cursor, Some(3));
    // A new row lands ahead of the selection (older stamp). An INDEX cursor
    // would now be pointing at row 2; an ID cursor stays on 3.
    let mut extra = d(9, Some("code"), None);
    extra.dispatched_at_ms = NOW_MS - 10_000;
    let mut rows = full_roster().rows;
    rows.push(extra);
    b.refresh(&roster(rows), &stages(), &ctx_at(&h, Rect::full(140, 40)));
    assert_eq!(
        b.cursor,
        Some(3),
        "the cursor is a row identity, not an index"
    );

    // …and when the selected row vanishes, it lands on the neighbour rather
    // than snapping back to the top.
    b.refresh(
        &roster(vec![
            d(1, Some("architect"), None),
            d(2, Some("code"), Some(1)),
        ]),
        &stages(),
        &ctx_at(&h, Rect::full(140, 40)),
    );
    assert_eq!(b.cursor, Some(2));
}

// --- Actions -------------------------------------------------------------

#[test]
fn enter_raises_a_jump_for_the_selected_row_and_drains_once() {
    let (mut b, _h) = open_board();
    ch(&mut b, 'l');
    assert_eq!(key(&mut b, KeyCode::Enter), BoardOutcome::Action);
    assert_eq!(
        b.take_action(),
        Some(BoardAction::Jump(PipelineJump {
            worktree: "/wt/w2".into(),
            session: Some("s-2".into()),
        }))
    );
    assert_eq!(b.take_action(), None);
}

#[test]
fn a_row_with_no_worktree_says_so_rather_than_raising_a_dead_jump() {
    let mut row = d(1, Some("code"), None);
    row.worktree_path = String::new();
    let (mut b, _h) = open_on(Rect::full(140, 40), &roster(vec![row]));
    ch(&mut b, 'l');
    assert_eq!(key(&mut b, KeyCode::Enter), BoardOutcome::Pending);
    assert_eq!(b.take_action(), None);
    assert!(render_text(&b, 140, 40).contains("no worktree"));
}

#[test]
fn a_click_selects_a_row_and_a_second_click_activates_it() {
    let (mut b, _h) = open_board();
    let inner = b.inner_rect().expect("a box fits");
    // The `code` column's first body line.
    let x = inner.x + b.board.col_w + 1;
    let y = inner.y + view::RAIL_ROWS;
    assert_eq!(b.handle_click(x, y), BoardOutcome::Pending);
    assert_eq!((b.col, b.cursor), (1, Some(2)));
    assert_eq!(b.handle_click(x, y), BoardOutcome::Action);
    assert!(matches!(b.take_action(), Some(BoardAction::Jump(_))));
    // The rail and the footer are chrome, not rows.
    assert_eq!(b.handle_click(x, inner.y), BoardOutcome::Pending);
}

// --- Toggles -------------------------------------------------------------

#[test]
fn space_freezes_the_view_and_stops_paying_for_samples() {
    let (mut b, h) = open_board();
    assert!(b.wants_dispatches());
    ch(&mut b, ' ');
    assert!(b.is_frozen());
    assert!(!b.wants_dispatches(), "a frozen board asks for no samples");
    // A refresh while frozen changes nothing at all.
    let before = b.board.clone();
    assert!(!b.refresh(&roster(vec![]), &stages(), &ctx_at(&h, Rect::full(140, 40))));
    assert_eq!(b.board, before);
    ch(&mut b, ' ');
    assert!(b.wants_dispatches());
}

#[test]
fn x_hides_finished_rows_without_lying_about_the_stage_count() {
    let mut done = d(3, Some("code"), Some(2));
    done.status = AgentDispatchStatus::Done;
    let r = roster(vec![
        d(1, Some("architect"), None),
        d(2, Some("code"), Some(1)),
        done,
    ]);
    let (mut b, h) = open_on(Rect::full(140, 40), &r);
    assert_eq!(b.board.columns[1].rows.len(), 2);
    ch(&mut b, 'x');
    b.rebuild_after_key(&r, &stages(), &ctx_at(&h, Rect::full(140, 40)));
    assert_eq!(b.board.columns[1].rows.len(), 1);
    assert_eq!(
        b.board.columns[1].head.total, 2,
        "the header still counts every row"
    );
    assert!(render_text(&b, 140, 40).contains("show finished"));
}

// --- Key contract --------------------------------------------------------

#[test]
fn unbound_keys_pass_through_so_the_opening_chord_still_toggles_it_shut() {
    let (mut b, _h) = open_board();
    // The `open-pipeline-board` chord itself.
    assert_eq!(
        b.handle_key(&KeyCode::Char('b'), Modifiers::ALT),
        BoardOutcome::Passthrough
    );
    // Ctrl-g is the global key lock, never "close the board".
    assert_eq!(
        b.handle_key(&KeyCode::Char('g'), Modifiers::CTRL),
        BoardOutcome::Passthrough
    );
    // A letter the board does not bind is the keymap's, not ours.
    assert_eq!(ch(&mut b, 'z'), BoardOutcome::Passthrough);
    // …but the ones it does bind are consumed.
    assert_eq!(ch(&mut b, 'q'), BoardOutcome::Close);
    assert_eq!(key(&mut b, KeyCode::Escape), BoardOutcome::Close);
    assert_eq!(
        b.handle_key(&KeyCode::Char('c'), Modifiers::CTRL),
        BoardOutcome::Close
    );
}

// --- Scrolling -----------------------------------------------------------

#[test]
fn a_tall_board_scrolls_and_the_cursor_stays_in_view() {
    let rows: Vec<AgentDispatch> = (1..=60)
        .map(|i| {
            let mut r = d(i, Some("code"), None);
            r.dispatched_at_ms = NOW_MS - (100 - i);
            r
        })
        .collect();
    let (mut b, _h) = open_on(Rect::full(140, 24), &roster(rows));
    ch(&mut b, 'l');
    assert_eq!(b.scroll, 0);
    for _ in 0..59 {
        key(&mut b, KeyCode::DownArrow);
    }
    assert_eq!(b.cursor, Some(60));
    assert!(b.scroll > 0, "the cursor pulled the viewport down");
    assert!(b.scroll <= b.scroll_max());
    // The last line is reachable — the clamp measured against the real body.
    assert_eq!(b.scroll, b.lines.len() - b.body_rows);
    // The wheel clamps at both ends.
    b.wheel(-1000);
    assert_eq!(b.scroll, 0);
    b.wheel(1000);
    assert_eq!(b.scroll, b.scroll_max());
}

#[test]
fn a_narrow_terminal_stacks_and_every_row_is_still_reachable() {
    let (b, _h) = open_on(Rect::full(60, 24), &full_roster());
    assert_eq!(b.board.mode, layout::Mode::Stacked);
    let ids: Vec<i64> = b.lines.iter().filter_map(|l| l.row_id).collect();
    assert_eq!(ids, vec![1, 2, 3]);
}

#[test]
fn a_stacked_click_resolves_the_row_on_the_line() {
    let (mut b, _h) = open_on(Rect::full(60, 24), &full_roster());
    let inner = b.inner_rect().expect("a box fits");
    // Line 0 is the `architect` group heading; line 1 is its row.
    let y = inner.y + view::RAIL_ROWS + 1;
    assert_eq!(b.handle_click(inner.x + 1, y), BoardOutcome::Pending);
    assert_eq!((b.col, b.cursor), (0, Some(1)));
}
