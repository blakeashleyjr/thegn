//! In-process chrome: the four surfaces (tabbar, sidebar, panel, statusbar)
//! drawn natively into the back-buffer `Surface` around the center pane. No
//! WASM, no IPC, no broadcast — widgets read state directly and draw cells.
//! This replaces the four zellij plugins.

use termwiz::cell::AttributeChange;
use termwiz::color::{ColorAttribute, SrgbaTuple};
use termwiz::surface::{Change, Position, Surface};

use crate::compositor::{Rect, compose_pane};
use crate::emulator::PaneEmulator;
use superzej_core::theme;

/// Parse a theme `"r;g;b"` triple into a termwiz color.
pub fn theme_color(triple: &str) -> ColorAttribute {
    let mut it = triple
        .split(';')
        .filter_map(|s| s.trim().parse::<u8>().ok());
    match (it.next(), it.next(), it.next()) {
        (Some(r), Some(g), Some(b)) => ColorAttribute::TrueColorWithDefaultFallback(SrgbaTuple(
            r as f32 / 255.0,
            g as f32 / 255.0,
            b as f32 / 255.0,
            1.0,
        )),
        _ => ColorAttribute::Default,
    }
}

/// Write `text` at `(x, y)`, clipped to `max_cols`, with the given colors. Does
/// not fill beyond the text — use [`fill`] first for a solid background.
pub fn draw_text(
    surface: &mut Surface,
    x: usize,
    y: usize,
    text: &str,
    fg: ColorAttribute,
    bg: ColorAttribute,
    max_cols: usize,
) {
    surface.add_change(Change::CursorPosition {
        x: Position::Absolute(x),
        y: Position::Absolute(y),
    });
    surface.add_change(Change::Attribute(AttributeChange::Foreground(fg)));
    surface.add_change(Change::Attribute(AttributeChange::Background(bg)));
    let clipped: String = text.chars().take(max_cols).collect();
    surface.add_change(Change::Text(clipped));
}

/// Draw a transport-neutral plugin [`View`] into a host-owned surface rect.
/// Plugins supply semantic roles only; this function resolves them against the
/// current superzej theme/accent and clips to the host-owned slot.
///
/// Not yet wired into the live chrome — the plugin API surface (v0) landed
/// ahead of the host-side contribution renderer; covered by unit tests.
#[allow(dead_code)]
pub fn draw_plugin_view(
    surface: &mut Surface,
    rect: Rect,
    view: &superzej_core::plugin_api::View,
    accent_rgb: &str,
) {
    if rect.rows == 0 || rect.cols == 0 {
        return;
    }
    fill(surface, rect, theme_color(theme::BG1));
    let mut x = rect.x;
    let max_x = rect.x + rect.cols;
    for span in &view.spans {
        if x >= max_x {
            break;
        }
        let fg = plugin_role_color(span.role, accent_rgb);
        let bg = theme_color(theme::BG1);
        let max_cols = max_x.saturating_sub(x);
        draw_text(surface, x, rect.y, &span.text, fg, bg, max_cols);
        x += span.text.chars().take(max_cols).count();
    }
}

#[allow(dead_code)]
fn plugin_role_color(
    role: superzej_core::plugin_api::StyleRole,
    accent_rgb: &str,
) -> ColorAttribute {
    use superzej_core::plugin_api::StyleRole;
    match role {
        StyleRole::Default => theme_color(theme::TEXT),
        StyleRole::Accent => theme_color(accent_rgb),
        StyleRole::Warning => theme_color(theme::AMBER),
        StyleRole::Error => theme_color(theme::RED),
        StyleRole::Faint => theme_color(theme::FAINT),
    }
}

/// Clear the logical back-buffer before composing a new frame. This is not a
/// physical terminal clear: `BufferedTerminal` still diffs this logical state
/// against its prior frame and emits only changed cells.
pub fn clear_frame(surface: &mut Surface) {
    surface.add_change(Change::ClearScreen(theme_color(theme::BG0)));
}

