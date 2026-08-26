//! Sidebar layout + paint: the single `build_sidebar` pass that both the
//! renderer and the mouse hit-test derive from, the row/rail line composers,
//! the metrics section, and the row context-menu overlay.
//!
//! Extracted from `chrome.rs` (which orchestrates the whole frame) so the
//! sidebar's view logic has a home that can grow — hit-testing, drag feedback
//! and the menu all live here, next to the geometry they depend on.

use termwiz::color::ColorAttribute;
use termwiz::surface::Surface;

use thegn_core::theme;

use crate::chrome::{
    FrameModel, S, col, draw_text, draw_text_bold, fill, focus_rgb, panel_rgb, theme_color,
    with_palette,
};
use crate::compositor::Rect;

/// Resolved sidebar row-display options, mirrored from `[ui]` config onto the
/// [`FrameModel`] so the pure row composers stay config-free (same pattern as
/// the other model-carried presentation flags). `Default` matches the config
/// defaults (everything on) so unit-built models render the full row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidebarDisplay {
    pub show_status_icon: bool,
    pub show_diff_stat: bool,
    pub show_ahead_behind: bool,
    pub show_pr_chip: bool,
    pub show_jj: bool,
    pub focus_detail: thegn_core::config::FocusDetail,
    pub detail_branch: bool,
    pub detail_branch_stat: bool,
    pub detail_pr: bool,
    pub icon_ahead: String,
    pub icon_behind: String,
    pub icon_status: String,
}

impl Default for SidebarDisplay {
    fn default() -> Self {
        Self {
            show_status_icon: true,
            show_diff_stat: true,
            show_ahead_behind: true,
            show_pr_chip: true,
            show_jj: true,
            focus_detail: thegn_core::config::FocusDetail::All,
            detail_branch: true,
            detail_branch_stat: true,
            detail_pr: true,
            icon_ahead: String::new(),
            icon_behind: String::new(),
            icon_status: String::new(),
        }
    }
}

impl SidebarDisplay {
    /// Snapshot the row-display options from `[ui]` config.
    pub fn from_ui(ui: &thegn_core::config::UiConfig) -> Self {
        Self {
            show_status_icon: ui.sidebar_show_status_icon,
            show_diff_stat: ui.sidebar_show_diff_stat,
            show_ahead_behind: ui.sidebar_show_ahead_behind,
            show_pr_chip: ui.sidebar_show_pr_chip,
            show_jj: ui.sidebar_show_jj,
            focus_detail: ui.sidebar_focus_detail,
            detail_branch: ui.sidebar_detail_branch,
            detail_branch_stat: ui.sidebar_detail_branch_stat,
            detail_pr: ui.sidebar_detail_pr,
            icon_ahead: ui.sidebar_icon_ahead.clone(),
            icon_behind: ui.sidebar_icon_behind.clone(),
            icon_status: ui.sidebar_icon_status.clone(),
        }
    }

    /// The `ahead` marker glyph: the config override, or the caps default (`↑` /
    /// ASCII `^`).
    fn ahead_glyph(&self) -> String {
        if self.icon_ahead.is_empty() {
            crate::caps::active_glyphs().arrow_up.to_string()
        } else {
            self.icon_ahead.clone()
        }
    }

    /// The `behind` marker glyph: override or caps default (`↓` / `v`).
    fn behind_glyph(&self) -> String {
        if self.icon_behind.is_empty() {
            crate::caps::active_glyphs().arrow_down.to_string()
        } else {
            self.icon_behind.clone()
        }
    }

    /// The dirty-status marker glyph: override or caps default (`●` / `*`).
    fn status_glyph(&self) -> String {
        if self.icon_status.is_empty() {
            crate::caps::active_glyphs().dot_filled.to_string()
        } else {
            self.icon_status.clone()
        }
    }

    /// Whether a row's detail line shows, given focus and whether it's the
    /// cursor row. The detail line only appears while the sidebar owns focus.
    fn show_detail(&self, focused: bool, is_cursor: bool) -> bool {
        use thegn_core::config::FocusDetail;
        match self.focus_detail {
            FocusDetail::Off => false,
            _ if !focused => false,
            FocusDetail::All => true,
            FocusDetail::Cursor => is_cursor,
        }
    }
}

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
        // without clobbering content. `content_cols` stops short of the
        // scrollbar gutter when one is reserved.
        crate::seg::draw_lines(
            surface,
            Rect {
                x: rect.x,
                y: p.y,
                cols: frame.content_cols,
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

    if let Some(hrect) = frame.hints {
        draw_sidebar_hints(surface, hrect, &model.sidebar_hints);
    }

    draw_sidebar_overflow(surface, &frame, model);

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

/// The slim collapsed rail: one row per visible row of EVERY kind
/// (workspaces, folders, hosts, terminals, banners), an activity dot in
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
                cols: frame.content_cols,
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

/// The scroll affordance gutter: a 1-column proportional bar on the sidebar's
/// right edge, present ONLY when the list overflows its window (and never in
/// rail mode — 4 columns cannot spare one). Its column is excluded from
/// [`SidebarFrame::content_cols`], so a right-aligned badge cluster can never
/// collide with it.
///
/// Hit-testing is unaffected by construction: `hit_rows` maps `frame.rows` only
/// and resolves `caret_x` from the LEFT edge, and `row_at` is y-only — so
/// reserving the right edge cannot move a click. A click on the gutter resolves
/// to the row beside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Scrollbar {
    pub x: usize,
    pub y: usize,
    pub rows: usize,
    pub thumb_y: usize,
    pub thumb_rows: usize,
}

/// A "there is more here" count chip. It is an OVERLAY, never a row: it takes
/// no layout budget (reserving one would shrink `list_rows`, which is the very
/// thing that hid the tail, and would feed back into the scroll clamp) and it
/// is not in [`SidebarFrame::rows`], so `hit_rows`/`row_at`/`menu_rect` never
/// see it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OverflowChip {
    /// The 1-row rect to paint into, right-aligned inside the sidebar column.
    pub rect: Rect,
    /// How many visible rows are hidden in that direction.
    pub hidden: usize,
}

