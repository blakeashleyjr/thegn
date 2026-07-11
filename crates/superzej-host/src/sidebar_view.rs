//! Sidebar layout + paint: the single `build_sidebar` pass that both the
//! renderer and the mouse hit-test derive from, the row/rail line composers,
//! the metrics section, and the row context-menu overlay.
//!
//! Extracted from `chrome.rs` (which orchestrates the whole frame) so the
//! sidebar's view logic has a home that can grow — hit-testing, drag feedback
//! and the menu all live here, next to the geometry they depend on.

use termwiz::color::ColorAttribute;
use termwiz::surface::Surface;

use superzej_core::theme;

use crate::chrome::{
    FrameModel, S, col, draw_text, draw_text_bold, fill, focus_rgb, panel_rgb, theme_color,
    with_palette,
};
use crate::compositor::Rect;

/// A row context menu (item 27): a short list of actions scoped to the row the
/// cursor sat on when it opened.
#[derive(Debug, Clone, Default)]
pub struct RowMenu {
    /// Visible-row index the menu is anchored to (where it's drawn).
    pub anchor: usize,
    pub entries: Vec<RowMenuEntry>,
    pub cursor: usize,
    /// The stable pin_key of the row this menu was opened for.
    pub target_pin_key: String,
}

#[derive(Debug, Clone)]
pub struct RowMenuEntry {
    pub label: String,
    /// A stable id the event loop dispatches on (e.g. "open", "close", "pin").
    /// Empty = a non-selectable separator rule.
    pub id: String,
    /// The keyboard shortcut that fires this action directly (rendered as a
    /// right-aligned chip) — the menu doubles as key discovery.
    pub key: Option<&'static str>,
    /// Destructive actions render red.
    pub danger: bool,
}

impl RowMenuEntry {
    pub fn new(id: &str, label: &str, key: Option<&'static str>) -> Self {
        RowMenuEntry {
            label: label.into(),
            id: id.into(),
            key,
            danger: false,
        }
    }

    pub fn danger(mut self) -> Self {
        self.danger = true;
        self
    }

    pub fn separator() -> Self {
        RowMenuEntry {
            label: String::new(),
            id: String::new(),
            key: None,
            danger: false,
        }
    }

    pub fn is_separator(&self) -> bool {
        self.id.is_empty()
    }
}

/// The nearest selectable entry index stepping from `from` by `dir` (+1/-1),
/// skipping separators; `from` itself is not required to be selectable.
/// Returns `from` unchanged when no selectable entry exists in that direction.
pub fn menu_step(entries: &[RowMenuEntry], from: usize, dir: i32) -> usize {
    let mut i = from as i64;
    loop {
        i += dir as i64;
        if i < 0 || i as usize >= entries.len() {
            return from;
        }
        if !entries[i as usize].is_separator() {
            return i as usize;
        }
    }
}