/// Fill `rect` with spaces on `bg` (a solid background block).
pub fn fill(surface: &mut Surface, rect: Rect, bg: ColorAttribute) {
    let row = " ".repeat(rect.cols);
    for r in 0..rect.rows {
        surface.add_change(Change::CursorPosition {
            x: Position::Absolute(rect.x),
            y: Position::Absolute(rect.y + r),
        });
        surface.add_change(Change::Attribute(AttributeChange::Background(bg)));
        surface.add_change(Change::Attribute(AttributeChange::Foreground(
            ColorAttribute::Default,
        )));
        surface.add_change(Change::Text(row.clone()));
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
}

#[derive(Debug, Clone)]
pub struct RowMenuEntry {
    pub label: String,
    /// A stable id the event loop dispatches on (e.g. "open", "close", "pin").
    pub id: String,
}

/// What the chrome needs to paint a frame. Populated from session state + DB +
/// git by the host; kept renderer-agnostic so it's unit-testable.
#[derive(Debug, Clone, Default)]
#[allow(dead_code)]
pub struct FrameModel {
    pub tabs: Vec<String>,
    pub active_tab: usize,
    /// The structured workspace tree. Replaces the old flat `Vec<String>`:
    /// rows carry kind/depth/status so the renderer composes glyphs itself.
    pub sidebar_rows: Vec<crate::sidebar::SidebarRow>,
    /// Selection cursor: an index into the *visible* rows of `sidebar_rows`.
    pub sidebar_selected: usize,
    /// True when the sidebar currently owns keyboard focus (drives the
    /// focus indicator + per-row digit hints in [`draw_sidebar`]).
    pub sidebar_focused: bool,
    /// Active fuzzy-filter query echoed in the header (empty = none).
    pub sidebar_filter: String,
    /// True while the filter input sub-mode is capturing keystrokes.
    pub sidebar_filtering: bool,
    /// The current sort mode, shown in the header.
    pub sidebar_sort: crate::sidebar::SortMode,
    /// Row indices (into the visible list) that are multi-selected.
    pub sidebar_marked: std::collections::HashSet<usize>,
    /// When `Some`, an open row context menu: (anchor visible-row index,
    /// entries, menu cursor).
    pub sidebar_menu: Option<RowMenu>,
    /// Data carriers populated by the hydration thread and consumed by the
    /// event loop to (re)derive `sidebar_rows`. The (slug, display) workspace
    /// list in display order, and per-worktree git/agent/activity status.
    pub sidebar_workspaces: Vec<(String, String)>,
    pub sidebar_status: crate::sidebar::SidebarStatus,
    /// Structured Diff/PR/Checks payload for the right panel.
    pub panel: crate::panel::PanelData,
    /// True when the right panel currently owns keyboard focus.
    pub panel_focused: bool,
    pub status: String,
    pub accent: String,
    /// Pin chips for the tabbar (label + status glyph), in `Alt-N` order.
    pub pins: Vec<crate::pins::PinChip>,
}

impl FrameModel {
    pub fn accent_or_default(&self) -> &str {
        if self.accent.is_empty() {
            theme::TEAL
        } else {
            &self.accent
        }
    }
}

pub fn draw_tabbar(surface: &mut Surface, rect: Rect, content: Rect, model: &FrameModel) {
    if rect.rows == 0 {
        return;
    }
    fill(surface, rect, theme_color(theme::BG1));
    if content.rows == 0 || content.cols == 0 {
        return;
    }
    let accent = theme_color(model.accent_or_default());
    let dim = theme_color(theme::DIM);
    let content_end = content.x.saturating_add(content.cols);

    // Right-align the pin chips first so the tab labels know where to stop.
    let chips_start = draw_pin_chips(surface, content, content_end, model, accent, dim);

    let mut x = content.x.saturating_add(1);
    for (i, name) in model.tabs.iter().enumerate() {
        if x >= chips_start {
            break;
        }
        let label = format!(" {name} ");
        let fg = if i == model.active_tab { accent } else { dim };
        let max = chips_start.saturating_sub(x);
        draw_text(
            surface,
            x,
            content.y,
            &label,
            fg,
            theme_color(theme::BG1),
            max,
        );
        x += label.chars().count();
    }
}

/// Render pin chips (`glyph label`) right-aligned in the tabbar content area.
/// Returns the left-most x the chips occupy, so tab labels can stop before them.
fn draw_pin_chips(
    surface: &mut Surface,
    content: Rect,
    content_end: usize,
    model: &FrameModel,
    accent: ColorAttribute,
    dim: ColorAttribute,
) -> usize {
    if model.pins.is_empty() {
        return content_end;
    }
    // Each chip reads " <glyph> <label> " (the leading index is implicit Alt-N).
    let chips: Vec<String> = model
        .pins
        .iter()
        .map(|c| format!(" {} {} ", c.glyph, c.label))
        .collect();
    let total: usize = chips.iter().map(|s| s.chars().count()).sum();
    let mut x = content_end.saturating_sub(total).max(content.x);
    let chips_start = x;
    let bg = theme_color(theme::BG1);
    for (chip, pin) in chips.iter().zip(model.pins.iter()) {
        if x >= content_end {
            break;
        }
        // Running pins read in the accent; stopped/failed read dim.
        let fg = if pin.glyph == crate::pins::PinHealth::Running.glyph() {
            accent
        } else {
            dim
        };
        let max = content_end.saturating_sub(x);
        draw_text(surface, x, content.y, chip, fg, bg, max);
        x += chip.chars().count();
    }
    chips_start
}

pub fn draw_statusbar(surface: &mut Surface, rect: Rect, model: &FrameModel) {
    if rect.rows == 0 {
        return;
    }
    fill(surface, rect, theme_color(theme::BG1));
    draw_text(
        surface,
        rect.x + 1,
        rect.y,
        &model.status,
        theme_color(theme::FAINT),
        theme_color(theme::BG1),
        rect.cols.saturating_sub(1),
    );
}

pub fn draw_sidebar(surface: &mut Surface, rect: Rect, model: &FrameModel) {
    fill(surface, rect, theme_color(theme::BG0));
    let accent = theme_color(model.accent_or_default());

    // Header: either the live filter input, or "WORKSPACES" + sort tag. The
    // focused marker (◂) signals the sidebar owns input.
    if model.sidebar_filtering || !model.sidebar_filter.is_empty() {
        let header = format!(" /{}", model.sidebar_filter);
        draw_text(
            surface,
            rect.x,
            rect.y,
            &header,
            accent,
            theme_color(theme::BG0),
            rect.cols,
        );
    } else {
        let marker = if model.sidebar_focused {
            " \u{25c2}"
        } else {
            ""
        };
        let title = format!(" WORKSPACES{marker}");
        draw_text(
            surface,
            rect.x,
            rect.y,
            &title,
            accent,
            theme_color(theme::BG0),
            rect.cols,
        );
        // Right-aligned 1-letter sort tag (n/r/a) when focused.
        if model.sidebar_focused && rect.cols >= 3 {
            let tag = &model.sidebar_sort.as_str()[..1];
            draw_text(
                surface,
                rect.x + rect.cols - 2,
                rect.y,
                tag,
                theme_color(theme::FAINT),
                theme_color(theme::BG0),
                1,
            );
        }
    }

    // Only visible rows are listed; the selection index is into that subset.
    let visible: Vec<&crate::sidebar::SidebarRow> =
        model.sidebar_rows.iter().filter(|r| r.visible).collect();

    for (i, row) in visible.iter().enumerate() {
        let y = rect.y + 1 + i;
        if y >= rect.y + rect.rows {
            break;
        }
        let selected = i == model.sidebar_selected;
        let marked = model.sidebar_marked.contains(&i);
        let bg = if selected {
            theme_color(theme::PANEL2)
        } else if marked {
            theme_color(theme::PANEL)
        } else {
            theme_color(theme::BG0)
        };
        if selected || marked {
            fill(
                surface,
                Rect {
                    x: rect.x,
                    y,
                    cols: rect.cols,
                    rows: 1,
                },
                bg,
            );
        }
        // Left-edge accent bar marks the cursor only while focused, so a stale
        // selection isn't mistaken for focus.
        if selected && model.sidebar_focused {
            draw_text(surface, rect.x, y, "\u{2590}", accent, bg, 1);
        }

        let composed = compose_sidebar_row(row, i, model.sidebar_focused);
        let fg = if row.active {
            accent
        } else if selected {
            theme_color(theme::TEXT)
        } else {
            theme_color(theme::DIM)
        };
        draw_text(
            surface,
            rect.x + 1,
            y,
            &composed.text,
            fg,
            bg,
            rect.cols.saturating_sub(1),
        );
        // Overpaint the status segment (git/agent/activity) in its own colors,
        // right after the label, when there's room.
        if let Some(seg) = composed.status {
            let sx = rect.x + 1 + composed.status_col;
            if sx < rect.x + rect.cols {
                draw_text(
                    surface,
                    sx,
                    y,
                    &seg,
                    status_seg_color(row),
                    bg,
                    (rect.x + rect.cols).saturating_sub(sx),
                );
            }
        }
    }

    // Row context menu overlay (item 27).
    if let Some(menu) = &model.sidebar_menu {
        draw_row_menu(surface, rect, menu, accent);
    }
}

/// The text composed for a row plus where its status segment begins (so the
/// renderer can recolor it). `text` already includes caret/connector/label and
/// a trailing space before the status; `status` is the git/agent/activity tail.
struct ComposedRow {
    text: String,
    status: Option<String>,
    status_col: usize,
}

fn compose_sidebar_row(
    row: &crate::sidebar::SidebarRow,
    visible_index: usize,
    focused: bool,
) -> ComposedRow {
    use crate::sidebar::RowKind;
    let mut text = String::new();

    // Quick-jump number (item 24): a 1-based index shown only while focused,
    // for the first 9 rows.
    if focused && visible_index < 9 {
        text.push_str(&format!("{} ", visible_index + 1));
    }

    match row.kind {
        RowKind::Workspace => {
            let caret = if row.collapsed {
                "\u{25b8}"
            } else {
                "\u{25be}"
            };
            text.push_str(caret);
            text.push(' ');
            text.push_str(&row.label);
        }
        RowKind::Worktree => {
            text.push_str("  ");
            text.push_str(activity_dot(row.activity));
            text.push_str(&row.label);
        }
        RowKind::Page => {
            text.push_str("      ");
            text.push_str(&row.label);
        }
    }

    // Agent glyph (item 19) sits just after the label.
    if let Some(agent) = &row.agent {
        text.push(' ');
        text.push_str(&superzej_core::theme::agent_glyph(agent));
    }

    // Git glyphs (item 18) form the recolored status tail.
    let status = row.git.map(|g| {
        let mut s = String::new();
        if g.dirty {
            s.push_str(" \u{25cf}"); // ●
        }
        if g.ahead > 0 {
            s.push_str(&format!(" \u{2191}{}", g.ahead)); // ↑N
        }
        if g.behind > 0 {
            s.push_str(&format!(" \u{2193}{}", g.behind)); // ↓N
        }
        s
    });
    let status = status.filter(|s| !s.is_empty());
    let status_col = text.chars().count();
    ComposedRow {
        text,
        status,
        status_col,
    }
}

/// The activity dot prefix for a worktree row (item 20). Active rows pulse via
/// the accent; quiet rows show a steady amber "look at me"; idle shows nothing.
fn activity_dot(state: crate::sidebar::ActivityState) -> &'static str {
    use crate::sidebar::ActivityState::*;
    match state {
        Active => "\u{25cf} ", // ● (colored at render via row.active/accent path is separate)
        Quiet => "\u{25cb} ",  // ○
        None => "",
    }
}