/// The result of one sidebar layout pass: the on-screen row placements, the
/// (clamped) scroll offset actually used, and the metrics section rect (full
/// mode only). Pure — the renderer paints it and the mouse path hit-tests it.
pub(crate) struct SidebarFrame {
    pub rows: Vec<SidebarPlacement>,
    #[allow(dead_code)] // the offset this pass used; asserted in tests
    pub scroll: usize,
    pub metrics: Option<Rect>,
    /// The navigation-hints footer rect — revealed only while the sidebar is
    /// focused (and the list has room), sitting above the metrics section.
    pub hints: Option<Rect>,
    /// What this layout pass could and could not show; the affordances below
    /// are derived from it.
    #[allow(dead_code)] // used in tests
    pub window: SidebarWindow,
    /// Columns available to row content — `rect.cols`, minus the scrollbar
    /// gutter when one is reserved.
    pub content_cols: usize,
    pub scrollbar: Option<Scrollbar>,
    /// Rows scrolled off the top / clipped off the bottom, as paintable chips.
    pub overflow_above: Option<OverflowChip>,
    pub overflow_below: Option<OverflowChip>,
}

/// The per-visible-row heights [`build_sidebar`] will lay out for `model` in
/// `rect`, plus the window geometry around them. The SINGLE definition of the
/// sidebar's vertical budget, shared by the layout pass and the event loop's
/// scroll math. Composes nothing — no `Line`, no allocation per row beyond the
/// heights vector itself — so the loop can call it on the input path without
/// turning a `Panes` frame into a `Full` one.
pub(crate) struct SidebarGeom {
    pub heights: Vec<usize>,
    pub list_y: usize,
    pub list_rows: usize,
    pub cursor: usize,
    /// The two `show_detail` inputs, frozen for the life of a drag gesture.
    /// Carried as fields (not re-derived) so the compose pass and any loop-side
    /// caller agree — see [`SidebarLayoutLock`].
    pub detail_focused: bool,
    pub detail_cursor: usize,
    pub metrics: Option<Rect>,
}

/// The vertical budget + row heights for `model` in `rect`, without composing
/// anything. See [`SidebarGeom`]. The event loop uses this to resolve a scroll
/// offset on the input path; [`build_sidebar`] uses it as its first step, so
/// there is exactly one definition of a row's height.
pub(crate) fn sidebar_geom(model: &FrameModel, rect: Rect) -> SidebarGeom {
    let visible: Vec<&crate::sidebar::SidebarRow> =
        model.sidebar_rows.iter().filter(|r| r.visible).collect();
    sidebar_geom_from(model, rect, &visible)
}

/// [`sidebar_geom`] with the visible-row slice already filtered — so
/// `build_sidebar` doesn't collect it twice.
fn sidebar_geom_from(
    model: &FrameModel,
    rect: Rect,
    visible: &[&crate::sidebar::SidebarRow],
) -> SidebarGeom {
    use crate::sidebar::RowKind;
    let rail = model.sidebar_rail;
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

    // The two inputs to `show_detail`, frozen for the life of a drag gesture.
    // Derived ONCE and carried as fields: the height pass and the compose pass
    // must agree or the `debug_assert_eq!` between them fires in debug builds.
    let (detail_focused, detail_cursor) = match &model.sidebar_drag_lock {
        Some(l) => (
            l.detail_focused,
            l.detail_cursor.min(visible.len().saturating_sub(1)),
        ),
        None => (model.sidebar_focused, cursor),
    };

    // Heights WITHOUT composing: every visible row used to be fully composed
    // (~10 allocations each) before the scroll clamp threw the off-screen ones
    // away — O(all worktrees) waste on every Full frame. The only
    // variable-height cases are a Worktree row's detail tier (probed via
    // `compose_detail_line`, and only when `show_detail` can be true — the
    // sidebar must be focused) and the SectionHeading's breathing gap; every
    // other row is exactly one line. `compose_row_lines` is the source of truth
    // for that rule — a debug assertion in `build_sidebar` keeps them lockstep.
    let heights: Vec<usize> = visible
        .iter()
        .enumerate()
        .map(|(i, row)| {
            if rail {
                return 1;
            }
            let mut h = 1;
            if row.kind == RowKind::Worktree
                && model
                    .sidebar_display
                    .show_detail(detail_focused, i == detail_cursor)
                && crate::sidebar::compose_detail_line(row, &model.sidebar_display).is_some()
            {
                h = 2;
            }
            // A section banner gets a breathing gap above it (except at the top).
            if row.kind == RowKind::SectionHeading && i > 0 {
                h += 1;
            }
            h
        })
        .collect();

    SidebarGeom {
        heights,
        list_y,
        list_rows,
        cursor,
        detail_focused,
        detail_cursor,
        metrics,
    }
}