pub fn draw_sidebar(surface: &mut Surface, rect: Rect, model: &FrameModel) {
    fill(surface, rect, col(S::Panel));
    if rect.cols == 0 || rect.rows == 0 {
        return;
    }
    // The slim rail is its own compact language (activity dot + initial); the
    // full panel renders the header, the laid-out rows, metrics, and menu.
    if model.sidebar_rail {
        draw_sidebar_rail(surface, rect, model);
        return;
    }
    let accent = theme_color(model.accent_or_default());

    // Header: the live filter input, or the "WORKSPACES" section title — in
    // the accent so the column titles pop against the tinted zone.
    if model.sidebar_filtering || !model.sidebar_filter.is_empty() {
        let header = format!(" /{}", model.sidebar_filter);
        draw_text(
            surface,
            rect.x,
            rect.y,
            &header,
            accent,
            col(S::Panel),
            rect.cols,
        );
    } else {
        draw_text_bold(
            surface,
            rect.x,
            rect.y,
            " WORKSPACES",
            col(S::Text),
            col(S::Panel),
            rect.cols,
        );
    }

    // One layout pass: the renderer and the click hit-test (`sidebar_hits`)
    // both derive geometry from `build_sidebar`, so paint and clicks can never
    // drift apart (the same contract the panel uses via `build_panel`).
    let frame = build_sidebar(model, rect, model.sidebar_scroll);
    for p in &frame.rows {
        // `draw_lines` fills the placement's full width in `bg`; every composed
        // line begins with a 1-col gutter so the cursor bar can overpaint col 0
        // without clobbering content.
        crate::seg::draw_lines(
            surface,
            Rect {
                x: rect.x,
                y: p.y,
                cols: rect.cols,
                rows: p.height,
            },
            &p.lines,
            p.bg,
        );
        // Left-edge accent bar marks the cursor row and spans its full height
        // (including the expanded detail line). It persists when the sidebar
        // loses focus but dims — bright focus color while focused, a muted
        // focus-over-panel tint otherwise — so a resting selection is still
        // visible without being mistaken for focus.
        if p.cursor_bar {
            let bar_fg = if model.sidebar_focused {
                col(S::Focus)
            } else {
                theme_color(&theme::blend_over(&focus_rgb(), &panel_rgb(), 0.5))
            };
            let bar = crate::caps::active_glyphs().half_block_r;
            for dy in 0..p.height {
                draw_text(surface, rect.x, p.y + dy, bar, bar_fg, tok_col(p.bg), 1);
            }
        }
    }

    if let Some(mrect) = frame.metrics {
        draw_metrics_section(surface, mrect, model);
    }

    // Live drag affordance: an accent insertion rule at the drop boundary
    // (the target-row highlight rides `row_bg`). Painted over the rows but
    // under the menu.
    if let Some(drag) = &model.sidebar_drag {
        let rule_y = match drag.spot {
            DragSpotViz::InsertBefore(i) => frame
                .rows
                .iter()
                .find(|p| p.visible_index == i)
                .map(|p| p.y),
            DragSpotViz::InsertAfter(i) => frame
                .rows
                .iter()
                .find(|p| p.visible_index == i)
                .map(|p| p.y + p.height),
            _ => None,
        };
        if let Some(y) = rule_y.filter(|y| *y < rect.y + rect.rows) {
            let rule = crate::caps::active_glyphs().box_h.repeat(rect.cols);
            draw_text(
                surface,
                rect.x,
                y,
                &rule,
                col(S::Focus),
                col(S::Panel),
                rect.cols,
            );
        }
    }

    // Row context menu overlay (item 27) — painted last so it stacks above the
    // rows and the metrics section.
    if let Some(menu) = &model.sidebar_menu {
        draw_row_menu(surface, rect, &frame, menu, accent);
    }
}

/// Where the row menu paints: anchored just under its target row's *rendered*
/// placement (so scroll offset and two-line detail rows are respected), clamped
/// so every entry fits inside the sidebar rect. Shared by paint and the mouse
/// hit-test so clicks can never drift from pixels.
pub(crate) fn menu_rect(rect: Rect, frame: &SidebarFrame, menu: &RowMenu) -> Rect {
    let below = frame
        .rows
        .iter()
        .find(|p| p.visible_index == menu.anchor)
        .map(|p| p.y + p.height)
        // Anchor row scrolled off: fall back to the top of the list area.
        .unwrap_or(rect.y + 2);
    let rows = menu.entries.len().min(rect.rows);
    let max_top = (rect.y + rect.rows).saturating_sub(rows).max(rect.y);
    Rect {
        x: rect.x,
        y: below.clamp(rect.y, max_top),
        cols: rect.cols,
        rows,
    }
}

/// The slim collapsed rail: one row per visible worktree, an activity dot in
/// its state color plus the first letter of the label. `model.sidebar_scroll`
/// keeps the cursor in view.
fn draw_sidebar_rail(surface: &mut Surface, rect: Rect, model: &FrameModel) {
    let frame = build_sidebar(model, rect, model.sidebar_scroll);
    for p in &frame.rows {
        crate::seg::draw_lines(
            surface,
            Rect {
                x: rect.x,
                y: p.y,
                cols: rect.cols,
                rows: p.height,
            },
            &p.lines,
            p.bg,
        );
    }
}

/// A laid-out sidebar row: which visible-row it is, where it sits, how tall it
/// is, and the composed line(s) + background to paint. The cursor row may be
/// two lines tall (the expanded detail tier); a section heading carries a
/// leading blank gap.
pub(crate) struct SidebarPlacement {
    pub visible_index: usize,
    pub y: usize,
    pub height: usize,
    pub lines: Vec<crate::seg::Line>,
    pub bg: crate::seg::Tok,
    pub cursor_bar: bool,
}

/// The result of one sidebar layout pass: the on-screen row placements, the
/// (clamped) scroll offset actually used, and the metrics section rect (full
/// mode only). Pure — the renderer paints it and the mouse path hit-tests it.
pub(crate) struct SidebarFrame {
    pub rows: Vec<SidebarPlacement>,
    pub scroll: usize,
    pub metrics: Option<Rect>,
}