fn status_seg_color(row: &crate::sidebar::SidebarRow) -> ColorAttribute {
    // Dirty dominates the tail color; otherwise neutral-dim and the ↑↓ read
    // fine. (Per-glyph coloring is a later refinement.)
    match row.git {
        Some(g) if g.dirty => theme_color(theme::AMBER),
        Some(g) if g.ahead > 0 || g.behind > 0 => theme_color(theme::DIM),
        _ => theme_color(theme::DIM),
    }
}

fn draw_row_menu(surface: &mut Surface, rect: Rect, menu: &RowMenu, accent: ColorAttribute) {
    let width = rect.cols;
    let top = (rect.y + 1 + menu.anchor + 1).min(rect.y + rect.rows.saturating_sub(1));
    for (i, entry) in menu.entries.iter().enumerate() {
        let y = top + i;
        if y >= rect.y + rect.rows {
            break;
        }
        let sel = i == menu.cursor;
        let bg = if sel {
            theme_color(theme::RAISE)
        } else {
            theme_color(theme::PANEL)
        };
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
        let fg = if sel {
            accent
        } else {
            theme_color(theme::TEXT)
        };
        draw_text(
            surface,
            rect.x + 1,
            y,
            &format!("\u{203a} {}", entry.label),
            fg,
            bg,
            width.saturating_sub(1),
        );
    }
}