/// Lay out the sidebar rows for `rect` with the window top at `desired_scroll`
/// (clamped to the end of the list, never moved to chase the cursor — see
/// [`scroll_to_reveal`]). Variable row heights (the focused detail tier, the
/// section-heading gap) are resolved via [`sidebar_geom`] so render and click
/// share one source.
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

    let geom = sidebar_geom_from(model, rect, &visible);
    let SidebarGeom {
        ref heights,
        list_y,
        list_rows,
        cursor,
        detail_focused,
        detail_cursor,
        metrics,
    } = geom;

    // Purely geometric: the window may sit anywhere from the top to the end of
    // the list. Revealing the cursor is the STATE layer's job
    // (`handlers::sidebar_scroll`) — putting it here would make paint depend on
    // a policy the hit-test could disagree with. When the list fits,
    // `max_sidebar_scroll` is 0, so this pins to 0 and the frame is identical
    // to a pre-scroll-model one.
    let scroll = desired_scroll.min(max_sidebar_scroll(heights, list_rows));

    // The warm-pool chip rides the ACTIVE workspace's row — the workspace_slug of
    // the active worktree row. (Workspace rows themselves carry `active = false`.)
    let active_ws_slug: Option<String> = visible
        .iter()
        .find(|r| r.active && r.kind == RowKind::Worktree)
        .map(|r| r.workspace_slug.clone());

    // Compose ONLY the rows that land inside the list window.
    let mut rows = Vec::new();
    let mut y = list_y;
    let bottom = list_y + list_rows;
    for (i, row) in visible.iter().enumerate().skip(scroll) {
        if y >= bottom {
            break;
        }
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
            let show_detail = model
                .sidebar_display
                .show_detail(detail_focused, i == detail_cursor);
            compose_row_lines(
                row,
                wt,
                is_cursor,
                show_detail,
                is_last,
                slots[i],
                pool,
                &model.sidebar_display,
            )
        };
        // A section banner gets a breathing gap above it (except at the top).
        if !rail && row.kind == RowKind::SectionHeading && i > 0 {
            lines.insert(0, crate::seg::Line::Blank);
        }
        debug_assert_eq!(
            lines.len().max(1),
            heights[i],
            "height pass diverged from compose_row_lines for row {i} ({:?})",
            row.kind
        );
        let bg = row_bg(row, i, cursor, model);
        // The cursor row always carries the left-edge bar; focus only tints it.
        let cursor_bar = !rail && is_cursor && !matches!(row.kind, RowKind::SectionHeading);
        let height = heights[i].min(bottom - y); // clip a partly-fitting tail row
        // `draw_lines` keeps a clipped row's FIRST `height` lines, which is
        // right for a worktree (name before detail) and wrong for a section
        // heading, whose line 0 is the breathing gap — the banner is what got
        // dropped, so the row painted as an invisible blank that still ate a
        // screen row. Trim the gap instead. Strictly AFTER the lockstep
        // assertion above, which must keep seeing the untrimmed vector.
        if height < lines.len() && !rail && row.kind == RowKind::SectionHeading && i > 0 {
            lines.remove(0);
        }
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
    // Navigation-hints footer: revealed only while the sidebar is focused (the
    // same "reveal on focus" language as the Alt/Ctrl+N digit hints), and only
    // when the laid-out list leaves genuine blank space below it. Anchoring to
    // the tail means it fills the empty column bottom without ever pushing a row
    // or forcing a scroll — a full list simply shows no hints. It sits just
    // above the metrics section (the list `bottom`).
    let hints = if rail || !model.sidebar_focused {
        None
    } else {
        // Adaptive: show as many tips as the blank tail affords, rather than
        // all-or-nothing. The list is ordered most-discoverable-first, so
        // clipping the tail degrades gracefully — and growing the key table
        // can never make the whole footer vanish.
        let blank = bottom.saturating_sub(y);
        // 1 row for the rule/title + 1 row of gap above it.
        let max_tips = blank.saturating_sub(2);
        (max_tips >= MIN_SIDEBAR_HINT_ROWS).then(|| {
            let tips = max_tips.min(model.sidebar_hints.len());
            let hint_h = tips + 1;
            Rect {
                x: rect.x,
                y: bottom - hint_h,
                cols: rect.cols,
                rows: hint_h,
            }
        })
    };
    // What this pass could and could not show. Measured from the same heights
    // the loop walked, so the two can never disagree.
    let window = sidebar_window(heights, list_rows, scroll);

    // The overflow affordances. Both are pure overlays derived AFTER layout:
    // reserving budget for them would shrink `list_rows`, which is exactly the
    // shortage they exist to report. Suppressed in the rail (4 columns) and
    // whenever the column is too narrow to say anything useful.
    //
    // The gutter is reserved ONLY when the list actually overflows, so a list
    // that fits renders byte-identically to a pre-scroll-model frame.
    let room = !rail && list_rows > 0 && rect.cols > SIDEBAR_OVERFLOW_COLS;
    let scrollbar = (room && window.total_cells > list_rows).then(|| {
        let thumb_rows = ((list_rows * list_rows) / window.total_cells)
            .max(1)
            .min(list_rows);
        // Distribute the thumb across the track in proportion to how much
        // content is above the window. `span` is 0-safe: the `then` guard
        // already established `total_cells > list_rows`.
        let span = window.total_cells - list_rows;
        let thumb_y = list_y + (window.above_cells * (list_rows - thumb_rows)) / span;
        Scrollbar {
            x: rect.x + rect.cols - 1,
            y: list_y,
            rows: list_rows,
            thumb_y: thumb_y.min(bottom - thumb_rows),
            thumb_rows,
        }
    });
    // Chips live INSIDE the content columns, so they can never overpaint the
    // gutter (which is drawn first).
    let content_cols = rect.cols - usize::from(scrollbar.is_some());
    let chip = |at_y: usize, hidden: usize| {
        (room && hidden > 0 && content_cols >= SIDEBAR_OVERFLOW_COLS).then(|| OverflowChip {
            rect: Rect {
                x: rect.x + content_cols - SIDEBAR_OVERFLOW_COLS,
                y: at_y,
                cols: SIDEBAR_OVERFLOW_COLS,
                rows: 1,
            },
            hidden,
        })
    };
    let overflow_above = chip(list_y, window.hidden_above);
    let overflow_below = chip(bottom.saturating_sub(1), window.hidden_below);

    SidebarFrame {
        rows,
        scroll,
        metrics,
        hints,
        window,
        content_cols,
        scrollbar,
        overflow_above,
        overflow_below,
    }
}