/// Lay out the sidebar rows for `rect`, starting from `desired_scroll` (clamped
/// so the cursor row stays fully visible). Variable row heights (the cursor's
/// two-tier expansion, the section-heading gap) are resolved here so render and
/// click share one source.
pub(crate) fn build_sidebar(model: &FrameModel, rect: Rect, desired_scroll: usize) -> SidebarFrame {
    use crate::sidebar::RowKind;
    let rail = model.sidebar_rail;
    let visible: Vec<&crate::sidebar::SidebarRow> =
        model.sidebar_rows.iter().filter(|r| r.visible).collect();

    // Quick-jump digits are revealed only while the sidebar is focused — they
    // declutter the resting view but let you see the Ctrl+N (workspace) and
    // Alt+N (worktree) targets when you're navigating it. Each axis counts
    // independently in visible order, slots 1..=9, matching the dispatch:
    // workspaces follow `sidebar_workspace_order` (switchable = has a
    // `worktree_path`); worktrees follow `sidebar_worktree_order` (Tab targets).
    let slots: Vec<Option<u8>> = if model.sidebar_focused {
        let (mut ws, mut wt): (u8, u8) = (1, 1);
        visible
            .iter()
            .map(|r| match r.kind {
                RowKind::Workspace if r.worktree_path.is_some() => {
                    let s = (ws <= 9).then_some(ws);
                    ws += 1;
                    s
                }
                RowKind::Worktree
                    if matches!(r.tab_target, Some(crate::sidebar::RowTarget::Tab(..))) =>
                {
                    let s = (wt <= 9).then_some(wt);
                    wt += 1;
                    s
                }
                _ => None,
            })
            .collect()
    } else {
        vec![None; visible.len()]
    };

    // The full panel reserves a header + blank row at the top and a metrics
    // section at the bottom; the rail uses the whole column.
    let (head_rows, metrics_rows) = if rail {
        (0, 0)
    } else {
        let m = if rect.rows > 10 && !model.metrics.targets.is_empty() {
            6.min(rect.rows.saturating_sub(4))
        } else {
            0
        };
        (2, m)
    };
    let metrics = (metrics_rows > 0).then_some(Rect {
        x: rect.x,
        y: rect.y + rect.rows - metrics_rows,
        cols: rect.cols,
        rows: metrics_rows,
    });
    let list_y = rect.y + head_rows;
    let list_rows = rect.rows.saturating_sub(head_rows + metrics_rows);
    let cursor = if visible.is_empty() {
        0
    } else {
        model.sidebar_selected.min(visible.len() - 1)
    };

    // Compose every visible row's line(s) + background once; the cursor row
    // expands to a detail line when it has secondary metadata.
    let mut composed: Vec<(Vec<crate::seg::Line>, crate::seg::Tok, bool)> =
        Vec::with_capacity(visible.len());
    // The warm-pool chip rides the ACTIVE workspace's row — the workspace_slug of
    // the active worktree row. (Workspace rows themselves carry `active = false`.)
    let active_ws_slug: Option<String> = visible
        .iter()
        .find(|r| r.active && r.kind == RowKind::Worktree)
        .map(|r| r.workspace_slug.clone());
    for (i, row) in visible.iter().enumerate() {
        let is_cursor = i == cursor;
        // A row is the last child at its depth when the next visible row steps
        // back up the tree (or there is none) — drives the └ vs ├ connector.
        let is_last = visible.get(i + 1).is_none_or(|n| n.depth < row.depth);
        let mut lines = if rail {
            vec![compose_rail_line(row)]
        } else {
            let wt = row
                .worktree_path
                .as_deref()
                .and_then(|p| model.sidebar_window_titles.get(p))
                .map(String::as_str);
            let pool = if row.kind == RowKind::Workspace
                && active_ws_slug.as_deref() == Some(row.workspace_slug.as_str())
            {
                model.pool
            } else {
                None
            };
            compose_row_lines(row, wt, is_cursor, is_last, slots[i], pool)
        };
        // A section banner gets a breathing gap above it (except at the top).
        if !rail && row.kind == RowKind::SectionHeading && i > 0 {
            lines.insert(0, crate::seg::Line::Blank);
        }
        let bg = row_bg(row, i, cursor, model);
        // The cursor row always carries the left-edge bar; focus only tints it.
        let cursor_bar = !rail && is_cursor && !matches!(row.kind, RowKind::SectionHeading);
        composed.push((lines, bg, cursor_bar));
    }
    let heights: Vec<usize> = composed.iter().map(|(l, _, _)| l.len().max(1)).collect();
    let scroll = clamp_sidebar_scroll(&heights, cursor, list_rows, desired_scroll);

    let mut rows = Vec::new();
    let mut y = list_y;
    let bottom = list_y + list_rows;
    for (i, (lines, bg, cursor_bar)) in composed.into_iter().enumerate().skip(scroll) {
        if y >= bottom {
            break;
        }
        let height = heights[i].min(bottom - y); // clip a partly-fitting tail row
        rows.push(SidebarPlacement {
            visible_index: i,
            y,
            height,
            lines,
            bg,
            cursor_bar,
        });
        y += heights[i];
    }
    SidebarFrame {
        rows,
        scroll,
        metrics,
    }
}

