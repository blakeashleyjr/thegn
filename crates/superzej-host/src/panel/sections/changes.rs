//! The changes section: porcelain status rows with inline hunk previews
//! (Normal/Half), and the full-width side-by-side diff of the selected file
//! (Full — the former diff overlay).

use superzej_core::diff_sbs::{CellKind, SbsCell, SbsFile};
use superzej_core::theme::Hue;

use crate::panel::docs::{diff_hunk_at, diff_hunk_starts};
use crate::seg::{Line, Seg, seg, sp};

use super::{
    ChangeRow, PanelHit, PanelRow, PanelUi, Section, SectionCtx, Stage, d, diffstat, f, g, g2, g3,
    hint_row, hue, rule, spinner_row, split_bar, t,
};

pub(super) fn content(ctx: &SectionCtx) -> Vec<PanelRow> {
    if ctx.full() {
        return side_by_side(ctx);
    }
    list(ctx)
}

/// Normal/Half: the change-row list. Half widens the split bar and deepens
/// the inline hunk preview.
fn list(ctx: &SectionCtx) -> Vec<PanelRow> {
    let (data, ui, deep) = (&ctx.model.panel, ctx.ui, ctx.deep());
    let mut rows: Vec<PanelRow> = Vec::new();
    if data.changes.is_empty() {
        rows.push(PanelRow::plain(Line::segs(vec![seg(
            g(),
            "working tree clean",
        )])));
        return rows;
    }
    for (i, c) in data.changes.iter().enumerate() {
        let on = ui.chg_sel == Some(i);
        rows.push(change_row(c, i, on, deep));
        if on {
            rows.extend(hunk_preview(c, ui, deep));
            rows.push(PanelRow::blank());
        }
    }
    rows.push(PanelRow::blank());
    rows.push(if ui.chg_sel.is_none() {
        hint_row(&[("↵", "preview"), ("space", "stage")])
    } else {
        hint_row(&[("↵", "dismiss"), ("space", "stage")])
    });
    rows
}

fn change_row(c: &ChangeRow, i: usize, on: bool, deep: bool) -> PanelRow {
    let (glyph, glyph_tok) = match c.stage {
        Stage::Staged => ("●", hue(Hue::Green)),
        Stage::Conflict => ("!", hue(Hue::Red)),
        _ => ("○", g2()),
    };
    let status_tok = match c.stage {
        Stage::Staged => hue(Hue::Green),
        Stage::Conflict => hue(Hue::Red),
        Stage::Untracked => g(),
        Stage::Unstaged => hue(Hue::Amber),
    };
    let name = match (on, c.stage) {
        (true, _) => seg(t(), c.name.clone()).bold(),
        (false, Stage::Conflict) => {
            seg(hue(Hue::Red), c.name.clone()).under(crate::seg::Under::CurlyHue(Hue::Red))
        }
        _ => seg(d(), c.name.clone()),
    };
    let l = vec![
        seg(glyph_tok, glyph),
        sp(1),
        seg(status_tok, format!("{:<2}", c.status)).bold(),
        seg(g2(), c.dir.clone()),
        name,
    ];
    let r: Vec<Seg> = match c.stage {
        Stage::Conflict if !on => vec![seg(hue(Hue::Red), "resolve ").bold(), seg(g2(), "↵")],
        Stage::Untracked => vec![seg(g2(), "new")],
        _ => {
            let mut v = diffstat(c.added, c.deleted);
            v.push(sp(1));
            v.extend(split_bar(c.added, c.deleted, if deep { 8 } else { 5 }));
            v
        }
    };
    let mut row = PanelRow::plain(Line::split(l, r)).with_hit(PanelHit::Row(Section::Changes, i));
    if on {
        row = row.with_bg(crate::seg::Tok::SelAccent);
    }
    row
}

/// The inline hunk preview under a highlighted change row. The Half view
/// shows more hunks and more lines per hunk.
fn hunk_preview(c: &ChangeRow, ui: &PanelUi, deep: bool) -> Vec<PanelRow> {
    let (hunk_cap, line_cap) = if deep { (3, 12) } else { (2, 6) };
    let mut rows = Vec::new();
    if c.stage == Stage::Untracked {
        rows.push(PanelRow::plain(Line::segs(vec![
            sp(1),
            seg(g2(), "▾ "),
            seg(f(), "untracked"),
            seg(g(), " · not in index"),
        ])));
        return rows;
    }
    match ui.hunks.get(&c.path) {
        Some(hunks) if !hunks.is_empty() => {
            for h in hunks.iter().take(hunk_cap) {
                let header_tok = if c.stage == Stage::Conflict {
                    hue(Hue::Red)
                } else {
                    hue(Hue::Purple)
                };
                rows.push(PanelRow::plain(Line::split(
                    vec![
                        sp(1),
                        seg(g2(), "▾ "),
                        seg(header_tok, h.header.clone()),
                        seg(g(), format!(" {}", h.func)),
                    ],
                    if c.stage == Stage::Staged {
                        vec![seg(hue(Hue::Green), "● staged")]
                    } else {
                        vec![seg(g2(), "○")]
                    },
                )));
                for (origin, text) in h.lines.iter().take(line_cap) {
                    let tok = match origin {
                        '+' => hue(Hue::Green),
                        '-' => hue(Hue::Red),
                        _ => d(),
                    };
                    let mark = match origin {
                        '+' => "+ ",
                        '-' => "− ",
                        _ => "  ",
                    };
                    rows.push(PanelRow::plain(Line::segs(vec![
                        sp(2),
                        seg(tok, format!("{mark}{text}")),
                    ])));
                }
                if h.truncated || h.lines.len() > line_cap {
                    rows.push(PanelRow::plain(Line::segs(vec![sp(2), seg(g2(), "…")])));
                }
            }
        }
        _ => rows.push(PanelRow::plain(Line::segs(vec![
            sp(1),
            seg(g2(), "▾ loading hunks…"),
        ]))),
    }
    rows
}