/// Paint the scroll affordances: the right-edge proportional gutter, and the
/// `⌄N` / `^N` count chips marking rows the window could not show.
///
/// Both are overlays — they take no layout budget and are absent from
/// `frame.rows`, so hit-testing never sees them (a click on either resolves to
/// the row beside it). A chip rides the underlying row's background so it reads
/// as an overlay rather than a notch punched in the cursor or active row.
fn draw_sidebar_overflow(surface: &mut Surface, frame: &SidebarFrame, model: &FrameModel) {
    if let Some(bar) = frame.scrollbar {
        let glyph = crate::caps::active_glyphs().half_block_r;
        let thumb_fg = if model.sidebar_focused {
            col(S::Focus)
        } else {
            col(S::Dim)
        };
        for dy in 0..bar.rows {
            let y = bar.y + dy;
            let on_thumb = y >= bar.thumb_y && y < bar.thumb_y + bar.thumb_rows;
            let fg = if on_thumb { thumb_fg } else { col(S::Ghost2) };
            draw_text(surface, bar.x, y, glyph, fg, col(S::Panel), 1);
        }
    }

    let g = crate::caps::active_glyphs();
    for (chip, glyph) in [
        (frame.overflow_above, g.arrow_up),
        (frame.overflow_below, g.arrow_down),
    ] {
        let Some(c) = chip else { continue };
        // Ride the row under the chip so it doesn't punch a hole in a
        // highlighted row; fall back to the panel tint on a blank tail.
        let bg = frame
            .rows
            .iter()
            .find(|p| c.rect.y >= p.y && c.rect.y < p.y + p.height)
            .map(|p| tok_col(p.bg))
            .unwrap_or_else(|| col(S::Panel));
        // Right-aligned in the 4-col chip: a leading gap keeps it off the row's
        // own text, and `{:>2}` keeps 1- and 2-digit counts in one column.
        let label = format!(" {glyph}{:>2}", c.hidden.min(99));
        draw_text(
            surface,
            c.rect.x,
            c.rect.y,
            &label,
            col(S::Faint),
            bg,
            c.rect.cols,
        );
    }
}

/// Don't bother with a NAVIGATE footer that can't show at least this many tips
/// — a one- or two-row stub reads as clutter rather than help.
const MIN_SIDEBAR_HINT_ROWS: usize = 3;

/// Width of an overflow chip: a direction glyph plus up to two digits (` ⌄12`).
const SIDEBAR_OVERFLOW_COLS: usize = 4;

/// Paint the navigation-hints footer: a rule + " NAVIGATE " title (matching the
/// metrics section) over a column of dim chord / label pairs.
///
/// `tips` comes from `model.sidebar_hints`
/// ([`crate::sidebar_keytable::footer_hints`]) — registry-derived jump chords
/// plus the sidebar key table — so nothing here is a hard-coded key.
fn draw_sidebar_hints(surface: &mut Surface, rect: Rect, tips: &[(String, String)]) {
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
        " NAVIGATE ",
        col(S::Dim),
        col(S::Panel),
        rect.cols.saturating_sub(1),
    );

    // Only the tips that actually fit contribute to the label column width, so
    // a clipped-off wide chord can't leave the visible rows over-indented.
    let tips = &tips[..tips.len().min(rect.rows.saturating_sub(1))];
    let chord_w = tips
        .iter()
        .map(|(c, _)| c.chars().count())
        .max()
        .unwrap_or(0);
    let max_y = rect.y + rect.rows;
    let right = rect.x + rect.cols;
    for (i, (chord, label)) in tips.iter().enumerate() {
        let y = rect.y + 1 + i;
        if y >= max_y {
            break;
        }
        draw_text(
            surface,
            rect.x + 1,
            y,
            chord,
            col(S::Faint),
            col(S::Panel),
            rect.cols.saturating_sub(1),
        );
        let lx = rect.x + 1 + chord_w + 1;
        if lx < right {
            draw_text(
                surface,
                lx,
                y,
                label,
                col(S::Ghost),
                col(S::Panel),
                right - lx,
            );
        }
    }
}

/// The largest *useful* top-row index: the smallest `s` whose remaining tail
/// `heights[s..]` fits entirely inside `list_rows` — i.e. "scrolled to the
/// end". `0` when the whole list already fits. Scrolling past this only
/// reveals blank space; leaving it UNBOUNDED is what used to strand the last
/// rows off the bottom with no way to reach them.
///
/// Never returns `>= heights.len()`, so a single row taller than the whole
/// window still yields a valid top index.
pub(crate) fn max_sidebar_scroll(heights: &[usize], list_rows: usize) -> usize {
    let n = heights.len();
    if n == 0 {
        return 0;
    }
    let mut used = 0usize;
    for (i, h) in heights.iter().enumerate().rev() {
        used += h;
        if used > list_rows {
            // Row `i` no longer fits, so `i + 1` is the last offset whose tail
            // does. Cap at `n - 1`: a lone over-tall row is still a valid top.
            return (i + 1).min(n - 1);
        }
    }
    0
}

/// Move `desired` the MINIMUM distance that brings `cursor` fully into view —
/// up if the cursor sits above the window, down if its last line falls past the
/// bottom — never past [`max_sidebar_scroll`].
///
/// This is the *reveal policy*, and it deliberately does NOT live inside
/// [`build_sidebar`]: paint and hit-test share that function, so it must stay a
/// pure geometric mapping of `desired_scroll`. Cursor-follow belongs to the
/// state layer (`handlers::sidebar_scroll`), which calls this and writes the
/// result back onto `SidebarState::scroll`.
///
/// O(n·window), and `n` is the worktree count — tiny.
pub(crate) fn scroll_to_reveal(
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
    // Start from the caller's window, bounded by the end of the list, then walk
    // it the shortest distance that reveals the cursor. Clamping to `cursor`
    // first handles "the cursor is above the window" in one step.
    let mut scroll = desired
        .min(max_sidebar_scroll(heights, list_rows))
        .min(cursor);
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

/// Where the list window sits for a given `scroll`, and how much content falls
/// outside it. The scroll affordances and the event loop's scroll math read
/// this; it composes nothing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct SidebarWindow {
    /// The offset this window was measured at (already clamped).
    pub scroll: usize,
    /// First visible-row index laid out (== `scroll` when non-empty).
    pub first_visible: usize,
    /// Last visible-row index with at least one line laid out. Equal to
    /// `first_visible` for an empty/degenerate window.
    pub last_visible: usize,
    /// Rows scrolled off the top.
    pub hidden_above: usize,
    /// Rows that got no line at all. A partially-clipped tail row counts as
    /// SHOWN, not hidden — it is on screen, so counting it would make the
    /// "N more" chip lie.
    pub hidden_below: usize,
    /// Cells of content above the window — drives the scrollbar thumb.
    pub above_cells: usize,
    /// Total cells the whole list would occupy.
    pub total_cells: usize,
    pub list_rows: usize,
}