/// Pick `scroll` (top visible-row index) so the cursor row fits fully within
/// `list_rows`, honoring `desired` where possible. Heights are per-row (the
/// cursor row may be 2). O(n·window) but `n` is the worktree count — tiny.
pub(crate) fn clamp_sidebar_scroll(
    heights: &[usize],
    cursor: usize,
    list_rows: usize,
    desired: usize,
) -> usize {
    let n = heights.len();
    if n == 0 || list_rows == 0 {
        return 0;
    }
    let cursor = cursor.min(n - 1);
    // Never scroll past the cursor (it must be at least the top row).
    let mut scroll = desired.min(cursor);
    loop {
        // Walk from `scroll`; does the cursor row's last line fit in the window?
        let mut used = 0usize;
        let mut fits = false;
        for (i, h) in heights.iter().enumerate().skip(scroll) {
            if i == cursor {
                fits = used + h <= list_rows;
                break;
            }
            used += h;
            if used >= list_rows {
                break;
            }
        }
        if fits || scroll >= cursor {
            break;
        }
        scroll += 1;
    }
    scroll
}

/// Background token for a row: cursor selection > active worktree > multi-select
/// mark > a recessed band for header rows (workspace/host/folder) > the plain
/// panel tint. Section banners never highlight — they read as titles.
fn row_bg(
    row: &crate::sidebar::SidebarRow,
    i: usize,
    cursor: usize,
    model: &FrameModel,
) -> crate::seg::Tok {
    use crate::seg::Tok;
    use crate::sidebar::RowKind;
    // Live drag: the source row lifts; a file-into target highlights.
    if let Some(drag) = &model.sidebar_drag {
        if drag.source == i {
            return Tok::Slot(S::Raise);
        }
        if drag.spot == DragSpotViz::Target(i) {
            return Tok::SelAccent;
        }
    }
    if row.kind == RowKind::SectionHeading {
        return Tok::Slot(S::Panel);
    }
    let header = matches!(
        row.kind,
        RowKind::Workspace | RowKind::TerminalHost | RowKind::Folder
    );
    if i == cursor {
        Tok::Slot(S::Panel2)
    } else if row.active {
        Tok::SelAccent
    } else if model.sidebar_marked.contains(&i) {
        Tok::Slot(S::Raise)
    } else if header {
        Tok::Slot(S::Bg0)
    } else {
        Tok::Slot(S::Panel)
    }
}

/// Resolve a seg color token to a concrete color (for the focus bar's bg).
fn tok_col(t: crate::seg::Tok) -> ColorAttribute {
    with_palette(|p| t.resolve(p))
}

/// One rendered row for the mouse path: on-screen geometry plus the identity
/// and affordances hit-testing needs (kind, stable key, the caret cell).
/// Derived from the same `build_sidebar` pass as the paint, so it can never
/// drift from pixels.
#[derive(Debug, Clone)]
pub(crate) struct RowHit {
    pub visible_index: usize,
    pub y: usize,
    pub height: usize,
    pub kind: crate::sidebar::RowKind,
    pub pin_key: String,
    /// The x of the collapse caret cell, for collapsible rows: clicking it
    /// toggles collapse instead of activating.
    pub caret_x: Option<usize>,
}