// ---- Full: the side-by-side diff (the former diff overlay) -----------------

/// One side of a side-by-side row, exactly `w` cells: 4-char line number +
/// gutter + clipped text, changed cells tinted across their full width.
fn diff_cell(cell: Option<&SbsCell>, w: usize) -> Vec<Seg> {
    let Some(cell) = cell else {
        return vec![sp(w)];
    };
    let (fg, bg) = match cell.kind {
        CellKind::Context => (t(), None),
        CellKind::Removed => (hue(Hue::Red), Some(crate::seg::Tok::Sel(Hue::Red, 14))),
        CellKind::Added => (hue(Hue::Green), Some(crate::seg::Tok::Sel(Hue::Green, 14))),
    };
    let text_w = w.saturating_sub(5);
    let text: String = cell.text.chars().take(text_w).collect();
    let pad = text_w - text.chars().count();
    let mut no = seg(g3(), format!("{:>4} ", cell.line_no));
    let mut body = seg(fg, format!("{text}{}", " ".repeat(pad)));
    if let Some(bg) = bg {
        no = no.bg(bg);
        body = body.bg(bg);
    }
    vec![no, body]
}

/// The flattened line at index `at`: a hunk header or an aligned row pair.
fn diff_flat_line(file: &SbsFile, starts: &[usize], at: usize, side: usize) -> Line {
    let h = diff_hunk_at(starts, at);
    let Some(hunk) = file.hunks.get(h) else {
        return Line::Blank;
    };
    let off = at - starts[h];
    if off == 0 {
        let mut segs = vec![seg(
            f(),
            format!("@@ -{} +{} @@", hunk.old_start, hunk.new_start),
        )];
        if !hunk.func.is_empty() {
            segs.push(seg(g2(), format!(" {}", hunk.func)));
        }
        return Line::segs(segs);
    }
    let Some(row) = hunk.rows.get(off - 1) else {
        return Line::Blank;
    };
    let mut segs = diff_cell(row.old.as_ref(), side);
    segs.push(seg(g3(), "│"));
    segs.extend(diff_cell(row.new.as_ref(), side));
    Line::segs(segs)
}

fn side_by_side(ctx: &SectionCtx) -> Vec<PanelRow> {
    let footer = hint_row(&[
        ("[ ]", "hunk"),
        ("j/k", "scroll"),
        ("n/p", "file"),
        ("space", "stage"),
    ]);
    let Some(doc) = &ctx.ui.docs.diff else {
        return vec![
            spinner_row(ctx.ui.docs.tick, "diff"),
            PanelRow::blank(),
            footer,
        ];
    };
    if doc.file.hunks.is_empty() {
        let msg = if doc.path.is_empty() {
            "working tree clean".to_string()
        } else {
            format!("no unstaged changes in {}", doc.path)
        };
        return vec![
            PanelRow::plain(Line::segs(vec![seg(d(), msg)])),
            PanelRow::blank(),
            footer,
        ];
    }

    let starts = diff_hunk_starts(&doc.file);
    let len = crate::panel::docs::diff_flat_len(&doc.file);
    let hunk = ctx.ui.diff_hunk.min(starts.len() - 1);
    let side = (ctx.cols.saturating_sub(1)) / 2;
    let body = ctx.rows.saturating_sub(3); // header + rule + footer
    let scroll = ctx.ui.scroll.min(len.saturating_sub(1));

    let mut rows: Vec<PanelRow> = Vec::with_capacity(body + 3);
    rows.push(PanelRow::plain(Line::split(
        vec![seg(d(), doc.path.clone()).bold()],
        vec![
            seg(hue(Hue::Green), format!("+{}", doc.file.added)),
            seg(g(), " "),
            seg(hue(Hue::Red), format!("−{}", doc.file.deleted)),
            seg(g(), format!(" · hunk {}/{}", hunk + 1, starts.len())),
        ],
    )));
    rows.push(rule());
    for at in scroll..(scroll + body).min(len) {
        rows.push(PanelRow::plain(diff_flat_line(
            &doc.file, &starts, at, side,
        )));
    }
    rows.push(footer);
    rows
}
