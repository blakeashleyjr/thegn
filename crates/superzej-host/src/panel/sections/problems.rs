//! Problems section — compiler/linter/test diagnostics collected from task output.
//!
//! Three view widths:
//!   Normal (39 cols): severity glyph + truncated message
//!   Half   (75 cols): + file:line on the right
//!   Full   (150 cols): left list + right detail (full message, source, code)

use crate::seg::{Line, Seg, seg, sp};

use super::{
    PanelHit, PanelRow, Section, SectionCtx, d, g, g2, g3, hint_row, hue, rule, t, two_col,
};
use crate::panel::Severity;
use superzej_core::theme::Hue;

fn severity_glyph(s: Severity) -> (&'static str, Tok) {
    use crate::seg::Tok;
    match s {
        Severity::Error => ("✗", hue(Hue::Red)),
        Severity::Warning => ("⚠", hue(Hue::Amber)),
        Severity::Info => ("·", hue(Hue::Blue)),
        Severity::Hint => ("→", g2()),
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{}…", cut.trim_end())
}

// ── main entry point ──────────────────────────────────────────────────────────

pub fn content(ctx: &SectionCtx) -> Vec<PanelRow> {
    let diags = &ctx.model.panel.diagnostics;
    if diags.is_empty() {
        return empty_view(ctx);
    }
    if ctx.full() {
        full_view(ctx)
    } else if ctx.deep() {
        half_view(ctx)
    } else {
        normal_view(ctx)
    }
}

fn empty_view(ctx: &SectionCtx) -> Vec<PanelRow> {
    if ctx.full() {
        // Full: structured two-column empty state with breadcrumb + explanation.
        vec![
            PanelRow::plain(Line::segs(vec![seg(d(), "PROBLEMS")])),
            rule(),
            PanelRow::plain(Line::segs(vec![seg(g2(), "no diagnostics collected yet")])),
            PanelRow::plain(Line::segs(vec![seg(
                g3(),
                "run a task in the Jobs section — errors and warnings",
            )])),
            PanelRow::plain(Line::segs(vec![seg(
                g3(),
                "from compiler/linter output will appear here",
            )])),
            rule(),
            hint_row(&[("↵", "open"), ("j/k", "select")]),
        ]
    } else if ctx.deep() {
        // Half: two rows of explanation.
        vec![
            PanelRow::plain(Line::split(
                vec![seg(g2(), "no diagnostics")],
                vec![seg(g3(), "run a task to collect")],
            )),
            hint_row(&[("↵", "open"), ("j/k", "select")]),
        ]
    } else {
        vec![
            PanelRow::plain(Line::segs(vec![seg(g2(), "no diagnostics")])),
            PanelRow::plain(Line::segs(vec![seg(g3(), "run a task to collect")])),
            hint_row(&[("↵", "open"), ("j/k", "select")]),
        ]
    }
}

// ── Normal view (39 cols) ─────────────────────────────────────────────────────

fn normal_view(ctx: &SectionCtx) -> Vec<PanelRow> {
    let diags = &ctx.model.panel.diagnostics;
    let cursor = ctx.ui.problems_cursor;
    let mut rows = Vec::new();

    for (i, d_item) in diags.iter().enumerate() {
        let (glyph, glyph_col) = severity_glyph(d_item.severity);
        let msg_w = ctx.cols.saturating_sub(glyph.len() + 2);
        let msg = truncate(&d_item.message, msg_w);
        let selected = i == cursor;

        let row_segs = vec![
            seg(glyph_col, glyph),
            seg(g3(), " "),
            seg(if selected { t() } else { d() }, msg),
        ];

        let row =
            PanelRow::plain(Line::segs(row_segs)).with_hit(PanelHit::Row(Section::Problems, i));
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

    rows.push(hint_row(&[("↵", "open"), ("j/k", "select")]));
    rows
}

// ── Half view (75 cols) ───────────────────────────────────────────────────────

fn half_view(ctx: &SectionCtx) -> Vec<PanelRow> {
    let diags = &ctx.model.panel.diagnostics;
    let cursor = ctx.ui.problems_cursor;
    let mut rows = Vec::new();

    for (i, d_item) in diags.iter().enumerate() {
        let (glyph, glyph_col) = severity_glyph(d_item.severity);
        let location = format!(
            "{}:{}",
            d_item.file.rsplit('/').next().unwrap_or(&d_item.file),
            d_item.line
        );
        let msg_w = ctx
            .cols
            .saturating_sub(glyph.len() + 2 + location.len() + 2);
        let msg = truncate(&d_item.message, msg_w);
        let selected = i == cursor;

        let row = PanelRow::plain(Line::split(
            vec![
                seg(glyph_col, glyph),
                seg(g3(), " "),
                seg(if selected { t() } else { d() }, msg),
            ],
            vec![seg(g2(), location)],
        ))
        .with_hit(PanelHit::Row(Section::Problems, i));
        let row = if selected {
            row.with_bg(crate::seg::Tok::SelAccent)
        } else {
            row
        };
        rows.push(row);

        if rows.len() + 1 > ctx.rows.saturating_sub(2) {
            break;
        }
    }

    rows.push(hint_row(&[("↵", "open"), ("j/k", "select")]));
    rows
}

// ── Full view (150 cols) ──────────────────────────────────────────────────────

fn full_view(ctx: &SectionCtx) -> Vec<PanelRow> {
    let diags = &ctx.model.panel.diagnostics;
    let cols = ctx.cols;
    let cursor = ctx.ui.problems_cursor;
    let mut rows = Vec::new();

    // Header summary
    let errors = diags
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .count();
    let warnings = diags
        .iter()
        .filter(|d| d.severity == Severity::Warning)
        .count();
    let mut header = vec![seg(d(), "PROBLEMS")];
    if errors > 0 {
        header.push(seg(g2(), "  "));
        header.push(seg(hue(Hue::Red), format!("✗ {errors}")));
    }
    if warnings > 0 {
        header.push(seg(g2(), "  "));
        header.push(seg(hue(Hue::Amber), format!("⚠ {warnings}")));
    }
    rows.push(PanelRow::plain(Line::segs(header)));
    rows.push(rule());

    let list_w = 40_usize.min(cols / 2);

    let list_rows: Vec<Vec<Seg>> = diags
        .iter()
        .enumerate()
        .map(|(i, d_item)| {
            let (glyph, glyph_col) = severity_glyph(d_item.severity);
            let sel = if i == cursor { "▶ " } else { "  " };
            let name_w = list_w.saturating_sub(sel.len() + glyph.len() + 1);
            let label = truncate(&d_item.message, name_w);
            vec![
                seg(if i == cursor { t() } else { g() }, sel),
                seg(glyph_col, glyph),
                seg(g3(), " "),
                seg(if i == cursor { t() } else { d() }, label),
            ]
        })
        .collect();

    let detail_rows = if let Some(d_item) = diags.get(cursor) {
        diag_detail_segs(d_item, cols.saturating_sub(list_w + 2))
    } else {
        vec![vec![seg(g2(), "select a diagnostic")]]
    };

    let combined = two_col(&list_rows, &detail_rows, list_w, 2);
    rows.extend(
        combined
            .into_iter()
            .enumerate()
            .map(|(i, l)| PanelRow::plain(l).with_hit(PanelHit::Row(Section::Problems, i))),
    );

    rows.push(rule());
    rows.push(hint_row(&[("↵", "open"), ("j/k", "select")]));
    rows
}

fn diag_detail_segs(d_item: &crate::panel::DiagnosticItem, w: usize) -> Vec<Vec<Seg>> {
    let mut out: Vec<Vec<Seg>> = Vec::new();
    let (glyph, glyph_col) = severity_glyph(d_item.severity);

    // Severity + source
    out.push(vec![
        seg(glyph_col, glyph),
        seg(g(), "  "),
        seg(t(), &d_item.source).bold(),
    ]);

    // Full message (may wrap across lines)
    for (i, chunk) in d_item
        .message
        .chars()
        .collect::<Vec<_>>()
        .chunks(w.max(1))
        .enumerate()
    {
        let s: String = chunk.iter().collect();
        if i == 0 {
            out.push(vec![seg(t(), s)]);
        } else {
            out.push(vec![sp(2), seg(d(), s)]);
        }
    }

    // File:line[:col]
    let loc = match d_item.col {
        Some(c) => format!("{}:{}:{}", d_item.file, d_item.line, c),
        None => format!("{}:{}", d_item.file, d_item.line),
    };
    out.push(vec![
        seg(g2(), "at  "),
        seg(g(), truncate(&loc, w.saturating_sub(4))),
    ]);

    // Code (if present)
    if let Some(code) = &d_item.code {
        out.push(vec![seg(g2(), "code "), seg(g(), code.clone())]);
    }

    out
}

use crate::seg::Tok;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chrome::FrameModel;
    use crate::layout::PanelWidth;
    use crate::panel::{DiagnosticItem, PanelData, PanelUi, Severity};

    fn make_ctx<'a>(
        model: &'a FrameModel,
        ui: &'a PanelUi,
        cols: usize,
        rows: usize,
    ) -> SectionCtx<'a> {
        SectionCtx {
            model,
            ui,
            cols,
            rows,
        }
    }

    fn make_diag(sev: Severity, msg: &str, file: &str) -> DiagnosticItem {
        DiagnosticItem {
            file: file.to_string(),
            line: 42,
            col: Some(5),
            severity: sev,
            message: msg.to_string(),
            source: "cargo".to_string(),
            code: None,
        }
    }

    #[test]
    fn empty_produces_three_rows() {
        let model = FrameModel::default();
        let ui = PanelUi::default();
        let ctx = make_ctx(&model, &ui, 40, 20);
        let rows = content(&ctx);
        assert_eq!(rows.len(), 3, "empty: {rows:?}");
    }

    #[test]
    fn normal_view_renders_diagnostics() {
        let mut model = FrameModel::default();
        model.panel.diagnostics = vec![
            make_diag(Severity::Error, "unused variable `x`", "src/main.rs"),
            make_diag(Severity::Warning, "deprecated function", "src/lib.rs"),
        ];
        let ui = PanelUi::default();
        let ctx = make_ctx(&model, &ui, 40, 20);
        let rows = content(&ctx);
        // 2 diag rows + 1 hint
        assert!(rows.len() >= 3, "got {}", rows.len());
    }

    #[test]
    fn selected_row_has_sel_accent_bg() {
        let mut model = FrameModel::default();
        model.panel.diagnostics = vec![
            make_diag(Severity::Error, "err", "a.rs"),
            make_diag(Severity::Warning, "warn", "b.rs"),
        ];
        let mut ui = PanelUi::default();
        ui.problems_cursor = 1;
        let ctx = make_ctx(&model, &ui, 40, 20);
        let rows = content(&ctx);
        // Row at index 1 (cursor=1) should have SelAccent background.
        assert!(
            rows[1].bg == Some(crate::seg::Tok::SelAccent),
            "row 1 bg: {:?}",
            rows[1].bg
        );
    }
}