/// The rendered rows resolved for mouse hit-testing (see [`RowHit`]).
pub(crate) fn hit_rows(model: &FrameModel, rect: Rect) -> Vec<RowHit> {
    use crate::sidebar::RowKind;
    let frame = build_sidebar(model, rect, model.sidebar_scroll);
    let visible: Vec<&crate::sidebar::SidebarRow> =
        model.sidebar_rows.iter().filter(|r| r.visible).collect();
    // Mirror `build_sidebar`'s quick-jump digit reservation: a focused
    // sidebar's workspace rows show " N " before the caret (3 cols).
    let mut ws_slot: u8 = 1;
    let mut digit_before_caret: Vec<bool> = Vec::with_capacity(visible.len());
    for r in &visible {
        let has = model.sidebar_focused
            && r.kind == RowKind::Workspace
            && r.worktree_path.is_some()
            && ws_slot <= 9;
        if has {
            ws_slot += 1;
        }
        digit_before_caret.push(has);
    }
    frame
        .rows
        .iter()
        .filter_map(|p| {
            let row = visible.get(p.visible_index)?;
            let caret_x = match row.kind {
                RowKind::Workspace | RowKind::TerminalHost => Some(
                    rect.x
                        + 1
                        + if digit_before_caret[p.visible_index] {
                            3
                        } else {
                            0
                        },
                ),
                RowKind::Folder => Some(rect.x + 3),
                _ => None,
            };
            Some(RowHit {
                visible_index: p.visible_index,
                y: p.y,
                height: p.height,
                kind: row.kind,
                pin_key: row.pin_key.clone(),
                caret_x,
            })
        })
        .collect()
}

/// The rendered row under screen row `my`, if any.
pub(crate) fn row_at(hits: &[RowHit], my: usize) -> Option<&RowHit> {
    hits.iter().find(|h| my >= h.y && my < h.y + h.height)
}

/// Live drag feedback carried on the model: the renderer lifts the source row
/// and paints the drop affordance. Loop-transient (mouse press → release);
/// never part of hydration equality.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidebarDragViz {
    /// Visible-row index of the row being dragged (renders raised).
    pub source: usize,
    pub spot: DragSpotViz,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DragSpotViz {
    /// Insert before this visible row: an accent rule paints above it.
    InsertBefore(usize),
    /// Insert at the end of the sibling run that ends after this visible row:
    /// the rule paints below it.
    InsertAfter(usize),
    /// Drop files the source into this row (folder / workspace header):
    /// the target row highlights.
    Target(usize),
    /// No valid drop here; only the source lift renders.
    Invalid,
}

fn draw_metrics_section(surface: &mut Surface, rect: Rect, model: &FrameModel) {
    if rect.rows < 2 || rect.cols == 0 {
        return;
    }

    let line = crate::caps::active_glyphs().box_h.repeat(rect.cols);
    draw_text(
        surface,
        rect.x,
        rect.y,
        &line,
        col(S::Border),
        col(S::Panel),
        rect.cols,
    );
    draw_text_bold(
        surface,
        rect.x + 1,
        rect.y,
        " METRICS ",
        col(S::Text),
        col(S::Panel),
        rect.cols.saturating_sub(1),
    );

    let mut y = rect.y + 1;
    let max_y = rect.y + rect.rows;
    for target in &model.metrics.targets {
        if y >= max_y {
            break;
        }
        let gl = crate::caps::active_glyphs();
        let (dot, dot_fg, health) = match target.health {
            crate::metrics::MetricHealth::Up => (gl.dot_filled, theme_color(theme::GREEN), "up"),
            crate::metrics::MetricHealth::Stale => (gl.dot_hollow, col(S::Dim), "stale"),
            crate::metrics::MetricHealth::Error => (gl.dot_hollow, theme_color(theme::RED), "err"),
        };
        draw_text(surface, rect.x + 1, y, dot, dot_fg, col(S::Panel), 1);
        let label = format!("{} {}", target.name, health);
        draw_text(
            surface,
            rect.x + 3,
            y,
            &label,
            col(S::Text),
            col(S::Panel),
            rect.cols.saturating_sub(3),
        );
        y += 1;

        match target.health {
            crate::metrics::MetricHealth::Up => {
                for sample in target.samples.iter().take(3) {
                    if y >= max_y {
                        break;
                    }
                    let value = crate::metrics::format_sample_value(sample.value);
                    let line = format!("  {} {}", sample.name, value);
                    draw_text(
                        surface,
                        rect.x + 1,
                        y,
                        &line,
                        col(S::Dim),
                        col(S::Panel),
                        rect.cols.saturating_sub(1),
                    );
                    y += 1;
                }
            }
            crate::metrics::MetricHealth::Stale | crate::metrics::MetricHealth::Error => {
                if y < max_y {
                    let err = target.error.as_deref().unwrap_or("scrape failed");
                    let line = format!("  err: {err}");
                    draw_text(
                        surface,
                        rect.x + 1,
                        y,
                        &line,
                        col(S::Faint),
                        col(S::Panel),
                        rect.cols.saturating_sub(1),
                    );
                    y += 1;
                }
            }
        }
    }
}