/// Measure the window `heights`/`list_rows`/`scroll` produce. Mirrors the
/// compose loop in [`build_sidebar`] exactly (`break` once `y >= bottom`), so
/// the two can never disagree about what is on screen.
pub(crate) fn sidebar_window(heights: &[usize], list_rows: usize, scroll: usize) -> SidebarWindow {
    let n = heights.len();
    let total_cells: usize = heights.iter().sum();
    let scroll = scroll.min(n.saturating_sub(1));
    let above_cells: usize = heights.iter().take(scroll).sum();
    let mut w = SidebarWindow {
        scroll,
        first_visible: scroll,
        last_visible: scroll,
        hidden_above: scroll,
        hidden_below: 0,
        above_cells,
        total_cells,
        list_rows,
    };
    if n == 0 || list_rows == 0 {
        w.hidden_below = n.saturating_sub(scroll);
        return w;
    }
    let mut y = 0usize;
    let mut laid_end = scroll;
    for (i, h) in heights.iter().enumerate().skip(scroll) {
        if y >= list_rows {
            break;
        }
        laid_end = i + 1;
        y += h;
    }
    w.last_visible = laid_end.saturating_sub(1).max(scroll);
    w.hidden_below = n - laid_end;
    w
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
        if drag.source == Some(i) {
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
    frame
        .rows
        .iter()
        .filter_map(|p| {
            let row = visible.get(p.visible_index)?;
            // The caret geometry below is the FULL layout's; the rail paints
            // no caret at all (`compose_rail_line`), so advertising one there
            // made unmarked cells toggle collapse (and the focused offset even
            // landed outside the 4-column rail).
            let caret_x = match row.kind {
                _ if model.sidebar_rail => None,
                // Header rows always reserve the 3-col quick-jump gutter
                // (`compose_row_lines` mirrors this), so the caret sits at a
                // stable column regardless of focus.
                RowKind::Workspace | RowKind::TerminalHost => Some(rect.x + 4),
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
    /// Visible-row index of the row being dragged (renders raised), or `None`
    /// when it has scrolled out of the window. `Option` rather than a sentinel:
    /// this used to carry `usize::MAX` for "off-screen", which is a real index
    /// as far as every comparison is concerned.
    pub source: Option<usize>,
    pub spot: DragSpotViz,
}

/// Row heights frozen for the duration of a sidebar drag gesture.
///
/// A worktree row is 1 or 2 lines depending on the detail tier
/// ([`SidebarDisplay::show_detail`]), and that tier depends on sidebar focus and
/// the cursor row — both of which move during a press-drag: the press hands
/// focus to the sidebar (under the default `FocusDetail::All` every
/// branch-bearing row grows), activating a row can hand focus straight back to
/// the center pane (every row shrinks), and edge autoscroll keeps moving the
/// cursor. Rows reflowing under a held pointer silently change the drop target
/// with no pointer motion at all.
///
/// The lock carries the values the **pressed frame was painted with**, so
/// nothing reflows mid-gesture — and a press that turns out to be a plain click
/// paints identically to the frame before it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidebarLayoutLock {
    pub detail_focused: bool,
    pub detail_cursor: usize,
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
        Failed => g.cross,                // ✗ / x — env bring-up failed
        None => "",
    }
}

/// The activity dot color token per state (`activity_active` /
/// `activity_waiting` / `activity_done`; loading = accent).
///
/// `blocked` splits the two "awaiting you" states apart. The FSM only knows the
/// worktree went quiet after working; *why* you are needed comes from the
/// attention tier, which is fed by real evidence — an `agent_attention`
/// notification, a queue needing a human, a gate error. So a worktree whose most
/// urgent signal is [`thegn_core::attention::AttentionTier::Blocked`] keeps the
/// loud red dot ("it asked you something"), and a merely-finished one gets
/// calmer amber ("your turn").
/// Both used to be the same red, which made a finished turn shout as loudly as a
/// stuck one. Seen-vs-unread stays the filled/hollow glyph distinction.
fn activity_dot_tok(state: crate::sidebar::ActivityState, blocked: bool) -> crate::seg::Tok {
    use crate::sidebar::ActivityState::*;
    // Failed reads as an error, so it takes a red hue rather than an activity
    // slot; every other state maps to its activity/accent slot.
    match state {
        Failed => crate::seg::Tok::Hue(theme::Hue::Red),
        _ => crate::seg::Tok::Slot(match state {
            Active => S::ActivityActive,
            Waiting | Read if blocked => S::ActivityWaiting,
            Waiting | Read => S::ActivityDone,
            Loading => S::Accent,
            _ => S::Dim,
        }),
    }
}

/// Whether this row's most urgent signal is "something is blocked on you"
/// (an agent asked a question, a queue needs a human) rather than merely
/// "finished". Drives the red-vs-amber dot in [`activity_dot_tok`].
fn row_is_blocked(row: &crate::sidebar::SidebarRow) -> bool {
    row.attention
        .is_some_and(|a| a.tier == thegn_core::attention::AttentionTier::Blocked)
}

/// Compose the on-screen line(s) for one visible row. Headers (workspace / host
/// / folder) are a single bold styled line; section banners render like the
/// "WORKSPACES" title; worktrees are a name/status split. `is_cursor` renders the
/// name in full (vs dim) for the highlighted row; `show_detail` grows a second
/// detail line carrying the branch + secondary metadata (env / backend / PR /
/// unread / disk), gated by the focused-detail policy. `slot` is the Ctrl+1..9
/// quick-jump digit for switchable workspace rows. Every line starts with a
/// 1-col gutter so the focus bar can overpaint col 0.
#[allow(clippy::too_many_arguments)]
fn compose_row_lines(
    row: &crate::sidebar::SidebarRow,
    window_title: Option<&str>,
    is_cursor: bool,
    show_detail: bool,
    is_last: bool,
    slot: Option<u8>,
    // Warm-spare-pool `(ready, target)` for THIS row — `Some` only on the active
    // workspace's row (pool is per-workspace); `None` hides the chip.
    pool: Option<(usize, usize)>,
    disp: &SidebarDisplay,
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
            // ALWAYS reserve the 3-col gutter (mirroring the worktree rows)
            // so headers and their carets sit at a stable column instead of
            // shifting 3 cells right the moment the sidebar takes focus.
            match slot {
                // Leading space keeps the digit off the cursor bar (col 0).
                Some(n) if row.kind == RowKind::Workspace => {
                    l.push(seg(Tok::Slot(S::Faint), format!(" {n} ")));
                }
                _ => l.push(sp(3)),
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
                    activity_dot_tok(row.activity, row_is_blocked(row)),
                    activity_dot_glyph(row.activity),
                ));
                left.push(sp(1));
            }
            let name_fg = if row.active {
                Tok::Slot(S::Focus)
            } else if is_cursor {
                Tok::Slot(S::Text)
            } else {
                Tok::Slot(S::Dim)
            };
            // Flat cross-workspace layout tags each row with its repo, dim, so
            // a cross-repo list still shows which workspace a worktree is in.
            if let Some(prefix) = &row.repo_prefix {
                left.push(seg(Tok::Slot(S::Faint), format!("{prefix}/")));
            }
            // Main line is the dynamic name (OSC window title) or the branch; PR
            // moves to a compact right-cluster chip + the focused detail line.
            let label = crate::sidebar::compose_row_label(
                window_title,
                crate::sidebar::row_display_branch(row),
            );
            left.push(seg(name_fg, label));

            // Right cluster (always-on): git status icon + uncommitted diff stat +
            // ahead/behind + PR chip + alert badge (the rest moves to the detail).
            let mut right: Vec<Seg> = Vec::new();
            let push_sp = |v: &mut Vec<Seg>| {
                if !v.is_empty() {
                    v.push(sp(1));
                }
            };
            if let Some(g) = row.git {
                if g.dirty && disp.show_status_icon {
                    right.push(seg(Tok::Hue(theme::Hue::Amber), disp.status_glyph())); // ●
                }
                if disp.show_diff_stat {
                    if g.add > 0 {
                        push_sp(&mut right);
                        right.push(seg(Tok::Hue(theme::Hue::Green), format!("+{}", g.add)));
                    }
                    if g.del > 0 {
                        push_sp(&mut right);
                        right.push(seg(Tok::Hue(theme::Hue::Red), format!("-{}", g.del)));
                    }
                }
                if disp.show_ahead_behind {
                    if g.ahead > 0 {
                        push_sp(&mut right);
                        right.push(seg(
                            Tok::Slot(S::Dim),
                            format!("{}{}", disp.ahead_glyph(), g.ahead),
                        )); // ↑N
                    }
                    if g.behind > 0 {
                        push_sp(&mut right);
                        right.push(seg(
                            Tok::Slot(S::Dim),
                            format!("{}{}", disp.behind_glyph(), g.behind),
                        )); // ↓N
                    }
                }
                // jujutsu-colocated marker (ĵ; ASCII `j`), caps-routed.
                if g.jj && disp.show_jj {
                    push_sp(&mut right);
                    right.push(seg(Tok::Slot(S::Dim), gl.jj));
                }
            }
            // Compact open-PR chip (⬡N) — the full `PR #N` moves to the detail line.
            if disp.show_pr_chip
                && let Some(n) = row.pr_number
            {
                push_sp(&mut right);
                right.push(seg(Tok::Hue(theme::Hue::Green), format!("{}{}", gl.hex, n)));
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
                // Guarantee the row keeps its identity: the left cluster
                // (gutter + connectors + NAME) holds at least its lead plus a
                // readable name slice before badges take space — plain
                // `Split` lets a badge-heavy right cluster (`● +1234 -567 ↑9
                // ↓9 ⬡12 ⚠3`) erase the name entirely at the default width.
                Line::SplitMinLeft {
                    l: left,
                    r: right,
                    min_l: 18,
                }
            }];
            if show_detail && let Some(detail) = crate::sidebar::compose_detail_line(row, disp) {
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
                    activity_dot_tok(row.activity, row_is_blocked(row)),
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
    // The other `layer::open_layer` bypass (hand-rolled rows, no card). It draws
    // inside the sidebar, where the pane caret never sits, but register the
    // cover anyway so this isn't a hole waiting for a layout change.
    crate::caret::cover(mrect);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sidebar::RowKind;

    fn entry(id: &str) -> RowMenuEntry {
        RowMenuEntry {
            label: id.to_string(),
            id: id.to_string(),
            key: None,
            danger: false,
        }
    }

    /// The two "awaiting you" states split by colour, not by glyph: an agent
    /// blocked on a question keeps the loud red slot, while a merely-finished
    /// turn gets the calmer amber one. Seen-vs-unread stays the filled/hollow
    /// glyph distinction, so both dimensions read independently.
    #[test]
    fn awaiting_dot_is_red_when_blocked_and_amber_when_merely_finished() {
        use crate::seg::Tok;
        use crate::sidebar::ActivityState;

        let row = |tier: Option<thegn_core::attention::AttentionTier>| {
            let mut r = crate::sidebar::SidebarRow::base(RowKind::Worktree, 1, "app/feat", "app");
            r.attention = tier.map(|tier| thegn_core::attention::AttentionScore {
                tier,
                sub: 0,
                reason: thegn_core::attention::AttentionReason::Idle,
                since: None,
                episode: 0,
            });
            r
        };
        let blocked = row(Some(thegn_core::attention::AttentionTier::Blocked));
        let finished = row(Some(thegn_core::attention::AttentionTier::Waiting));
        let no_score = row(None);

        for state in [ActivityState::Waiting, ActivityState::Read] {
            assert_eq!(
                activity_dot_tok(state, row_is_blocked(&blocked)),
                Tok::Slot(S::ActivityWaiting),
                "{state:?} with a blocked tier must stay red"
            );
            // Anything short of `Blocked` — including no score at all — is
            // "finished, your turn" rather than "stuck on a question".
            for r in [&finished, &no_score] {
                assert_eq!(
                    activity_dot_tok(state, row_is_blocked(r)),
                    Tok::Slot(S::ActivityDone),
                    "{state:?} without a blocked tier must be amber"
                );
            }
        }

        // Unread-vs-seen remains the glyph axis, independent of the colour.
        let g = crate::caps::active_glyphs();
        assert_eq!(activity_dot_glyph(ActivityState::Waiting), g.dot_filled);
        assert_eq!(activity_dot_glyph(ActivityState::Read), g.dot_hollow);

        // Working is unaffected by blocked-ness, and a dotless row stays dotless.
        assert_eq!(
            activity_dot_tok(ActivityState::Active, true),
            Tok::Slot(S::ActivityActive)
        );
        assert_eq!(activity_dot_glyph(ActivityState::None), "");
    }

    #[test]
    fn menu_step_skips_separators_both_directions() {
        let entries = vec![
            entry("a"),
            RowMenuEntry::separator(),
            entry("b"),
            entry("c"),
        ];
        // Forward from 0 skips the separator at 1 and lands on 2.
        assert_eq!(menu_step(&entries, 0, 1), 2);
        // Backward from 2 skips the separator and lands on 0.
        assert_eq!(menu_step(&entries, 2, -1), 0);
        // Forward within a contiguous run.
        assert_eq!(menu_step(&entries, 2, 1), 3);
    }

    #[test]
    fn menu_step_clamps_at_edges() {
        let entries = vec![entry("a"), entry("b")];
        // No selectable entry past the end ⇒ unchanged.
        assert_eq!(menu_step(&entries, 1, 1), 1);
        // Nothing before the start ⇒ unchanged.
        assert_eq!(menu_step(&entries, 0, -1), 0);
        // A trailing separator with nothing beyond it ⇒ stay put.
        let trailing = vec![entry("a"), RowMenuEntry::separator()];
        assert_eq!(menu_step(&trailing, 0, 1), 0);
    }

    #[test]
    fn scroll_to_reveal_keeps_cursor_visible() {
        // Ten single-height rows, a 4-row window.
        let heights = vec![1usize; 10];
        // Cursor near the top with desired 0 ⇒ no scroll.
        assert_eq!(scroll_to_reveal(&heights, 1, 4, 0), 0);
        // Cursor below the window ⇒ scroll so the cursor fits.
        let scroll = scroll_to_reveal(&heights, 7, 4, 0);
        assert!(
            (4..=7).contains(&scroll),
            "cursor 7 fits in a 4-row window: {scroll}"
        );
        // Scroll never advances past the cursor.
        assert!(scroll_to_reveal(&heights, 3, 4, 9) <= 3);
    }

    #[test]
    fn scroll_to_reveal_degenerate_inputs() {
        assert_eq!(scroll_to_reveal(&[], 0, 4, 0), 0);
        assert_eq!(scroll_to_reveal(&[1, 1], 0, 0, 0), 0);
        // A cursor past the end is clamped to the last row.
        assert_eq!(scroll_to_reveal(&[1, 1, 1], 99, 3, 0), 0);
    }

    #[test]
    fn max_scroll_is_zero_when_the_list_fits() {
        assert_eq!(max_sidebar_scroll(&[], 4), 0);
        assert_eq!(max_sidebar_scroll(&[1, 1, 1], 4), 0);
        // Exactly full still needs no scrolling.
        assert_eq!(max_sidebar_scroll(&[1, 1, 1, 1], 4), 0);
    }

    #[test]
    fn max_scroll_reaches_the_last_row() {
        // Ten 1-cell rows in a 4-row window: the end of the list is at 6.
        assert_eq!(max_sidebar_scroll(&[1usize; 10], 4), 6);
        // Two-line rows halve it: heights [2,2,2,2] in 4 rows ⇒ top 2.
        assert_eq!(max_sidebar_scroll(&[2, 2, 2, 2], 4), 2);
        // Mixed: [1,2,1,1] in 3 rows ⇒ the tail from 2 sums to 2 (fits),
        // from 1 it sums to 4 (doesn't).
        assert_eq!(max_sidebar_scroll(&[1, 2, 1, 1], 3), 2);
    }

    #[test]
    fn max_scroll_general_property_holds() {
        // The defining property: the tail from `max` fits, and the tail from
        // one row earlier does not (unless `max` is already 0).
        for heights in [
            vec![1usize; 10],
            vec![2, 1, 2, 1, 3, 1],
            vec![1, 1, 5, 1],
            vec![3],
        ] {
            for list_rows in 1..=8 {
                let m = max_sidebar_scroll(&heights, list_rows);
                let tail: usize = heights[m..].iter().sum();
                if m > 0 {
                    assert!(
                        tail <= list_rows,
                        "tail from {m} must fit in {list_rows}: {heights:?}"
                    );
                    let bigger: usize = heights[m - 1..].iter().sum();
                    assert!(
                        bigger > list_rows,
                        "one row earlier must NOT fit: {heights:?} / {list_rows}"
                    );
                }
                assert!(m < heights.len(), "max scroll stays a valid top index");
            }
        }
    }

    #[test]
    fn scroll_to_reveal_never_strands_the_tail() {
        // The reported bug: with the cursor at the very bottom, the window must
        // reach the end of the list — and an over-eager `desired` must not
        // scroll past it into blank space.
        let heights = vec![1usize; 10];
        assert_eq!(scroll_to_reveal(&heights, 9, 4, 0), 6);
        assert_eq!(scroll_to_reveal(&heights, 9, 4, 99), 6);
    }

    #[test]
    fn scroll_honors_desired_between_the_bounds() {
        // THE regression this change exists to prevent. The old clamp did
        // `desired.min(cursor)`, so a window deliberately scrolled to 4 with
        // the cursor at 5 collapsed back to the cursor's minimum fit (2).
        // A viewport is only pinned by its own bounds, not by the cursor.
        let heights = vec![1usize; 10];
        assert_eq!(scroll_to_reveal(&heights, 5, 4, 4), 4);
    }

    #[test]
    fn scroll_to_reveal_survives_a_row_taller_than_the_window() {
        // A 2-line detail row in a 1-row window can never "fit"; the walk must
        // still terminate and stay in range rather than spin or overflow.
        let heights = vec![2usize, 2, 2];
        for cursor in 0..3 {
            for desired in 0..4 {
                let s = scroll_to_reveal(&heights, cursor, 1, desired);
                assert!(s <= cursor, "scroll {s} must not pass cursor {cursor}");
            }
        }
    }

    #[test]
    fn scroll_invariants_hold_exhaustively() {
        // Whatever the shape, the resolved window must contain the cursor's
        // last line (when it can fit at all) and never pass the cursor.
        for heights in [vec![1usize; 8], vec![2, 1, 2, 1, 1], vec![1, 2, 1, 2, 1, 1]] {
            for list_rows in 1..=6 {
                for cursor in 0..heights.len() {
                    for desired in 0..=heights.len() {
                        let s = scroll_to_reveal(&heights, cursor, list_rows, desired);
                        assert!(s <= cursor, "scroll {s} passed cursor {cursor}");
                        // Walk the window and confirm the cursor row lands in it
                        // whenever a single row that tall could fit at all.
                        if heights[cursor] <= list_rows {
                            let used: usize = heights[s..cursor].iter().sum();
                            assert!(
                                used + heights[cursor] <= list_rows,
                                "cursor {cursor} not fully visible from scroll {s} \
                                 ({heights:?}, list_rows {list_rows})"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn sidebar_window_counts_hidden_above_and_below() {
        let heights = vec![1usize; 10];
        // Top of a 4-row window: nothing above, six rows below.
        let w = sidebar_window(&heights, 4, 0);
        assert_eq!((w.hidden_above, w.hidden_below), (0, 6));
        assert_eq!((w.first_visible, w.last_visible), (0, 3));
        assert_eq!(w.total_cells, 10);
        // Scrolled to the end: nothing hidden below — this is what the "N more"
        // chip keys off, so it must go quiet exactly when the tail is reachable.
        let w = sidebar_window(&heights, 4, 6);
        assert_eq!((w.hidden_above, w.hidden_below), (6, 0));
        assert_eq!(w.last_visible, 9);
    }

    #[test]
    fn sidebar_window_counts_a_clipped_tail_row_as_shown() {
        // A 2-line row starting on the window's last line gets clipped to one
        // line — but it IS on screen, so counting it as hidden would make the
        // chip claim a row is missing while the user is looking at it.
        let heights = vec![1usize, 1, 2];
        let w = sidebar_window(&heights, 3, 0);
        assert_eq!(w.hidden_below, 0);
        assert_eq!(w.last_visible, 2);
    }

    fn hit(y: usize, height: usize) -> RowHit {
        RowHit {
            visible_index: 0,
            y,
            height,
            kind: RowKind::Worktree,
            pin_key: String::new(),
            caret_x: None,
        }
    }

    #[test]
    fn row_at_maps_screen_row_into_row_bounds() {
        // Row 0 spans y=2 (height 2 ⇒ rows 2,3); row 1 spans y=4 (height 1).
        let hits = vec![hit(2, 2), hit(4, 1)];
        assert!(row_at(&hits, 1).is_none(), "above the first row");
        assert_eq!(row_at(&hits, 2).map(|h| h.y), Some(2));
        assert_eq!(
            row_at(&hits, 3).map(|h| h.y),
            Some(2),
            "second line of a 2-high row"
        );
        assert_eq!(row_at(&hits, 4).map(|h| h.y), Some(4));
        assert!(row_at(&hits, 5).is_none(), "below the last row");
    }

    // Regression: focusing the sidebar expands rows to a detail line
    // (`show_detail` + `compose_detail_line`), growing their heights and pushing
    // every row below downward. Paint (`draw_sidebar`) and the click hit-test
    // (`hit_rows`) share this `build_sidebar` pass, so a click MUST be resolved
    // against the focus state that was actually PAINTED. Flipping
    // `sidebar_focused` before the hit-test (the bug: `run.rs` used to set
    // `sb.focused = true` + sync before `on_left_press`) resolves the click
    // against the taller, not-yet-painted layout and lands it on the wrong
    // (higher) row — persistently, since activating a row hands focus back to the
    // center pane so every sidebar click is a focus transition. This locks that
    // focus really does shift the geometry.
    #[test]
    fn focus_expands_rows_and_shifts_geometry() {
        use crate::sidebar::SidebarRow;
        use thegn_core::config::FocusDetail;
        let rect = Rect {
            x: 0,
            y: 0,
            cols: 40,
            rows: 20,
        };
        let mut model = FrameModel {
            sidebar_rows: vec![
                SidebarRow::base(RowKind::Worktree, 1, "feat-a", "ws"),
                SidebarRow::base(RowKind::Worktree, 1, "feat-b", "ws"),
            ],
            ..FrameModel::default()
        };
        // Detail-on-focus for all rows, leading with the branch name — so a
        // focused row grows a second line regardless of git/PR/env data.
        model.sidebar_display = SidebarDisplay {
            focus_detail: FocusDetail::All,
            detail_branch: true,
            ..SidebarDisplay::default()
        };
        let y_of = |f: &SidebarFrame, idx: usize| {
            f.rows.iter().find(|p| p.visible_index == idx).map(|p| p.y)
        };

        model.sidebar_focused = false;
        let compact = build_sidebar(&model, rect, 0);
        model.sidebar_focused = true;
        let focused = build_sidebar(&model, rect, 0);

        // Same rows, same rect — but the second row sits lower once focused,
        // because the first row grew a detail line. A hit-test that used the
        // focused geometry against the compact pixels would mis-resolve by that
        // delta.
        assert!(
            y_of(&focused, 1) > y_of(&compact, 1),
            "focus must push the second row down: compact={:?} focused={:?}",
            y_of(&compact, 1),
            y_of(&focused, 1),
        );
    }
}
