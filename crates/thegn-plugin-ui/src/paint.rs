use super::*;
use tg_kit::Tok;
use tg_kit::ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Widget as RenderWidget, Wrap},
};
use thegn_core::plugin_api::StyleRole;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

pub(super) fn paint(ui: &UiEngine, buf: &mut Buffer) {
    let widgets = ui.widgets();
    let focused = widgets.get(ui.focus).map(|w| w.id());
    if ui.view.is_none() {
        Paragraph::new(format!("Waiting for plugin{}", ui.glyphs.ellipsis)).render(ui.area, buf);
        return;
    }
    for w in &widgets {
        let Some(area) = ui.layout.get(w.id()).copied() else {
            continue;
        };
        if area.width == 0 || area.height == 0 {
            continue;
        }
        let focus = focused == Some(w.id());
        let sk = ui.selection_key(w.id());
        let selection = ui.selection.get(&sk);
        if let Widget::Grid {
            width,
            height,
            cells,
            ..
        } = w
        {
            for c in cells {
                let glyph_width = UnicodeWidthStr::width(c.span.text.as_str()) as u16;
                if c.x + glyph_width <= area.width.min(*width) && c.y < area.height.min(*height) {
                    buf.set_stringn(
                        area.x + c.x,
                        area.y + c.y,
                        &c.span.text,
                        glyph_width as usize,
                        role(ui, c.span.role),
                    );
                }
            }
            continue;
        }
        let title = match w {
            Widget::Text { title, .. }
            | Widget::Form { title, .. }
            | Widget::List { title, .. }
            | Widget::Table { title, .. }
            | Widget::Tree { title, .. }
            | Widget::Presentation { title, .. } => title.as_str(),
            Widget::Progress { label, .. } => label.as_str(),
            Widget::Confirm { .. } => "Confirm",
            _ => "Actions",
        };
        let border = ui.theme.fg(if focus { Tok::Accent } else { Tok::Border });
        let block = Block::default()
            .borders(Borders::ALL)
            .title(title.to_string())
            .border_style(border);
        let inner = block.inner(area);
        block.render(area, buf);
        let mut lines: Vec<Line> = Vec::new();
        let mut offset = 0;
        match w {
            Widget::Text { id, text, .. } => {
                lines = text.lines().map(|l| Line::raw(l.to_string())).collect();
                offset = *ui.scroll.get(id).unwrap_or(&0);
            }
            Widget::Form {
                id,
                instance,
                fields,
                submit,
                ..
            } => {
                if let Some(state) = ui.forms.get(&ui.key(id, instance)) {
                    let mut y = 0u16;
                    for (i, f) in fields.iter().enumerate() {
                        let Some(editor) = state.fields.get(&f.id) else {
                            continue;
                        };
                        let selected = focus && i == state.field;
                        let prefix = if selected { "> " } else { "  " };
                        let marker = if f.required { "*" } else { "" };
                        let base = ui.theme.fg(if f.disabled {
                            Tok::Faint
                        } else if f.read_only {
                            Tok::Dim
                        } else {
                            Tok::Text
                        });
                        let label = format!("{prefix}{}{marker}: ", f.label);
                        if f.multiline {
                            draw_line(buf, inner, y, state.scroll, &Line::styled(label, base));
                            y += 1;
                            for (line_index, line) in editor
                                .text()
                                .lines()
                                .chain(std::iter::once(""))
                                .enumerate()
                                .skip(editor.scroll_line)
                                .take(3)
                            {
                                draw_line(
                                    buf,
                                    inner,
                                    y,
                                    state.scroll,
                                    &Line::styled(
                                        format!(
                                            "  {}",
                                            clip_columns(
                                                line,
                                                editor.scroll_column,
                                                inner.width.saturating_sub(2) as usize
                                            )
                                        ),
                                        base,
                                    ),
                                );
                                if selected && y >= state.scroll && y - state.scroll < inner.height
                                {
                                    draw_editor_selection(
                                        buf,
                                        inner.x + 2,
                                        inner.y + y - state.scroll,
                                        editor,
                                        line_index,
                                        inner.right(),
                                        ui,
                                    );
                                }
                                y += 1;
                            }
                            // Exactly three editing rows even for an empty final line.
                            let row_height = field_height(f)
                                - u16::from(!f.helper.is_empty())
                                - u16::from(!f.error.is_empty());
                            let expected = fields[..i]
                                .iter()
                                .map(|f| effective_field_height(f, state))
                                .sum::<u16>()
                                + row_height;
                            while y < expected {
                                y += 1;
                            }
                        } else {
                            let value = if !f.choices.is_empty() || f.kind == FieldKind::Choice {
                                f.choices
                                    .iter()
                                    .find(|c| c.value == editor.text())
                                    .map(|c| format!("◀ {} ▶", c.label))
                                    .unwrap_or_else(|| format!("[unavailable: {}]", editor.text()))
                            } else if f.kind == FieldKind::Toggle {
                                format!("[{}]", if editor.text() == "true" { "x" } else { " " })
                            } else {
                                clip_columns(
                                    editor.text(),
                                    editor.scroll_column,
                                    inner.width.saturating_sub(field_label_width(f)) as usize,
                                )
                            };
                            draw_line(
                                buf,
                                inner,
                                y,
                                state.scroll,
                                &Line::styled(format!("{label}{value}"), base),
                            );
                            if selected && f.choices.is_empty() && f.kind != FieldKind::Toggle {
                                draw_editor_selection(
                                    buf,
                                    inner.x.saturating_add(field_label_width(f)),
                                    inner.y + y.saturating_sub(state.scroll),
                                    editor,
                                    0,
                                    inner.right(),
                                    ui,
                                );
                            }
                            y += 1;
                        }
                        if !f.helper.is_empty() {
                            draw_line(
                                buf,
                                inner,
                                y,
                                state.scroll,
                                &Line::styled(format!("  {}", f.helper), ui.theme.fg(Tok::Dim)),
                            );
                            y += 1;
                        }
                        let error = state
                            .errors
                            .get(&f.id)
                            .filter(|s| !s.is_empty())
                            .unwrap_or(&f.error);
                        if !error.is_empty() {
                            draw_line(
                                buf,
                                inner,
                                y,
                                state.scroll,
                                &Line::styled(format!("  {error}"), ui.theme.fg(Tok::Red)),
                            );
                            y += 1;
                        }
                    }
                    draw_line(
                        buf,
                        inner,
                        y,
                        state.scroll,
                        &Line::styled(
                            if state.pending {
                                format!("Saving{} (typing is preserved)", ui.glyphs.ellipsis)
                            } else {
                                format!("Ctrl+Enter: {submit} | Tab: next | Esc: cancel")
                            },
                            ui.theme.fg(Tok::Dim),
                        ),
                    );
                }
                continue;
            }
            Widget::Confirm { message, .. } => {
                lines.push(Line::raw(message.clone()));
                lines.push(Line::styled(
                    "Enter: confirm | Esc: cancel",
                    ui.theme.fg(Tok::Dim),
                ));
            }
            Widget::Progress {
                label,
                current,
                total,
                ..
            } => {
                let progress = match (current, total) {
                    (Some(c), Some(t)) => format!("{label}: {c}/{t}"),
                    (Some(c), None) => format!("{label}: {c}"),
                    _ => format!("{label}{}", ui.glyphs.ellipsis),
                };
                lines.push(Line::styled(progress, ui.theme.fg(Tok::Accent)));
                for r in rows(w, selection) {
                    lines.push(Line::raw(format!("> {}", r.label)));
                }
            }
            Widget::Presentation { message, state, .. } => {
                let tok = match state {
                    PresentationState::Error => Tok::Red,
                    PresentationState::Offline | PresentationState::Stale => Tok::Amber,
                    PresentationState::Loading => Tok::Accent,
                    _ => Tok::Dim,
                };
                lines.push(Line::styled(
                    format!("{state:?}: {message}"),
                    ui.theme.fg(tok),
                ));
                for r in rows(w, selection) {
                    lines.push(Line::raw(format!("> {}", r.label)));
                }
            }
            _ => {
                let rows = rows(w, selection);
                let current = selection.and_then(|s| s.current.as_ref());
                let selected = rows
                    .iter()
                    .position(|r| Some(&r.id) == current)
                    .unwrap_or(0);
                let header = usize::from(matches!(w, Widget::Table { .. }));
                let available = (inner.height as usize).saturating_sub(header);
                let start = selected.saturating_sub(available.saturating_sub(1));
                if let Widget::Table { columns, .. } = w {
                    let labels = columns
                        .iter()
                        .enumerate()
                        .map(|(i, c)| {
                            format!(
                                "{}{}",
                                if selection.is_some_and(|s| s.column == i) {
                                    ui.glyphs.caret_open
                                } else {
                                    " "
                                },
                                c.label
                            )
                        })
                        .collect::<Vec<_>>();
                    let label = table_text(
                        columns,
                        &labels,
                        inner.width.saturating_sub(2) as usize,
                        selection.map_or(0, |s| s.hscroll as usize),
                    );
                    lines.push(Line::styled(
                        format!("  {label}"),
                        ui.theme.fg(Tok::Accent).add_modifier(Modifier::BOLD),
                    ));
                }
                for (i, r) in rows.iter().enumerate().skip(start).take(available) {
                    let marker = if selection.is_some_and(|s| s.marked.contains(&r.id)) {
                        ui.glyphs.check
                    } else if i == selected {
                        ">"
                    } else {
                        " "
                    };
                    let tree = if let Widget::Tree { items, .. } = w {
                        if items.iter().any(|i| i.parent.as_deref() == Some(&r.id)) {
                            if selection.is_some_and(|s| s.expanded.contains(&r.id)) {
                                format!("{} ", ui.glyphs.caret_open)
                            } else {
                                format!("{} ", ui.glyphs.caret_closed)
                            }
                        } else {
                            String::from("  ")
                        }
                    } else {
                        String::new()
                    };
                    let text = if let Widget::Table { columns, rows, .. } = w {
                        let cells = rows
                            .iter()
                            .find(|row| row.id == r.id)
                            .map(|row| row.cells.as_slice())
                            .unwrap_or(&[]);
                        format!(
                            "{marker} {}",
                            table_text(
                                columns,
                                cells,
                                inner.width.saturating_sub(2) as usize,
                                selection.map_or(0, |s| s.hscroll as usize)
                            )
                        )
                    } else {
                        format!("{marker} {}{tree}{}", "  ".repeat(r.depth.min(12)), r.label)
                    };
                    lines.push(Line::styled(
                        clip_columns(
                            &text,
                            if matches!(w, Widget::Table { .. }) {
                                0
                            } else {
                                selection.map_or(0, |s| s.hscroll as usize)
                            },
                            inner.width as usize,
                        ),
                        ui.theme.fg(if r.disabled {
                            Tok::Faint
                        } else if i == selected && focus {
                            Tok::Accent
                        } else {
                            Tok::Text
                        }),
                    ));
                }
            }
        }
        Paragraph::new(lines)
            .scroll((offset, 0))
            .wrap(Wrap { trim: false })
            .render(inner, buf);
        if selection.is_some_and(|s| s.context) {
            let s = selection.expect("context");
            let actions = actions(w);
            let height = (actions.len() as u16 + 2).min(area.height);
            let context = Rect::new(area.x, area.bottom() - height, area.width, height);
            let block = Block::default()
                .borders(Borders::ALL)
                .title("Context actions")
                .border_style(ui.theme.fg(Tok::Accent));
            let cinner = block.inner(context);
            clear(context, buf);
            block.render(context, buf);
            let lines: Vec<_> = actions
                .iter()
                .enumerate()
                .map(|(i, a)| {
                    Line::styled(
                        format!(
                            "{}{}{}",
                            if i == s.action { "> " } else { "  " },
                            a.label,
                            if a.disabled {
                                format!(" ({})", a.disabled_reason)
                            } else {
                                String::new()
                            }
                        ),
                        ui.theme.fg(if a.disabled { Tok::Dim } else { Tok::Text }),
                    )
                })
                .collect();
            Paragraph::new(lines).render(cinner, buf);
        }
    }
    if let Some(popup) = &ui.popup
        && let Some(area) = ui.layout.get(&popup.widget).copied()
    {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(format!(
                "Choose {} type to search {} Esc closes",
                ui.glyphs.middot, ui.glyphs.middot
            ))
            .border_style(ui.theme.fg(Tok::Accent));
        let inner = block.inner(area);
        clear(area, buf);
        block.render(area, buf);
        let mut lines = vec![Line::raw(format!(
            "Search: {}",
            clip_columns(
                popup.query.text(),
                popup.query.scroll_column,
                inner.width.saturating_sub(8) as usize
            )
        ))];
        if let Some(Widget::Form { fields, .. }) = widgets.iter().find(|w| w.id() == popup.widget)
            && let Some(f) = fields.iter().find(|f| f.id == popup.field)
        {
            let query = popup.query.text().to_lowercase();
            let options: Vec<_> = f
                .choices
                .iter()
                .filter(|c| c.label.to_lowercase().contains(&query))
                .collect();
            let start = popup
                .index
                .saturating_sub(inner.height.saturating_sub(2) as usize);
            for (i, c) in options
                .iter()
                .enumerate()
                .skip(start)
                .take(inner.height.saturating_sub(1) as usize)
            {
                lines.push(Line::styled(
                    format!(
                        "{}{}{}",
                        if i == popup.index { "> " } else { "  " },
                        c.label,
                        if c.disabled { " (unavailable)" } else { "" }
                    ),
                    ui.theme.fg(if c.disabled { Tok::Dim } else { Tok::Text }),
                ));
            }
        }
        Paragraph::new(lines).render(inner, buf);
        if inner.width > 8 && inner.height > 0 {
            draw_editor_selection(
                buf,
                inner.x + 8,
                inner.y,
                &popup.query,
                0,
                inner.right(),
                ui,
            );
        }
    }
    if ui.recovery_open {
        let area = ui.area;
        clear(area, buf);
        let block = Block::default()
            .borders(Borders::ALL)
            .title(
                format!(
                    "Draft recovery {} </{}: records {} {}/{}: scroll {} Ctrl+Delete: discard {} Esc: close",
                    ui.glyphs.middot, ui.glyphs.arrow_right, ui.glyphs.middot,
                    ui.glyphs.arrow_up, ui.glyphs.arrow_down, ui.glyphs.middot,
                    ui.glyphs.middot,
                ),
            )
            .border_style(ui.theme.fg(Tok::Amber));
        let inner = block.inner(area);
        block.render(area, buf);
        let records = ui.recoveries();
        let mut lines = Vec::new();
        if let Some((scope, widget, instance)) = records.get(ui.recovery_index) {
            lines.push(Line::styled(
                format!(
                    "Record {}/{} {} scope: {scope} {} form: {widget} {} instance: {instance}",
                    ui.recovery_index + 1,
                    records.len(),
                    ui.glyphs.middot,
                    ui.glyphs.middot,
                    ui.glyphs.middot,
                ),
                ui.theme.fg(Tok::Amber),
            ));
            if let Some(values) = ui.recovery_values(scope, widget, instance) {
                for (id, value) in values {
                    lines.push(Line::styled(format!("{id}:"), ui.theme.fg(Tok::Accent)));
                    lines.extend(
                        value
                            .lines()
                            .chain(std::iter::once(""))
                            .map(|line| Line::raw(line.to_string())),
                    );
                }
            }
        } else {
            lines.push(Line::raw(
                "No removed unsaved forms. Pending requests remain visible in the status line.",
            ));
        }
        Paragraph::new(lines)
            .scroll((ui.recovery_scroll, 0))
            .wrap(Wrap { trim: false })
            .render(inner, buf);
    }
}
fn draw_line(buf: &mut Buffer, area: Rect, y: u16, scroll: u16, line: &Line) {
    if y >= scroll && y - scroll < area.height {
        line.clone()
            .render(Rect::new(area.x, area.y + y - scroll, area.width, 1), buf);
    }
}
fn clear(area: Rect, buf: &mut Buffer) {
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            if let Some(c) = buf.cell_mut((x, y)) {
                c.reset();
            }
        }
    }
}
fn role(ui: &UiEngine, r: StyleRole) -> Style {
    ui.theme.fg(match r {
        StyleRole::Accent => Tok::Accent,
        StyleRole::Error => Tok::Red,
        StyleRole::Warning => Tok::Amber,
        StyleRole::Faint => Tok::Dim,
        _ => Tok::Text,
    })
}
fn clip_columns(s: &str, skip: usize, width: usize) -> String {
    let mut col = 0;
    let mut out = String::new();
    for g in s.graphemes(true) {
        let w = if g == "\t" {
            4 - col % 4
        } else {
            UnicodeWidthStr::width(g)
        };
        if col >= skip && col + w <= skip + width {
            if g == "\t" {
                out.push_str(&" ".repeat(w));
            } else {
                out.push_str(g);
            }
        }
        col += w;
        if col >= skip + width {
            break;
        }
    }
    out
}