/// The indent + connector segments for a tree row at `depth` (worktree = 1,
/// folder child = 2). Two cells of indent per ancestor level, then a `└`/`├`
/// connector in a ghost tone so the nesting is visible at a glance.
fn tree_lead(depth: u8, is_last: bool) -> Vec<crate::seg::Seg> {
    use crate::seg::{Tok, seg, sp};
    let gl = crate::caps::active_glyphs();
    let indent = (depth.saturating_sub(1)) as usize * 2;
    let conn = if is_last { gl.tree_corner } else { gl.tree_tee }; // └ / ├
    vec![sp(indent), seg(Tok::Slot(S::Ghost2), format!("{conn} "))]
}

/// The activity dot glyph (item 20): filled while active/waiting, hollow once
/// read-but-stuck, `↻` while building; `None` = nothing. ASCII-safe glyph set.
fn activity_dot_glyph(state: crate::sidebar::ActivityState) -> &'static str {
    use crate::sidebar::ActivityState::*;
    let g = crate::caps::active_glyphs();
    match state {
        Active | Waiting => g.dot_filled, // ● / *
        Read => g.dot_hollow,             // ○ / o
        Loading => g.refresh,             // ↻ / @ — worktree building
        None => "",
    }
}

/// The activity dot color token per state (`activity_active`/`activity_waiting`;
/// loading = accent). Both red states share the waiting slot (glyph-only diff).
fn activity_dot_tok(state: crate::sidebar::ActivityState) -> crate::seg::Tok {
    use crate::sidebar::ActivityState::*;
    crate::seg::Tok::Slot(match state {
        Active => S::ActivityActive,
        Waiting | Read => S::ActivityWaiting,
        Loading => S::Accent,
        None => S::Dim,
    })
}