/// Draw the tabbed right panel: a `DIFF | PR | CHECKS` tab bar (row 0), the
/// active tab's body, and a context-sensitive help bar (last row).
pub fn draw_panel(
    surface: &mut Surface,
    rect: Rect,
    model: &FrameModel,
    ui: &crate::panel::PanelUi,
) {
    use crate::panel::PanelTab;
    fill(surface, rect, theme_color(theme::PANEL));
    if rect.rows == 0 || rect.cols == 0 {
        return;
    }
    let accent = theme_color(model.accent_or_default());

    // Row 0: tab bar.
    draw_panel_tabbar(surface, rect, ui.tab, accent, model.panel_focused);

    // Body: rows 1..rows-1 (leave the last row for the help bar).
    let body = Rect {
        x: rect.x,
        y: rect.y + 1,
        cols: rect.cols,
        rows: rect.rows.saturating_sub(2),
    };
    match ui.tab {
        PanelTab::Diff => draw_diff_tab(surface, body, model, ui, accent),
        PanelTab::Pr => draw_pr_tab(surface, body, model, accent),
        PanelTab::Checks => draw_checks_tab(surface, body, model),
    }

    // Last row: help bar.
    if rect.rows >= 2 {
        let help_y = rect.y + rect.rows - 1;
        let hint = panel_help_hint(ui.tab, ui.diff_view);
        let help_rect = Rect {
            x: rect.x,
            y: help_y,
            cols: rect.cols,
            rows: 1,
        };
        fill(surface, help_rect, theme_color(theme::BG1));
        draw_text(
            surface,
            rect.x + 1,
            help_y,
            hint,
            theme_color(theme::FAINT),
            theme_color(theme::BG1),
            rect.cols.saturating_sub(1),
        );
    }
}

fn draw_panel_tabbar(
    surface: &mut Surface,
    rect: Rect,
    active: crate::panel::PanelTab,
    accent: ColorAttribute,
    focused: bool,
) {
    use crate::panel::PanelTab;
    let bar = Rect {
        x: rect.x,
        y: rect.y,
        cols: rect.cols,
        rows: 1,
    };
    fill(surface, bar, theme_color(theme::BG1));
    let mut x = rect.x + 1;
    for (tab, label) in [
        (PanelTab::Diff, "DIFF"),
        (PanelTab::Pr, "PR"),
        (PanelTab::Checks, "CHECKS"),
    ] {
        let seg = format!(" {label} ");
        let fg = if tab == active {
            accent
        } else {
            theme_color(theme::DIM)
        };
        let max = (rect.x + rect.cols).saturating_sub(x);
        if max == 0 {
            break;
        }
        draw_text(surface, x, rect.y, &seg, fg, theme_color(theme::BG1), max);
        x += seg.chars().count();
    }
    // A right-aligned focus marker so a focused panel is unmistakable.
    if focused {
        let marker = "\u{25c2}";
        let mx = (rect.x + rect.cols).saturating_sub(2);
        if mx > x {
            draw_text(
                surface,
                mx,
                rect.y,
                marker,
                accent,
                theme_color(theme::BG1),
                1,
            );
        }
    }
}

fn draw_diff_tab(
    surface: &mut Surface,
    rect: Rect,
    model: &FrameModel,
    ui: &crate::panel::PanelUi,
    accent: ColorAttribute,
) {
    use crate::panel::DiffView;
    match ui.diff_view {
        DiffView::FileList => draw_diff_filelist(surface, rect, model, ui, accent),
        DiffView::FileDiff => draw_diff_filediff(surface, rect, ui),
    }
}

fn draw_diff_filelist(
    surface: &mut Surface,
    rect: Rect,
    model: &FrameModel,
    ui: &crate::panel::PanelUi,
    accent: ColorAttribute,
) {
    if model.panel.files.is_empty() {
        draw_text(
            surface,
            rect.x + 1,
            rect.y,
            "no changes",
            theme_color(theme::FAINT),
            theme_color(theme::PANEL),
            rect.cols.saturating_sub(1),
        );
        return;
    }
    for (i, f) in model.panel.files.iter().enumerate() {
        let y = rect.y + i;
        if i >= rect.rows {
            break;
        }
        let selected = model.panel_focused && i == ui.diff_cursor;
        let bg = if selected {
            theme_color(theme::PANEL2)
        } else {
            theme_color(theme::PANEL)
        };
        if selected {
            fill(
                surface,
                Rect {
                    x: rect.x,
                    y,
                    cols: rect.cols,
                    rows: 1,
                },
                bg,
            );
            draw_text(surface, rect.x, y, "\u{2590}", accent, bg, 1);
        }
        let status_color = match f.status {
            'A' => theme_color(theme::GREEN),
            'D' => theme_color(theme::RED),
            _ => theme_color(theme::AMBER),
        };
        draw_text(
            surface,
            rect.x + 1,
            y,
            &f.status.to_string(),
            status_color,
            bg,
            1,
        );
        draw_text(
            surface,
            rect.x + 3,
            y,
            &f.path,
            theme_color(theme::TEXT),
            bg,
            rect.cols.saturating_sub(3),
        );
    }
}

fn draw_diff_filediff(surface: &mut Surface, rect: Rect, ui: &crate::panel::PanelUi) {
    let lines: Vec<&str> = ui.file_diff.lines().collect();
    for (row, line) in lines.iter().skip(ui.diff_scroll).enumerate() {
        let y = rect.y + row;
        if row >= rect.rows {
            break;
        }
        // Color +/- lines; everything else is plain.
        let fg = match line.as_bytes().first() {
            Some(b'+') => theme_color(theme::GREEN),
            Some(b'-') => theme_color(theme::RED),
            Some(b'@') => theme_color(theme::PURPLE),
            _ => theme_color(theme::DIM),
        };
        draw_text(
            surface,
            rect.x + 1,
            y,
            line,
            fg,
            theme_color(theme::PANEL),
            rect.cols.saturating_sub(1),
        );
    }
}