fn draw_editor_selection(
    buf: &mut Buffer,
    x: u16,
    y: u16,
    e: &Editor,
    line: usize,
    right: u16,
    ui: &UiEngine,
) {
    let Some(selection) = e.selection() else {
        return;
    };
    let mut line_index = 0;
    let mut col = 0usize;
    for (byte, g) in e.text().grapheme_indices(true) {
        if g == "\n" {
            line_index += 1;
            col = 0;
            continue;
        }
        let width = if g == "\t" {
            4 - col % 4
        } else {
            UnicodeWidthStr::width(g)
        };
        if line_index == line
            && byte >= selection.start
            && byte < selection.end
            && col >= e.scroll_column
        {
            let cx = x.saturating_add((col - e.scroll_column).min(u16::MAX as usize) as u16);
            if cx.saturating_add(width as u16) <= right && buf.area.contains((cx, y).into()) {
                Span::styled(
                    if g == "\t" {
                        " ".repeat(width)
                    } else {
                        g.to_string()
                    },
                    ui.theme.fg(Tok::Bg0).bg(ui.theme.color(Tok::Accent)),
                )
                .render(Rect::new(cx, y, width as u16, 1), buf);
            }
        }
        col += width;
    }
}

/// Format only the visible column window; huge declared widths never allocate huge pads.
fn table_text(columns: &[TableColumn], cells: &[String], width: usize, skip: usize) -> String {
    let fixed: usize = columns.iter().map(|c| c.width.unwrap_or(0) as usize).sum();
    let auto = columns.iter().filter(|c| c.width.is_none()).count();
    let share = width.saturating_sub(fixed + columns.len().saturating_sub(1) * 2) / auto.max(1);
    let mut at = 0usize;
    let mut out = String::new();
    for (i, column) in columns.iter().enumerate() {
        let extent = column.width.map_or(share.max(6), usize::from);
        let start = at.max(skip);
        let end = (at + extent).min(skip + width);
        if start < end {
            let text = cells.get(i).map(String::as_str).unwrap_or("");
            let clipped = clip_columns(text, start - at, end - start);
            let used = UnicodeWidthStr::width(clipped.as_str());
            out.push_str(&clipped);
            out.push_str(&" ".repeat((end - start).saturating_sub(used)));
        }
        at += extent;
        if i + 1 < columns.len() {
            let start = at.max(skip);
            let end = (at + 2).min(skip + width);
            if start < end {
                out.push_str(&" ".repeat(end - start));
            }
            at += 2;
        }
        if at >= skip + width {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn columns_have_actual_widths_unicode_clipping_and_bounded_padding() {
        let cols = vec![
            TableColumn {
                id: "a".into(),
                label: "A".into(),
                width: Some(4),
                sortable: false,
            },
            TableColumn {
                id: "b".into(),
                label: "B".into(),
                width: Some(u16::MAX),
                sortable: false,
            },
        ];
        let cells = vec!["界界x".into(), "second".into()];
        assert_eq!(table_text(&cols, &cells, 12, 0), "界界  second");
        let far = table_text(&cols, &cells, 12, 60000);
        assert_eq!(far.len(), 12);
        assert!(far.chars().all(|c| c == ' '));
    }
}