/// Compose the on-screen line(s) for one visible row. Headers (workspace / host
/// / folder) are a single bold styled line; section banners render like the
/// "WORKSPACES" title; worktrees are a name/status split, and the cursor row
/// (`expanded`) grows a second detail line carrying the secondary metadata
/// (env / backend / PR / unread / disk). `slot` is the Ctrl+1..9 quick-jump
/// digit for switchable workspace rows. Every line starts with a 1-col gutter
/// so the focus bar can overpaint col 0.
fn compose_row_lines(
    row: &crate::sidebar::SidebarRow,
    window_title: Option<&str>,
    expanded: bool,
    is_last: bool,
    slot: Option<u8>,
    // Warm-spare-pool `(ready, target)` for THIS row — `Some` only on the active
    // workspace's row (pool is per-workspace); `None` hides the chip.
    pool: Option<(usize, usize)>,
) -> Vec<crate::seg::Line> {
    use crate::seg::{Line, Seg, Tok, seg, sp};
    use crate::sidebar::{ActivityState, RowKind};
    let gl = crate::caps::active_glyphs();
    let caret = |collapsed: bool| {
        if collapsed {
            gl.caret_closed // ▸
        } else {
            gl.caret_open // ▾
        }
    };

    match row.kind {
        RowKind::Workspace | RowKind::TerminalHost => {
            let mut l = vec![sp(1)];
            // Quick-jump digit on a switchable workspace row (Ctrl+1..9).
            if row.kind == RowKind::Workspace
                && let Some(n) = slot
            {
                // Leading space keeps the digit off the cursor bar (col 0).
                l.push(seg(Tok::Slot(S::Faint), format!(" {n} ")));
            }
            l.push(seg(Tok::Slot(S::Faint), caret(row.collapsed)));
            l.push(sp(1));
            if row.kind == RowKind::TerminalHost {
                // Host group glyph: local vs remote (from the rep connection).
                let local = row
                    .terminal_connection
                    .as_deref()
                    .map(str::is_empty)
                    .unwrap_or(true);
                let host = if local { gl.host_local } else { gl.host_remote };
                l.push(seg(Tok::Slot(S::Dim), format!("{host} ")));
            } else if row.dir {
                // A non-git "dir" workspace gets a home/dir glyph to read apart.
                l.push(seg(Tok::Slot(S::Text), format!("{} ", gl.dir)));
            }
            l.push(seg(Tok::Slot(S::Text), row.label.clone()).bold());
            // Warm-spare-pool chip, right-aligned on the active title (accent
            // when full, dim while provisioning).
            match pool.filter(|(_, t)| *t > 0) {
                Some((ready, target)) => {
                    let tok = if ready >= target {
                        Tok::Slot(S::Accent)
                    } else {
                        Tok::Slot(S::Dim)
                    };
                    vec![Line::Split {
                        l,
                        r: vec![seg(tok, format!("warm {ready}/{target} "))],
                    }]
                }
                None => vec![Line::Segs(l)],
            }
        }
        RowKind::SectionHeading => vec![Line::Segs(vec![
            sp(1),
            seg(Tok::Slot(S::Text), row.label.clone()).bold(),
        ])],
        RowKind::EmptyHint => vec![Line::Segs(vec![
            sp(3),
            seg(Tok::Slot(S::Faint), row.label.clone()),
        ])],
        RowKind::Folder => {
            // Label = bare folder name (rename/delete seed from it); the
            // filed-count decoration is render-only.
            let label = if row.child_count > 0 {
                format!("{} ({})", row.label, row.child_count)
            } else {
                row.label.clone()
            };
            vec![Line::Segs(vec![
                sp(1),
                sp(2),
                seg(Tok::Slot(S::Faint), caret(row.collapsed)),
                sp(1),
                seg(Tok::Slot(S::Dim), format!("{} ", gl.folder)), // ▪
                seg(Tok::Slot(S::Text), label).bold(),
            ])]
        }
        RowKind::Terminal => {
            // Remote (ssh AND mosh — the transport distinction carries no
            // user signal) vs local shell.
            let remote = row
                .terminal_connection
                .as_deref()
                .is_some_and(|c| c.starts_with("ssh") || c.starts_with("mosh"));
            let host = if remote {
                gl.host_remote
            } else {
                gl.host_local
            };
            let mut l = vec![sp(1)];
            l.extend(tree_lead(row.depth, is_last));
            l.push(seg(Tok::Slot(S::Dim), format!("{host} ")));
            l.push(seg(Tok::Slot(S::Dim), row.label.clone()));
            vec![Line::Segs(l)]
        }
        RowKind::Worktree => {
            // Left cluster: gutter, Alt+1..9 jump digit, tree connector,
            // activity dot, the dynamic name, then the agent glyph.
            let mut left = vec![sp(1)];
            left.push(match slot {
                Some(n) => seg(Tok::Slot(S::Faint), format!(" {n} ")),
                None => sp(3), // reserve digit gutter → tree connectors stay aligned (#10+, dormant)
            });
            left.extend(tree_lead(row.depth, is_last));
            if matches!(row.activity, ActivityState::None) {
                left.push(sp(2)); // keep names aligned with dotted rows
            } else {
                left.push(seg(
                    activity_dot_tok(row.activity),
                    activity_dot_glyph(row.activity),
                ));
                left.push(sp(1));
            }
            let name_fg = if row.active {
                Tok::Slot(S::Focus)
            } else if expanded {
                Tok::Slot(S::Text)
            } else {
                Tok::Slot(S::Dim)
            };
            let label = crate::sidebar::compose_row_label(row.pr_number, window_title, &row.label);
            left.push(seg(name_fg, label));

            // Right cluster (always-on): git status + alert badge (PR/unread/disk move to the detail line).
            let mut right: Vec<Seg> = Vec::new();
            let push_sp = |v: &mut Vec<Seg>| {
                if !v.is_empty() {
                    v.push(sp(1));
                }
            };
            if let Some(g) = row.git {
                if g.dirty {
                    right.push(seg(Tok::Hue(theme::Hue::Amber), gl.dot_filled)); // ●
                }
                if g.ahead > 0 {
                    push_sp(&mut right);
                    right.push(seg(
                        Tok::Slot(S::Dim),
                        format!("{}{}", gl.arrow_up, g.ahead),
                    )); // ↑N
                }
                if g.behind > 0 {
                    push_sp(&mut right);
                    right.push(seg(
                        Tok::Slot(S::Dim),
                        format!("{}{}", gl.arrow_down, g.behind),
                    ));
                    // ↓N
                }
            }
            if row.alert_count > 0 {
                push_sp(&mut right);
                right.push(seg(
                    Tok::Hue(theme::Hue::Red),
                    format!("{} {}", gl.warn, row.alert_count),
                ));
                // ⚠N (caps-routed → `!N` in ASCII)
            }
            if row
                .worktree_path
                .as_deref()
                .is_some_and(crate::hibernator::is_hibernated)
            {
                push_sp(&mut right);
                right.push(seg(Tok::Slot(S::Dim), gl.moon.to_string())); // ⏾ hibernated
            }

            let mut lines = vec![if right.is_empty() {
                Line::Segs(left)
            } else {
                Line::Split { l: left, r: right }
            }];
            if expanded && let Some(detail) = crate::sidebar::compose_detail_line(row) {
                lines.push(detail);
            }
            lines
        }
    }
}