fn draw_pr_tab(surface: &mut Surface, rect: Rect, model: &FrameModel, accent: ColorAttribute) {
    if let Some(pr) = &model.panel.pr {
        let state_color = match pr.state.as_str() {
            "OPEN" => theme_color(theme::GREEN),
            "MERGED" => theme_color(theme::PURPLE),
            "CLOSED" => theme_color(theme::RED),
            _ => theme_color(theme::DIM),
        };
        let draft = if pr.is_draft { " (draft)" } else { "" };
        draw_text(
            surface,
            rect.x + 1,
            rect.y,
            &format!("#{}", pr.number),
            accent,
            theme_color(theme::PANEL),
            rect.cols.saturating_sub(1),
        );
        let num_w = format!("#{}", pr.number).chars().count() + 2;
        draw_text(
            surface,
            rect.x + 1 + num_w,
            rect.y,
            &format!("{}{}", pr.state, draft),
            state_color,
            theme_color(theme::PANEL),
            rect.cols.saturating_sub(1 + num_w),
        );
        draw_text(
            surface,
            rect.x + 1,
            rect.y + 1,
            &pr.title,
            theme_color(theme::TEXT),
            theme_color(theme::PANEL),
            rect.cols.saturating_sub(1),
        );
        if let Some(decision) = &pr.review_decision {
            draw_text(
                surface,
                rect.x + 1,
                rect.y + 3,
                decision,
                theme_color(theme::DIM),
                theme_color(theme::PANEL),
                rect.cols.saturating_sub(1),
            );
        }
    } else {
        let note = model.panel.pr_note.as_deref().unwrap_or("no pull request");
        draw_text(
            surface,
            rect.x + 1,
            rect.y,
            note,
            theme_color(theme::FAINT),
            theme_color(theme::PANEL),
            rect.cols.saturating_sub(1),
        );
    }
}

fn draw_checks_tab(surface: &mut Surface, rect: Rect, model: &FrameModel) {
    use crate::panel::CheckState;
    if model.panel.checks.is_empty() {
        draw_text(
            surface,
            rect.x + 1,
            rect.y,
            "no checks",
            theme_color(theme::FAINT),
            theme_color(theme::PANEL),
            rect.cols.saturating_sub(1),
        );
        return;
    }
    for (i, c) in model.panel.checks.iter().enumerate() {
        let y = rect.y + i;
        if i >= rect.rows {
            break;
        }
        let (glyph, color) = match c.state {
            CheckState::Pass => ("\u{2713}", theme_color(theme::GREEN)),
            CheckState::Fail => ("\u{2717}", theme_color(theme::RED)),
            CheckState::Pending => ("\u{2022}", theme_color(theme::AMBER)),
        };
        draw_text(
            surface,
            rect.x + 1,
            y,
            glyph,
            color,
            theme_color(theme::PANEL),
            1,
        );
        draw_text(
            surface,
            rect.x + 3,
            y,
            &c.name,
            theme_color(theme::TEXT),
            theme_color(theme::PANEL),
            rect.cols.saturating_sub(3),
        );
    }
}

/// The context-sensitive help-bar hint for the active tab/view.
fn panel_help_hint(tab: crate::panel::PanelTab, view: crate::panel::DiffView) -> &'static str {
    use crate::panel::{DiffView, PanelTab};
    match (tab, view) {
        (PanelTab::Diff, DiffView::FileList) => "1/2/3 tab  j/k move  ↵ open  o edit  esc",
        (PanelTab::Diff, DiffView::FileDiff) => "j/k scroll  o edit  esc back",
        (PanelTab::Pr, _) => "1/2/3 tab  o browser  m merge  a approve  c create  esc",
        (PanelTab::Checks, _) => "1/2/3 tab  r rerun  esc",
    }
}

/// One pin's slot in the top strip: where it sits and how to label it. The
/// emulator is looked up by the caller via `pane`.
pub struct StripCell {
    pub pane: crate::center::PaneId,
    pub rect: Rect,
    pub label: String,
    pub glyph: char,
    pub focused: bool,
}

/// Render the top pinned-program strip: for each cell, a 1-row accent header
/// (`glyph label`) then its live pane below. A 1-col gap between cells reads as a
/// divider. The strip background is painted first so empty slack is themed.
pub fn draw_strip<'a>(
    surface: &mut Surface,
    strip: Rect,
    cells: &[StripCell],
    accent: &str,
    lookup: impl Fn(crate::center::PaneId) -> Option<&'a dyn PaneEmulator>,
) {
    if strip.rows == 0 || strip.cols == 0 {
        return;
    }
    fill(surface, strip, theme_color(theme::BG0));
    let accent_c = theme_color(accent);
    let dim = theme_color(theme::DIM);
    for cell in cells {
        if cell.rect.rows == 0 || cell.rect.cols == 0 {
            continue;
        }
        // Header row.
        let header_bg = theme_color(theme::BG1);
        let header_rect = Rect {
            x: cell.rect.x,
            y: cell.rect.y,
            cols: cell.rect.cols,
            rows: 1,
        };
        fill(surface, header_rect, header_bg);
        let fg = if cell.focused { accent_c } else { dim };
        let text = format!(" {} {} ", cell.glyph, cell.label);
        draw_text(
            surface,
            cell.rect.x,
            cell.rect.y,
            &text,
            fg,
            header_bg,
            cell.rect.cols,
        );
        // Pane body below the header.
        if cell.rect.rows > 1
            && let Some(emu) = lookup(cell.pane)
        {
            let body = Rect {
                x: cell.rect.x,
                y: cell.rect.y + 1,
                cols: cell.rect.cols,
                rows: cell.rect.rows - 1,
            };
            compose_pane(surface, emu, body);
        }
    }
}

