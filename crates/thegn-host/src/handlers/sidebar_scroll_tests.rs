//! Tests for the sidebar scroll model (`handlers::sidebar_scroll`).
//!
//! The bug these lock down: the sidebar's window used to be derived from the
//! cursor with no upper bound, so the bottom-most workspace could not be
//! reached and any unfocused rebuild yanked the window back to the top.

use crate::chrome::FrameModel;
use crate::compositor::Rect;
use crate::handlers::sidebar_persist::SidebarState;
use crate::sidebar::{RowKind, SidebarRow};
use crate::sidebar_view::{build_sidebar, max_sidebar_scroll, sidebar_geom};

/// A workspace header plus `n` worktrees — more than any test rect can show.
fn rows(n: usize) -> Vec<SidebarRow> {
    let mut v = vec![SidebarRow {
        pin_key: "app".into(),
        ..SidebarRow::base(RowKind::Workspace, 0, "app", "app")
    }];
    for i in 0..n {
        let name = format!("wt{i}");
        v.push(SidebarRow {
            worktree_path: Some(format!("/wt/{name}")),
            pin_key: format!("app/{name}"),
            ..SidebarRow::base(RowKind::Worktree, 1, &name, "app")
        });
    }
    v
}

/// A 6-row column (header + blank leaves 4 list rows) over 21 rows of content.
fn fixture() -> (FrameModel, Rect, SidebarState) {
    let model = FrameModel {
        sidebar_rows: rows(20),
        ..Default::default()
    };
    let rect = Rect {
        x: 0,
        y: 0,
        cols: 30,
        rows: 6,
    };
    (model, rect, SidebarState::default())
}

#[test]
fn wheel_scrolls_the_window_not_the_cursor() {
    let (mut model, rect, mut sb) = fixture();
    sb.cursor = 0;
    assert!(sb.scroll_by(&mut model, rect, 3), "the window moved");
    assert_eq!(sb.scroll, 3);
    assert_eq!(sb.cursor, 0, "the cursor stays where it was");
    // And back up, clamped at the top.
    sb.scroll_by(&mut model, rect, -99);
    assert_eq!(sb.scroll, 0);
    assert!(
        !sb.scroll_by(&mut model, rect, -1),
        "a wheel tick at the top reports no movement, so it can't force a repaint"
    );
}

#[test]
fn scrolling_reaches_the_last_row_and_stops() {
    // The reported bug, at the state layer: the viewport must be able to walk
    // all the way to the bottom-most row, and must not run past it into blank
    // space no matter how hard the wheel is spun.
    let (mut model, rect, mut sb) = fixture();
    sb.cursor = 0;
    for _ in 0..50 {
        sb.scroll_by(&mut model, rect, 3);
    }
    let geom = sidebar_geom(&model, rect);
    let max = max_sidebar_scroll(&geom.heights, geom.list_rows);
    assert_eq!(sb.scroll, max, "the window pins at the end of the list");

    let frame = build_sidebar(&model, rect, sb.scroll);
    let visible = model.sidebar_rows.iter().filter(|r| r.visible).count();
    assert_eq!(
        frame.rows.last().unwrap().visible_index,
        visible - 1,
        "the last row is on screen"
    );
    assert!(frame.overflow_below.is_none(), "and nothing is below it");
}

#[test]
fn an_unfocused_settle_preserves_a_scrolled_window() {
    // THE intermittent-disappearance regression. `SidebarState::rebuild` snaps
    // the cursor to the active worktree on every unfocused rebuild (hydration
    // tick, git-watch event, tab switch). That used to drag the window to the
    // top via `desired.min(cursor)`; now the cursor and the viewport are
    // independent, so a scrolled-down sidebar stays where it was put.
    let (mut model, rect, mut sb) = fixture();
    sb.focused = false;
    sb.scroll_by(&mut model, rect, 12);
    let parked = sb.scroll;
    assert!(parked > 0);

    // Simulate the snap `rebuild` performs while unfocused.
    sb.cursor = 0;
    sb.settle_scroll(&mut model, rect);
    assert_eq!(sb.scroll, parked, "an unfocused settle only clamps");
}

#[test]
fn a_focused_settle_follows_the_cursor() {
    // Keyboard navigation still pulls the window along — the reveal policy just
    // lives here now instead of inside `build_sidebar`.
    let (mut model, rect, mut sb) = fixture();
    sb.focused = true;
    sb.cursor = 20;
    sb.settle_scroll(&mut model, rect);
    let frame = build_sidebar(&model, rect, sb.scroll);
    assert!(
        frame.rows.iter().any(|p| p.visible_index == 20),
        "the cursor row is laid out (scroll={})",
        sb.scroll
    );
}

#[test]
fn settle_clamps_a_window_left_past_a_shrunken_list() {
    // Collapsing a workspace (or a hydration prune) can leave `scroll` pointing
    // past the end. Nothing resets it explicitly — the clamp is what keeps the
    // window from dangling in blank space.
    let (mut model, rect, mut sb) = fixture();
    sb.focused = false;
    sb.scroll_by(&mut model, rect, 99);
    model.sidebar_rows.truncate(3);
    sb.cursor = 0;
    sb.settle_scroll(&mut model, rect);
    assert_eq!(sb.scroll, 0, "a list that now fits scrolls back to the top");
}

#[test]
fn reanchor_pulls_an_offscreen_cursor_into_the_window() {
    // The safety net for the decoupled wheel: scroll away from the cursor and
    // the next cursor-relative key must act on a row that is actually visible.
    // Without this, `d`/`Enter` after a wheel scroll would target an invisible
    // row — the exact hazard the old cursor-walking wheel existed to avoid.
    let (mut model, rect, mut sb) = fixture();
    sb.cursor = 0;
    sb.scroll_by(&mut model, rect, 10);
    assert!(sb.reanchor_cursor(), "the cursor was off-screen");
    assert!(
        sb.cursor >= sb.first_visible && sb.cursor <= sb.last_visible,
        "cursor {} landed in the window [{}, {}]",
        sb.cursor,
        sb.first_visible,
        sb.last_visible
    );
    assert!(!sb.reanchor_cursor(), "already in the window ⇒ no move");
}

#[test]
fn reanchor_is_a_no_op_before_anything_is_laid_out() {
    // An unstamped window must not snap the cursor to 0 — that would move the
    // selection on the first keypress of a session.
    let mut sb = SidebarState {
        cursor: 7,
        ..Default::default()
    };
    assert!(!sb.reanchor_cursor());
    assert_eq!(sb.cursor, 7);
}

#[test]
fn page_step_follows_the_window_height() {
    let (mut model, rect, mut sb) = fixture();
    assert_eq!(sb.page_step(), 10, "fixed fallback before the first layout");
    sb.settle_scroll(&mut model, rect);
    let geom = sidebar_geom(&model, rect);
    assert_eq!(sb.page_step(), geom.list_rows);
}