/// The slim-rail line for one row, fitted to the rail's ~4 cols. Worktrees and
/// terminals keep their identity (activity dot + first letter); workspaces show
/// a bold initial so repo boundaries stay legible; structural rows (folders,
/// host groups, the section banner) render a faint divider; empty hints vanish.
fn compose_rail_line(row: &crate::sidebar::SidebarRow) -> crate::seg::Line {
    use crate::seg::{Line, Tok, seg, sp};
    use crate::sidebar::{ActivityState, RowKind};
    let gl = crate::caps::active_glyphs();
    let initial = |label: &str| -> String {
        label
            .chars()
            .next()
            .map(|c| c.to_string())
            .unwrap_or_default()
    };
    match row.kind {
        RowKind::Worktree | RowKind::Terminal => {
            let dot = if matches!(row.activity, ActivityState::None) {
                seg(Tok::Slot(S::Ghost2), gl.middot) // · placeholder keeps the column
            } else {
                seg(
                    activity_dot_tok(row.activity),
                    activity_dot_glyph(row.activity),
                )
            };
            let fg = if row.active {
                Tok::Slot(S::Focus)
            } else {
                Tok::Slot(S::Dim)
            };
            Line::Segs(vec![sp(1), dot, sp(1), seg(fg, initial(&row.label))])
        }
        // A workspace keeps its identity at rail width: a bold initial in the
        // letter column (no dot cell — headers have no activity).
        RowKind::Workspace => Line::Segs(vec![
            sp(3),
            seg(Tok::Slot(S::Text), initial(&row.label)).bold(),
        ]),
        // Hints carry no identity worth a rail row; render an empty line.
        RowKind::EmptyHint => Line::Blank,
        // Folders / host groups / the section banner: a faint divider.
        _ => Line::Segs(vec![sp(1), seg(Tok::Slot(S::Faint), gl.box_h)]),
    }
}

fn draw_row_menu(
    surface: &mut Surface,
    rect: Rect,
    frame: &SidebarFrame,
    menu: &RowMenu,
    accent: ColorAttribute,
) {
    let gl = crate::caps::active_glyphs();
    let mrect = menu_rect(rect, frame, menu);
    let width = mrect.cols;
    for (i, entry) in menu.entries.iter().enumerate() {
        let y = mrect.y + i;
        if y >= rect.y + rect.rows {
            break;
        }
        let sel = i == menu.cursor;
        // Panel2/Raise so the menu reads as raised above the Panel-tinted zone.
        let bg = if sel { col(S::Raise) } else { col(S::Panel2) };
        fill(
            surface,
            Rect {
                x: rect.x,
                y,
                cols: width,
                rows: 1,
            },
            bg,
        );
        if entry.is_separator() {
            let rule = gl.box_h.repeat(width.saturating_sub(2));
            draw_text(surface, rect.x + 1, y, &rule, col(S::Border), bg, width);
            continue;
        }
        let fg = if entry.danger {
            theme_color(theme::RED)
        } else if sel {
            accent
        } else {
            col(S::Text)
        };
        draw_text(
            surface,
            rect.x + 1,
            y,
            &format!("{} {}", gl.chevron, entry.label),
            fg,
            bg,
            width.saturating_sub(1),
        );
        // Right-aligned key chip: the menu doubles as key discovery.
        if let Some(key) = entry.key {
            let kw = key.chars().count() + 1;
            if width > kw + 4 {
                draw_text(surface, rect.x + width - kw, y, key, col(S::Faint), bg, kw);
            }
        }
    }
}
