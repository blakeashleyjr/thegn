//! Symbols section — a document-symbol outline for the selected file.
//!
//! Populated from the language server when one is available, falling back to the
//! tree-sitter entity parser otherwise (see `run.rs`'s outline fetch). Selecting
//! a row navigates to the symbol's definition via the `open-file:` primitive.
//!
//! Widths:
//!   Normal (39): kind glyph + name (indented by nesting depth)
//!   Half   (75): + file:line on the right
//!   Full  (150): breadcrumb header + the same list

use crate::seg::{Line, seg};

use super::{PanelHit, PanelRow, Section, SectionCtx, d, g2, g3, hint_row, rule, t};

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{}…", cut.trim_end())
}

pub fn content(ctx: &SectionCtx) -> Vec<PanelRow> {
    let data = &ctx.model.panel;
    if data.symbols.is_empty() {
        return empty_view(ctx);
    }
    if ctx.full() {
        full_view(ctx)
    } else {
        list_view(ctx, ctx.deep())
    }
}

fn empty_view(ctx: &SectionCtx) -> Vec<PanelRow> {
    let file = &ctx.model.panel.symbols_file;
    let msg = if file.is_empty() {
        "select a file to outline"
    } else {
        "no symbols in this file"
    };
    let mut rows = vec![PanelRow::plain(Line::segs(vec![seg(g2(), msg)]))];
    if ctx.full() && !file.is_empty() {
        rows.push(PanelRow::plain(Line::segs(vec![seg(g3(), file.clone())])));
    }
    rows.push(hint_row(&[
        ("↵", "go to def"),
        ("r", "refs"),
        ("o", "outline"),
        ("j/k", "select"),
    ]));
    rows
}

fn list_view(ctx: &SectionCtx, half: bool) -> Vec<PanelRow> {
    let data = &ctx.model.panel;
    let cursor = ctx.ui.symbols_cursor;
    let mut rows = Vec::new();

    for (i, s) in data.symbols.iter().enumerate() {
        let selected = i == cursor;
        let indent = "  ".repeat(s.depth as usize);
        let name_col = if selected { t() } else { d() };
        let location = format!(
            "{}:{}",
            s.file.rsplit('/').next().unwrap_or(&s.file),
            s.line
        );

        let left = vec![
            seg(g3(), indent.clone()),
            seg(g2(), format!("{} ", s.kind)),
            seg(
                name_col,
                truncate(&s.name, ctx.cols.saturating_sub(indent.len() + 12)),
            ),
        ];
        let line = if half {
            Line::split(left, vec![seg(g2(), location)])
        } else {
            Line::segs(left)
        };

        let row = PanelRow::plain(line).with_hit(PanelHit::Row(Section::Symbols, i));
        let row = if selected {
            row.with_bg(crate::seg::Tok::SelAccent)
        } else {
            row
        };
        rows.push(row);

        if rows.len() >= ctx.rows.saturating_sub(1) {
            break;
        }
    }

    rows.push(hint_row(&[
        ("↵", "go to def"),
        ("r", "refs"),
        ("o", "outline"),
        ("j/k", "select"),
    ]));
    rows
}

fn full_view(ctx: &SectionCtx) -> Vec<PanelRow> {
    let file = &ctx.model.panel.symbols_file;
    let mut rows = vec![
        PanelRow::plain(Line::segs(vec![seg(d(), "OUTLINE")])),
        PanelRow::plain(Line::segs(vec![seg(g3(), file.clone())])),
        rule(),
    ];
    rows.extend(list_view(ctx, true));
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chrome::FrameModel;
    use crate::panel::{PanelUi, SymbolRow};

    fn make_ctx<'a>(model: &'a FrameModel, ui: &'a PanelUi) -> SectionCtx<'a> {
        SectionCtx {
            model,
            ui,
            cols: 40,
            rows: 20,
        }
    }

    fn sym(kind: &str, name: &str, line: u64, depth: u16) -> SymbolRow {
        SymbolRow {
            kind: kind.into(),
            name: name.into(),
            file: "src/a.rs".into(),
            line,
            col: 0,
            depth,
        }
    }

    #[test]
    fn empty_outline_prompts_for_a_file() {
        let model = FrameModel::default();
        let ui = PanelUi::default();
        let rows = content(&make_ctx(&model, &ui));
        // Prompt row + hint row, and no row hits into the Symbols section.
        assert!(rows.iter().all(|r| r.hit.is_none()));
        assert!(rows.len() >= 2);
    }

    #[test]
    fn renders_symbol_rows_with_hits() {
        let mut model = FrameModel::default();
        model.panel.symbols = vec![sym("struct", "Foo", 1, 0), sym("fn", "bar", 3, 1)];
        model.panel.symbols_file = "src/a.rs".into();
        let ui = PanelUi::default();
        let rows = content(&make_ctx(&model, &ui));
        // 2 symbol rows (each a hit) + the hint row.
        let hits = rows
            .iter()
            .filter(|r| matches!(r.hit, Some(PanelHit::Row(Section::Symbols, _))))
            .count();
        assert_eq!(hits, 2);
    }

    #[test]
    fn selected_row_has_sel_accent_bg() {
        let mut model = FrameModel::default();
        model.panel.symbols = vec![sym("fn", "a", 1, 0), sym("fn", "b", 2, 0)];
        let ui = PanelUi {
            symbols_cursor: 1,
            ..Default::default()
        };
        let rows = content(&make_ctx(&model, &ui));
        assert_eq!(rows[1].bg, Some(crate::seg::Tok::SelAccent));
    }
}