/// Draw the surrounding chrome (sidebar/panel/tabbar/statusbar) — the center is
/// filled separately by [`render_tab`].
pub fn draw_chrome(
    surface: &mut Surface,
    chrome: &crate::layout::ChromeLayout,
    model: &FrameModel,
    panel_ui: &crate::panel::PanelUi,
) {
    if let Some(sb) = chrome.sidebar {
        draw_sidebar(surface, sb, model);
    }
    if let Some(pn) = chrome.panel {
        draw_panel(surface, pn, model, panel_ui);
    }
    draw_tabbar(surface, chrome.tabbar, chrome.tabbar_content(), model);
    draw_statusbar(surface, chrome.statusbar, model);
}

/// Compose a multi-pane tab: lay the `center` tree out within `chrome.center`,
/// paint each visible pane (resolved via `lookup`), draw a 1-row accent header
/// above the focused split when there's more than one pane, then the chrome.
pub fn render_tab<'a>(
    surface: &mut Surface,
    chrome: &crate::layout::ChromeLayout,
    center: &crate::center::CenterTree,
    focused: crate::center::PaneId,
    model: &FrameModel,
    panel_ui: &crate::panel::PanelUi,
    lookup: impl Fn(crate::center::PaneId) -> Option<&'a dyn PaneEmulator>,
) {
    let _ = focused; // a non-destructive focus border is a later polish
    for (id, rect) in center.layout(chrome.center) {
        if let Some(emu) = lookup(id) {
            compose_pane(surface, emu, rect);
        }
    }
    draw_chrome(surface, chrome, model, panel_ui);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::emulator::Vt100Emulator;
    use crate::layout;

    fn lines(s: &Surface) -> Vec<String> {
        s.screen_chars_to_string()
            .lines()
            .map(|l| l.to_string())
            .collect()
    }

    /// Build a minimal sidebar row for renderer tests.
    fn row(kind: crate::sidebar::RowKind, label: &str) -> crate::sidebar::SidebarRow {
        crate::sidebar::SidebarRow {
            kind,
            depth: if kind == crate::sidebar::RowKind::Workspace {
                0
            } else {
                1
            },
            label: label.into(),
            workspace_slug: "app".into(),
            tab_target: None,
            active: false,
            worktree_path: None,
            pin_key: label.into(),
            branch: None,
            git: None,
            agent: None,
            activity: crate::sidebar::ActivityState::None,
            visible: true,
            collapsed: false,
        }
    }

    #[test]
    fn sidebar_focus_indicator_appears_only_when_focused() {
        let rect = Rect {
            x: 0,
            y: 0,
            cols: 24,
            rows: 6,
        };
        use crate::sidebar::RowKind;
        let model = FrameModel {
            sidebar_rows: vec![
                row(RowKind::Workspace, "app"),
                row(RowKind::Worktree, "home"),
            ],
            sidebar_selected: 1,
            sidebar_focused: true,
            ..Default::default()
        };
        let mut s = Surface::new(24, 6);
        draw_sidebar(&mut s, rect, &model);
        let text = s.screen_chars_to_string();
        assert!(text.contains('\u{25c2}'), "focused header marker: {text:?}");

        let mut unfocused = model.clone();
        unfocused.sidebar_focused = false;
        let mut s2 = Surface::new(24, 6);
        draw_sidebar(&mut s2, rect, &unfocused);
        let text2 = s2.screen_chars_to_string();
        assert!(
            !text2.contains('\u{25c2}'),
            "no focus marker when unfocused: {text2:?}"
        );
    }

    #[test]
    fn sidebar_renders_glyphs_caret_dirty_and_agent() {
        use crate::sidebar::{ActivityState, GitGlyphs, RowKind};
        let rect = Rect {
            x: 0,
            y: 0,
            cols: 30,
            rows: 8,
        };
        let mut ws = row(RowKind::Workspace, "app");
        ws.collapsed = false;
        let mut wt = row(RowKind::Worktree, "feat");
        wt.git = Some(GitGlyphs {
            dirty: true,
            ahead: 2,
            behind: 1,
        });
        wt.agent = Some("claude".into());
        wt.activity = ActivityState::Active;
        let model = FrameModel {
            sidebar_rows: vec![ws, wt],
            sidebar_selected: 0,
            sidebar_focused: true,
            ..Default::default()
        };
        let mut s = Surface::new(30, 8);
        draw_sidebar(&mut s, rect, &model);
        let text = s.screen_chars_to_string();
        assert!(text.contains('\u{25be}'), "expanded caret ▾: {text:?}"); // expanded workspace
        assert!(text.contains("feat"));
        assert!(text.contains('\u{2191}'), "ahead glyph ↑: {text:?}");
        assert!(text.contains('\u{2193}'), "behind glyph ↓: {text:?}");
        assert!(text.contains('C'), "agent glyph for claude: {text:?}");
    }

    #[test]
    fn clear_frame_removes_stale_cells_from_logical_surface() {
        let mut s = Surface::new(20, 3);
        draw_text(
            &mut s,
            0,
            0,
            "STALE",
            theme_color(theme::TEXT),
            theme_color(theme::BG1),
            20,
        );
        assert!(s.screen_chars_to_string().contains("STALE"));

        clear_frame(&mut s);
        let text = s.screen_chars_to_string();
        assert!(!text.contains("STALE"), "logical clear removes old cells");
    }
    #[test]
    fn plugin_view_is_host_rendered_with_semantic_roles() {
        use superzej_core::plugin_api::{Span, StyleRole, View};

        let mut s = Surface::new(20, 1);
        let view = View::line([
            Span::styled("ok", StyleRole::Accent),
            Span::styled(" warn", StyleRole::Warning),
        ]);
        draw_plugin_view(
            &mut s,
            Rect {
                x: 0,
                y: 0,
                cols: 20,
                rows: 1,
            },
            &view,
            theme::TEAL,
        );

        let text = s.screen_chars_to_string();
        assert!(text.contains("ok warn"), "{text:?}");
    }

    #[test]
    fn tabbar_shows_tab_names() {
        let mut s = Surface::new(80, 1);
        let model = FrameModel {
            tabs: vec!["app/home".into(), "app/feat".into()],
            active_tab: 1,
            ..Default::default()
        };
        let rect = Rect {
            x: 0,
            y: 0,
            cols: 80,
            rows: 1,
        };
        draw_tabbar(&mut s, rect, rect, &model);
        let l = lines(&s);
        assert!(l[0].contains("app/home"));
        assert!(l[0].contains("app/feat"));
    }

    #[test]
    fn tabbar_renders_pin_chips_right_aligned() {
        let mut s = Surface::new(80, 1);
        let model = FrameModel {
            tabs: vec!["app/home".into()],
            active_tab: 0,
            pins: vec![
                crate::pins::PinChip {
                    index: 1,
                    label: "mail".into(),
                    glyph: crate::pins::PinHealth::Running.glyph(),
                },
                crate::pins::PinChip {
                    index: 2,
                    label: "logs".into(),
                    glyph: crate::pins::PinHealth::Stopped.glyph(),
                },
            ],
            ..Default::default()
        };
        let rect = Rect {
            x: 0,
            y: 0,
            cols: 80,
            rows: 1,
        };
        draw_tabbar(&mut s, rect, rect, &model);
        let row = &lines(&s)[0];
        assert!(row.contains("mail"), "chip label present: {row:?}");
        assert!(row.contains("logs"));
        assert!(row.contains("app/home"), "tab label still present");
        // The chips are right of the tab label.
        let tab_at = row.find("app/home").unwrap();
        let mail_at = row.find("mail").unwrap();
        assert!(mail_at > tab_at, "chips render to the right of tabs");
    }

    #[test]
    fn strip_draws_header_label_and_glyph() {
        let mut s = Surface::new(40, 6);
        let strip = Rect {
            x: 0,
            y: 0,
            cols: 40,
            rows: 6,
        };
        let emu = Vt100Emulator::new(5, 40, 100);
        let cells = vec![StripCell {
            pane: 1,
            rect: strip,
            label: "syslog".into(),
            glyph: crate::pins::PinHealth::Running.glyph(),
            focused: true,
        }];
        draw_strip(&mut s, strip, &cells, theme::TEAL, |id| {
            (id == 1).then_some(&emu as &dyn PaneEmulator)
        });
        let header = &lines(&s)[0];
        assert!(header.contains("syslog"), "header label: {header:?}");
        assert!(header.contains(crate::pins::PinHealth::Running.glyph()));
    }

    #[test]
    fn tabbar_labels_start_in_center_content_area_when_sidebar_is_visible() {
        let chrome = layout::compute(160, 10, true, true);
        let mut s = Surface::new(160, 10);
        let model = FrameModel {
            tabs: vec!["repo/home".into(), "repo/feat".into()],
            active_tab: 0,
            ..Default::default()
        };

        draw_chrome(&mut s, &chrome, &model, &crate::panel::PanelUi::default());

        let l = lines(&s);
        let row = &l[0];
        let far_left: String = row.chars().take(chrome.center.x).collect();
        let center_band: String = row
            .chars()
            .skip(chrome.center.x)
            .take(chrome.center.cols)
            .collect();

        assert!(
            !far_left.contains("repo/home") && !far_left.contains("repo/feat"),
            "tab labels should not draw in sidebar columns: {row:?}"
        );
        assert!(
            center_band.contains("repo/home") && center_band.contains("repo/feat"),
            "tab labels should draw in center band: {row:?}"
        );
    }

    #[test]
    fn full_frame_tab_label_is_center_aligned_not_far_left() {
        let cols = 160usize;
        let rows = 10usize;
        let chrome = layout::compute(cols, rows, true, true);
        let mut emu = Vt100Emulator::new(chrome.center.rows as u16, chrome.center.cols as u16, 0);
        emu.advance(b"CENTER");
        let model = FrameModel {
            tabs: vec!["repo/home".into()],
            active_tab: 0,
            ..Default::default()
        };
        let center = crate::center::CenterTree::Leaf(1);
        let mut s = Surface::new(cols, rows);

        render_tab(
            &mut s,
            &chrome,
            &center,
            1,
            &model,
            &crate::panel::PanelUi::default(),
            |id| (id == 1).then_some(&emu as &dyn PaneEmulator),
        );

        let l = lines(&s);
        let row = &l[0];
        let far_left: String = row.chars().take(chrome.center.x).collect();
        let center_band: String = row
            .chars()
            .skip(chrome.center.x)
            .take(chrome.center.cols)
            .collect();

        assert!(
            !far_left.contains("repo/home"),
            "tab label should not flash/draw at far-left: {row:?}"
        );
        assert!(
            center_band.contains("repo/home"),
            "tab label should appear in center band: {row:?}"
        );
    }

    #[test]
    fn render_tab_paints_every_visible_pane() {
        use crate::center::{Branch, CenterTree, Dir};
        let cols = 160usize;
        let rows = 40usize;
        let chrome = layout::compute(cols, rows, false, false); // full-width center

        // Two side-by-side panes (ids 1 and 2).
        let center = CenterTree::Split {
            dir: Dir::Row,
            children: vec![
                Branch {
                    weight: 1.0,
                    child: CenterTree::Leaf(1),
                },
                Branch {
                    weight: 1.0,
                    child: CenterTree::Leaf(2),
                },
            ],
        };
        let half = (chrome.center.cols / 2) as u16;
        let mut left = Vt100Emulator::new(chrome.center.rows as u16, half, 0);
        left.advance(b"LEFTPANE");
        let mut right = Vt100Emulator::new(chrome.center.rows as u16, half, 0);
        right.advance(b"RIGHTPANE");

        let model = FrameModel {
            tabs: vec!["repo/home".into()],
            ..Default::default()
        };
        let mut s = Surface::new(cols, rows);
        render_tab(
            &mut s,
            &chrome,
            &center,
            1,
            &model,
            &crate::panel::PanelUi::default(),
            |id| match id {
                1 => Some(&left as &dyn PaneEmulator),
                2 => Some(&right as &dyn PaneEmulator),
                _ => None,
            },
        );
        let text = s.screen_chars_to_string();
        assert!(text.contains("LEFTPANE"), "left pane painted");
        assert!(text.contains("RIGHTPANE"), "right pane painted");
    }

    #[test]
    fn full_frame_places_chrome_and_center_pane() {
        let cols = 160usize;
        let rows = 40usize;
        let chrome = layout::compute(cols, rows, true, true);

        let mut emu = Vt100Emulator::new(chrome.center.rows as u16, chrome.center.cols as u16, 0);
        emu.advance(b"CENTER-CONTENT");

        let model = FrameModel {
            tabs: vec!["repo/home".into()],
            active_tab: 0,
            sidebar_rows: vec![
                row(crate::sidebar::RowKind::Workspace, "repo"),
                row(crate::sidebar::RowKind::Worktree, "feat"),
            ],
            panel: crate::panel::PanelData {
                branch: "feat".into(),
                pr: Some(crate::panel::PrSummary {
                    number: 42,
                    title: "a pr".into(),
                    state: "OPEN".into(),
                    url: "https://example/42".into(),
                    is_draft: false,
                    review_decision: None,
                }),
                ..Default::default()
            },
            status: "Cmd-K  Alt-w new  Alt-o switch".into(),
            ..Default::default()
        };

        let mut s = Surface::new(cols, rows);
        let center = crate::center::CenterTree::Leaf(1);
        // PR tab so the #42 summary is on screen.
        let panel_ui = crate::panel::PanelUi {
            tab: crate::panel::PanelTab::Pr,
            ..Default::default()
        };
        render_tab(&mut s, &chrome, &center, 1, &model, &panel_ui, |id| {
            (id == 1).then_some(&emu as &dyn PaneEmulator)
        });
        let l = lines(&s);

        // Tabbar (row 0) carries the tab name; statusbar (last row) the hints.
        assert!(l[0].contains("repo/home"), "tabbar: {:?}", l[0]);
        assert!(l[rows - 1].contains("Cmd-K"), "status: {:?}", l[rows - 1]);
        // Sidebar title and the center content both present somewhere.
        let all = l.join("\n");
        assert!(all.contains("WORKSPACES"));
        assert!(all.contains("CENTER-CONTENT"));
        assert!(all.contains("#42"));
    }

    #[test]
    fn panel_renders_tab_bar_and_diff_files() {
        use crate::panel::{DiffFile, PanelData, PanelUi};
        let rect = Rect {
            x: 0,
            y: 0,
            cols: 44,
            rows: 12,
        };
        let model = FrameModel {
            panel: PanelData {
                branch: "feat".into(),
                files: vec![
                    DiffFile {
                        status: 'M',
                        path: "src/main.rs".into(),
                        added: 3,
                        deleted: 1,
                    },
                    DiffFile {
                        status: 'A',
                        path: "src/new.rs".into(),
                        added: 9,
                        deleted: 0,
                    },
                ],
                ..Default::default()
            },
            panel_focused: true,
            ..Default::default()
        };
        let mut s = Surface::new(44, 12);
        draw_panel(&mut s, rect, &model, &PanelUi::default());
        let text = s.screen_chars_to_string();
        assert!(text.contains("DIFF"), "tab bar: {text:?}");
        assert!(text.contains("PR"));
        assert!(text.contains("CHECKS"));
        assert!(text.contains("src/main.rs"), "file list: {text:?}");
        assert!(text.contains("src/new.rs"));
        // Help bar hint for the default Diff:FileList view.
        assert!(text.contains("open"), "help bar: {text:?}");
    }

    #[test]
    fn panel_checks_tab_lists_check_states() {
        use crate::panel::{CheckLine, CheckState, PanelData, PanelTab, PanelUi};
        let rect = Rect {
            x: 0,
            y: 0,
            cols: 44,
            rows: 12,
        };
        let model = FrameModel {
            panel: PanelData {
                checks: vec![
                    CheckLine {
                        name: "build".into(),
                        state: CheckState::Pass,
                    },
                    CheckLine {
                        name: "test".into(),
                        state: CheckState::Fail,
                    },
                ],
                ..Default::default()
            },
            ..Default::default()
        };
        let ui = PanelUi {
            tab: PanelTab::Checks,
            ..Default::default()
        };
        let mut s = Surface::new(44, 12);
        draw_panel(&mut s, rect, &model, &ui);
        let text = s.screen_chars_to_string();
        assert!(text.contains("build"), "checks: {text:?}");
        assert!(text.contains("test"));
        assert!(text.contains("rerun"), "checks help bar: {text:?}");
    }
}
