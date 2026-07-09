//! The changes section: porcelain status rows with inline hunk previews
//! (Normal/Half), and the full-width side-by-side diff of the selected file
//! (Full — the former diff overlay).

use superzej_core::diff_sbs::{CellKind, SbsCell, SbsFile};
use superzej_core::theme::Hue;

use crate::panel::docs::{diff_hunk_at, diff_hunk_starts};
use crate::seg::{Line, Seg, Tok, seg, sp};

use crate::seg::seg_width;

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
        rows.push(change_row(c, i, on, deep, ctx.cols));
        if on {
            // Entities this file touches (item 311), above its hunk preview.
            if let Some((_, changes)) = data
                .entities
                .as_ref()
                .and_then(|e| e.per_file.iter().find(|(p, _)| *p == c.path))
            {
                let mut segs = vec![seg(hue(Hue::Purple), "  ▸ ")];
                for (n, ch) in changes.iter().take(6).enumerate() {
                    if n > 0 {
                        segs.push(seg(g2(), ", "));
                    }
                    segs.push(seg(g(), format!("{} {}", ch.kind.label(), ch.name)));
                }
                rows.push(PanelRow::plain(Line::segs(segs)));
            }
            rows.extend(hunk_preview(c, ui, deep, ctx.cols));
            rows.push(PanelRow::blank());
        }
    }
    // Semantic-impact footer (item 313): a labeled, selectable one-line entity
    // summary at the *bottom* of the file list (the last actionable row, hit
    // index `changes.len()`), expanding inline into a per-file / per-entity
    // breakdown when `impact_open`.
    if data
        .entities
        .as_ref()
        .and_then(|e| e.impact.as_ref())
        .is_some()
    {
        rows.push(PanelRow::blank());
        rows.extend(impact_footer(data, ui, ctx.cols));
    }
    rows.push(PanelRow::blank());
    rows.push(if ui.impact_open {
        // On the footer row Enter collapses; on an entity row it opens the def.
        hint_row(&[("↵", "open/close"), ("j/k", "entity")])
    } else if ui.chg_sel.is_none() {
        hint_row(&[("↵", "preview"), ("space", "stage")])
    } else {
        hint_row(&[("↵", "dismiss"), ("space", "stage")])
    });
    rows
}

/// The semantic-impact footer row plus, when expanded, its per-file / per-entity
/// breakdown. The collapsed line is rebuilt from the structured per-file entity
/// data (rather than the pre-baked `impact.summary` string) so it carries a clear
/// `semantic` label and clips cleanly to the panel width instead of mid-word.
fn impact_footer(data: &crate::panel::PanelData, ui: &PanelUi, cols: usize) -> Vec<PanelRow> {
    let Some(entities) = data.entities.as_ref() else {
        return Vec::new();
    };
    let mut rows: Vec<PanelRow> = Vec::new();
    let mut line = vec![seg(hue(Hue::Purple), "◈ "), seg(f(), "semantic")];
    line.extend(impact_counts_segs(&entities.per_file, cols));
    let mut footer = PanelRow::plain(Line::segs(line))
        .with_hit(PanelHit::Row(Section::Changes, data.changes.len()));
    if ui.impact_open {
        footer = footer.with_bg(crate::seg::Tok::SelAccent);
    }
    rows.push(footer);
    if !ui.impact_open {
        return rows;
    }
    // Expanded: a one-line legend, then each file with its touched entities.
    rows.push(PanelRow::plain(Line::segs(vec![
        sp(2),
        seg(f(), "entity-level changes touched by this diff"),
    ])));
    // Each rendered entity row is actionable: its hit index runs sequentially
    // past the footer (`changes.len()`), matching `EntitySummary::entity_targets`
    // one-for-one so the drill-in can map a cursor row back to its (file, line).
    let base = data.changes.len() + 1;
    let mut ordinal = 0usize;
    for (path, changes) in &entities.per_file {
        if changes.is_empty() {
            continue;
        }
        rows.push(PanelRow::plain(Line::segs(vec![
            sp(1),
            seg(f(), path.clone()),
        ])));
        for c in changes.iter().take(superzej_core::semantic::ENTITY_ROW_CAP) {
            let (glyph, tok) = match c.touch {
                superzej_core::semantic::Touch::Added => ("+", hue(Hue::Green)),
                superzej_core::semantic::Touch::Modified => ("~", hue(Hue::Amber)),
                superzej_core::semantic::Touch::Removed => ("−", hue(Hue::Red)),
            };
            let mut segs = vec![
                sp(2),
                seg(tok, format!("{glyph} ")),
                seg(f(), format!("{} ", c.kind.label())),
                seg(t(), c.name.clone()),
                sp(1),
            ];
            segs.extend(diffstat(c.added, c.deleted));
            // The row-mode cursor highlight is applied by the frame builder
            // (`cursor_tint`) for any row carrying `Row(open, cursor)` — just
            // give the row its hit so the cursor can land on and activate it.
            rows.push(
                PanelRow::plain(Line::segs(segs))
                    .with_hit(PanelHit::Row(Section::Changes, base + ordinal)),
            );
            ordinal += 1;
        }
        if changes.len() > superzej_core::semantic::ENTITY_ROW_CAP {
            rows.push(PanelRow::plain(Line::segs(vec![
                sp(2),
                seg(
                    f(),
                    format!(
                        "… +{} more",
                        changes.len() - superzej_core::semantic::ENTITY_ROW_CAP
                    ),
                ),
            ])));
        }
    }
    rows
}

/// The `· <kind counts> · N files` tail of the collapsed impact line, built from
/// the per-file entity churn. Kinds are ordered most-frequent first and dropped
/// (with a trailing "…") once they'd overflow `cols`, so the line never clips a
/// word mid-glyph.
fn impact_counts_segs(
    per_file: &[(String, Vec<superzej_core::semantic::EntityChange>)],
    cols: usize,
) -> Vec<Seg> {
    // Count entities by kind label, preserving churn order; then rank by count.
    let mut by_kind: Vec<(&'static str, usize)> = Vec::new();
    let mut files = 0usize;
    for (_, changes) in per_file {
        if changes.is_empty() {
            continue;
        }
        files += 1;
        for c in changes {
            let label = c.kind.label();
            match by_kind.iter_mut().find(|(k, _)| *k == label) {
                Some((_, n)) => *n += 1,
                None => by_kind.push((label, 1)),
            }
        }
    }
    by_kind.sort_by_key(|(_, n)| std::cmp::Reverse(*n));

    let files_tail = format!("{files} file{}", if files == 1 { "" } else { "s" });
    // Budget: total cols minus the "◈ semantic" prefix (width 10), the two " · "
    // separators (6), and the files tail — leaving room for the kind list.
    let budget = cols.saturating_sub(16 + files_tail.chars().count()).max(4);
    let mut kinds = String::new();
    let mut dropped = false;
    for (i, (label, n)) in by_kind.iter().enumerate() {
        let part = format!(
            "{}{n} {label}{}",
            if i > 0 { ", " } else { "" },
            if *n == 1 { "" } else { "s" }
        );
        if kinds.chars().count() + part.chars().count() <= budget {
            kinds.push_str(&part);
        } else {
            dropped = true;
            break;
        }
    }
    if dropped && kinds.chars().count() < budget {
        kinds.push('…');
    }

    let mut segs = Vec::new();
    if !kinds.is_empty() {
        segs.push(seg(g2(), " · "));
        segs.push(seg(f(), kinds));
    }
    segs.push(seg(g2(), " · "));
    segs.push(seg(f(), files_tail));
    segs
}

fn change_row(c: &ChangeRow, i: usize, on: bool, deep: bool, cols: usize) -> PanelRow {
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
    // Build right side first so its width is known before computing the path budget.
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
    // Prefix width: glyph(1) + sp(1) + status(2) = 4.
    // indent() adds sp(2) to l and sp(1) to r; draw_line adds 1-cell gap between l and r.
    // Net path budget = cols - 4(prefix) - r_w - 5(indent+gap overhead).
    let path_budget = cols.saturating_sub(4 + seg_width(&r) + 5);
    let dir_display = clip_dir_left(&c.dir, c.name.chars().count(), path_budget);
    let l = vec![
        seg(glyph_tok, glyph),
        sp(1),
        seg(status_tok, format!("{:<2}", c.status)).bold(),
        // The path prefix is a label, not scaffolding: `faint` clears a
        // readable contrast on the panel surface where `ghost2` (the structural
        // floor) read as grey-on-grey next to the brighter file name.
        seg(f(), dir_display),
        name,
    ];
    let mut row = PanelRow::plain(Line::split(l, r)).with_hit(PanelHit::Row(Section::Changes, i));
    if on {
        row = row.with_bg(crate::seg::Tok::SelAccent);
    }
    row
}

/// Left-clip `dir` so that `name_w` chars are guaranteed to fit within `budget`.
///
/// Returns the portion of `dir` to display. Clips from the left (preserving the
/// tail of the path nearest the filename), snapping to a `/` boundary so it
/// reads as `…parent/` rather than `…arent/`. Returns an empty string when the
/// budget is too tight to show any directory context.
fn clip_dir_left(dir: &str, name_w: usize, budget: usize) -> String {
    let dir_chars: Vec<char> = dir.chars().collect();
    let dir_w = dir_chars.len();
    if dir_w + name_w <= budget {
        return dir.to_string();
    }
    let dir_budget = budget.saturating_sub(name_w);
    if dir_budget <= 1 {
        return String::new();
    }
    // Reserve 1 char for the leading "…", the rest goes to the dir suffix.
    let take = dir_budget.saturating_sub(1);
    let from = dir_w.saturating_sub(take);
    // Advance to the next '/' so we start cleanly on a component boundary.
    let snap = dir_chars[from..]
        .iter()
        .position(|&c| c == '/')
        .map(|p| from + p + 1)
        .unwrap_or(from);
    if snap >= dir_w {
        return String::new();
    }
    format!("…{}", dir_chars[snap..].iter().collect::<String>())
}

/// The inline hunk preview under a highlighted change row. The Half view
/// shows more hunks and more lines per hunk.
fn hunk_preview(c: &ChangeRow, ui: &PanelUi, deep: bool, cols: usize) -> Vec<PanelRow> {
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
                    rows.extend(wrap_preview(tok, mark, text, cols));
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

/// Wrap one inline-preview line onto continuation rows instead of letting the
/// renderer clip it. The first row carries the `+ `/`− ` mark at the 2-cell
/// indent; continuations align under the text at a 4-cell indent.
fn wrap_preview(tok: crate::seg::Tok, mark: &str, text: &str, cols: usize) -> Vec<PanelRow> {
    let text_w = cols.saturating_sub(4).max(1);
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return vec![PanelRow::plain(Line::segs(vec![
            sp(2),
            seg(tok, mark.to_string()),
        ]))];
    }
    let mut rows = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let end = (i + text_w).min(chars.len());
        let chunk: String = chars[i..end].iter().collect();
        let segs = if i == 0 {
            vec![sp(2), seg(tok, format!("{mark}{chunk}"))]
        } else {
            vec![sp(4), seg(tok, chunk)]
        };
        rows.push(PanelRow::plain(Line::segs(segs)));
        i = end;
    }
    rows
}

// ---- Full: the side-by-side diff (the former diff overlay) -----------------

/// One side of a side-by-side row, each visual row exactly `w` cells: 4-char
/// line number + gutter + text, changed cells tinted across their full width.
///
/// Long text wraps onto continuation rows instead of being clipped: the first
/// row shows the line number, continuations blank the gutter. Returns one entry
/// per visual row (always at least one), so an absent cell yields a single
/// blank `w`-cell row.
fn diff_cell(cell: Option<&SbsCell>, w: usize, mask: Option<&[bool]>) -> Vec<Vec<Seg>> {
    let Some(cell) = cell else {
        return vec![vec![sp(w)]];
    };
    let (fg, hue_kind, bg) = match cell.kind {
        CellKind::Context => (t(), None, None),
        CellKind::Removed => (hue(Hue::Red), Some(Hue::Red), Some(Tok::Sel(Hue::Red, 14))),
        CellKind::Added => (
            hue(Hue::Green),
            Some(Hue::Green),
            Some(Tok::Sel(Hue::Green, 14)),
        ),
    };
    let text_w = w.saturating_sub(5).max(1);
    let chars: Vec<char> = cell.text.chars().collect();
    let mut out: Vec<Vec<Seg>> = Vec::new();
    let mut i = 0;
    let mut first = true;
    loop {
        let end = (i + text_w).min(chars.len());
        let pad = text_w - (end - i);
        let no_text = if first {
            format!("{:>4} ", cell.line_no)
        } else {
            "     ".to_string()
        };
        let mut no = seg(g3(), no_text);
        if let Some(bg) = bg {
            no = no.bg(bg);
        }
        let mut segs = vec![no];
        match (mask, bg, hue_kind) {
            // Word-level emphasis (item 601): split the wrapped chunk into runs
            // by the changed-mask; changed runs get a brighter tint + bold.
            (Some(mask), Some(base_bg), Some(kh)) => {
                let emph_bg = Tok::Sel(kh, 22);
                let mut j = i;
                while j < end {
                    let changed = mask.get(j).copied().unwrap_or(false);
                    let mut k = j + 1;
                    while k < end && mask.get(k).copied().unwrap_or(false) == changed {
                        k += 1;
                    }
                    let run: String = chars[j..k].iter().collect();
                    let mut s = seg(fg, run).bg(if changed { emph_bg } else { base_bg });
                    if changed {
                        s = s.bold();
                    }
                    segs.push(s);
                    j = k;
                }
                if pad > 0 {
                    segs.push(seg(fg, " ".repeat(pad)).bg(base_bg));
                }
            }
            _ => {
                let chunk: String = chars[i..end].iter().collect();
                let mut body = seg(fg, format!("{chunk}{}", " ".repeat(pad)));
                if let Some(bg) = bg {
                    body = body.bg(bg);
                }
                segs.push(body);
            }
        }
        out.push(segs);
        first = false;
        i = end;
        if i >= chars.len() {
            break;
        }
    }
    out
}

/// Per-char "changed" mask aligned to a diff side's text — drives the
/// word-level emphasis in [`diff_cell`] (item 601). Length equals the side's
/// char count (the `word_diff` segments concatenate to the full text).
fn changed_mask(segs: &[superzej_core::diff_highlight::WordSeg]) -> Vec<bool> {
    let mut m = Vec::new();
    for s in segs {
        m.extend(std::iter::repeat_n(s.changed, s.text.chars().count()));
    }
    m
}

/// The flattened line at index `at`: a hunk header (one visual row) or an
/// aligned old/new row pair. Long cells wrap, so a row pair can span several
/// visual rows; the old and new sides wrap independently and stay column-locked
/// (the shorter side blanks its trailing continuation rows). Always ≥ 1 row.
fn diff_flat_line(file: &SbsFile, starts: &[usize], at: usize, side: usize) -> Vec<Line> {
    let h = diff_hunk_at(starts, at);
    let Some(hunk) = file.hunks.get(h) else {
        return vec![Line::Blank];
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
        return vec![Line::segs(segs)];
    }
    let Some(row) = hunk.rows.get(off - 1) else {
        return vec![Line::Blank];
    };
    // Word-level emphasis (item 601): only when a removed line is paired with
    // an added line do we diff the two texts and emphasize just the changed
    // runs. Pure add/del rows have nothing to diff against → no mask.
    let (old_mask, new_mask) = match (row.old.as_ref(), row.new.as_ref()) {
        (Some(o), Some(n)) if o.kind == CellKind::Removed && n.kind == CellKind::Added => {
            let (os, ns) = superzej_core::diff_highlight::word_diff(&o.text, &n.text);
            (Some(changed_mask(&os)), Some(changed_mask(&ns)))
        }
        _ => (None, None),
    };
    let old = diff_cell(row.old.as_ref(), side, old_mask.as_deref());
    let new = diff_cell(row.new.as_ref(), side, new_mask.as_deref());
    let n = old.len().max(new.len());
    let blank = || vec![sp(side)];
    (0..n)
        .map(|k| {
            let mut segs = old.get(k).cloned().unwrap_or_else(blank);
            segs.push(seg(g3(), "│"));
            segs.extend(new.get(k).cloned().unwrap_or_else(blank));
            Line::segs(segs)
        })
        .collect()
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
    // Scroll stays in flat-line units (j/k and hunk-nav land on hunk starts);
    // expand each flat line into its wrapped visual rows, stopping once the
    // viewport is full. A flat line longer than the band shows its head and the
    // tail spills below the fold — strictly better than the old hard clip.
    let mut produced = 0;
    let mut at = scroll;
    while produced < body && at < len {
        for line in diff_flat_line(&doc.file, &starts, at, side) {
            if produced >= body {
                break;
            }
            rows.push(PanelRow::plain(line));
            produced += 1;
        }
        at += 1;
    }
    rows.push(footer);
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(line_no: u32, text: &str, kind: CellKind) -> SbsCell {
        SbsCell {
            line_no,
            text: text.to_string(),
            kind,
        }
    }

    /// Concatenated text of a built seg run, for asserting on rendered content.
    fn segs_text(segs: &[Seg]) -> String {
        segs.iter().map(|s| s.text.as_str()).collect()
    }

    #[test]
    fn diff_cell_wraps_long_text_onto_continuation_rows() {
        // side = 15 → text_w = 10. A 25-char line needs 3 rows.
        let c = cell(7, "0123456789abcdefghijklmno", CellKind::Added);
        let rows = diff_cell(Some(&c), 15, None);
        assert_eq!(rows.len(), 3);
        // First row carries the line number; continuations blank the gutter.
        assert_eq!(segs_text(&rows[0]), "   7 0123456789");
        assert_eq!(segs_text(&rows[1]), "     abcdefghij");
        assert_eq!(segs_text(&rows[2]), "     klmno     ");
        // Every visual row is exactly `side` cells wide.
        for r in &rows {
            assert_eq!(seg_width(r), 15);
        }
    }

    #[test]
    fn diff_cell_short_text_is_a_single_row() {
        let c = cell(3, "hi", CellKind::Context);
        let rows = diff_cell(Some(&c), 15, None);
        assert_eq!(rows.len(), 1);
        assert_eq!(segs_text(&rows[0]), "   3 hi        ");
        // Absent cell → one blank row of full width.
        let blank = diff_cell(None, 15, None);
        assert_eq!(blank.len(), 1);
        assert_eq!(seg_width(&blank[0]), 15);
    }

    #[test]
    fn diff_flat_line_locks_columns_when_sides_wrap_unevenly() {
        let mut file = SbsFile::default();
        file.hunks.push(superzej_core::diff_sbs::SbsHunk {
            old_start: 1,
            new_start: 1,
            func: String::new(),
            rows: vec![superzej_core::diff_sbs::SbsRow {
                old: Some(cell(1, "short", CellKind::Removed)),
                new: Some(cell(1, "0123456789abcdefghij", CellKind::Added)),
            }],
        });
        let starts = diff_hunk_starts(&file);
        // at=1 is the first row (at=0 is the hunk header). side=15 → text_w=10:
        // old fits in 1 row, new needs 2 → the pair spans 2 column-locked rows.
        let lines = diff_flat_line(&file, &starts, 1, 15);
        assert_eq!(lines.len(), 2);
        for line in &lines {
            if let Line::Segs(segs) = line {
                // 15 (old) + 1 (│ separator) + 15 (new) = 31 cells.
                assert_eq!(seg_width(segs), 31);
            } else {
                panic!("expected Segs");
            }
        }
    }

    #[test]
    fn wrap_preview_continues_long_lines_under_the_text() {
        // cols = 14 → text_w = 10.
        let rows = wrap_preview(t(), "+ ", "0123456789abcde", 14);
        assert_eq!(rows.len(), 2);
        if let Line::Segs(segs) = &rows[0].line {
            // sp(2) + "+ " + first 10 chars.
            assert_eq!(segs_text(segs), "  + 0123456789");
        } else {
            panic!("expected Segs");
        }
        if let Line::Segs(segs) = &rows[1].line {
            // continuation aligns under the text at the 4-cell indent.
            assert_eq!(segs_text(segs), "    abcde");
        } else {
            panic!("expected Segs");
        }
    }

    #[test]
    fn wrap_preview_empty_text_keeps_the_mark() {
        let rows = wrap_preview(t(), "  ", "", 40);
        assert_eq!(rows.len(), 1);
        if let Line::Segs(segs) = &rows[0].line {
            assert_eq!(segs_text(segs), "    ");
        } else {
            panic!("expected Segs");
        }
    }

    /// A change row renders the directory prefix and the file name legibly on
    /// the panel surface it is painted on (see `chrome::draw_panel`). Guards the
    /// regression where the dir prefix used `ghost2` (the structural floor,
    /// ~2.1:1 on the panel) and read as grey-on-grey beside the file name.
    #[test]
    fn change_row_path_is_legible_on_the_panel() {
        use termwiz::surface::Surface;
        let c = ChangeRow {
            status: "M".into(),
            stage: Stage::Unstaged,
            dir: "crates/superzej-host/src/".into(),
            name: "changes.rs".into(),
            path: "crates/superzej-host/src/changes.rs".into(),
            added: 3,
            deleted: 1,
        };
        for on in [false, true] {
            let row = change_row(&c, 0, on, false, 70);
            let mut s = Surface::new(70, 1);
            crate::seg::draw_line(
                &mut s,
                0,
                0,
                70,
                &row.line,
                // The panel pad background `draw_panel` paints rows on.
                row.bg.unwrap_or(Tok::Slot(crate::chrome::S::Panel)),
            );
            let v = crate::seg::text_contrast_violations(&mut s, 3.0);
            assert!(
                v.is_empty(),
                "low-contrast text in change row (on={on}): {v:?}"
            );
        }
    }

    /// The expanded semantic-impact breakdown paints its colored touch glyphs,
    /// kind labels and entity names legibly on the panel surface, and renders all
    /// three touch verbs (+/~/−).
    #[test]
    fn impact_breakdown_rows_are_legible_on_the_panel() {
        use superzej_core::semantic::{EntityChange, EntityKind, EntitySummary, Touch};
        use termwiz::surface::Surface;
        let data = crate::panel::PanelData {
            changes: vec![ChangeRow {
                status: "M".into(),
                stage: Stage::Unstaged,
                dir: "src/".into(),
                name: "a.rs".into(),
                path: "src/a.rs".into(),
                added: 30,
                deleted: 10,
            }],
            entities: Some(EntitySummary::new(vec![(
                "src/a.rs".into(),
                vec![
                    EntityChange {
                        kind: EntityKind::Function,
                        name: "handle".into(),
                        added: 12,
                        deleted: 2,
                        touch: Touch::Modified,
                        start_line: 1,
                    },
                    EntityChange {
                        kind: EntityKind::Enum,
                        name: "Verdict".into(),
                        added: 18,
                        deleted: 0,
                        touch: Touch::Added,
                        start_line: 1,
                    },
                    EntityChange {
                        kind: EntityKind::Function,
                        name: "old".into(),
                        added: 0,
                        deleted: 8,
                        touch: Touch::Removed,
                        start_line: 1,
                    },
                ],
            )])),
            ..Default::default()
        };
        let ui = PanelUi {
            impact_open: true,
            ..Default::default()
        };
        let rows = impact_footer(&data, &ui, 44);
        for row in &rows {
            let mut s = Surface::new(44, 1);
            crate::seg::draw_line(
                &mut s,
                0,
                0,
                44,
                &row.line,
                row.bg.unwrap_or(Tok::Slot(crate::chrome::S::Panel)),
            );
            let v = crate::seg::text_contrast_violations(&mut s, 3.0);
            assert!(v.is_empty(), "low-contrast breakdown row: {v:?}");
        }
        let all = segs_text(
            &rows
                .iter()
                .flat_map(|r| match &r.line {
                    Line::Segs(v) => v.clone(),
                    _ => Vec::new(),
                })
                .collect::<Vec<_>>(),
        );
        assert!(all.contains("+ "), "added glyph: {all}");
        assert!(all.contains("~ "), "modified glyph: {all}");
        assert!(all.contains("− "), "removed glyph: {all}");
        assert!(
            all.contains("Verdict") && all.contains("handle"),
            "names: {all}"
        );
    }

    #[test]
    fn clip_dir_left_passes_through_short_paths() {
        assert_eq!(clip_dir_left("src/", 6, 20), "src/");
        assert_eq!(clip_dir_left("", 6, 20), "");
    }

    #[test]
    fn clip_dir_left_snaps_to_slash_boundary() {
        // "crates/superzej-host/src/" (25 chars) + "chrome.rs" (9) = 34
        // budget = 20 → dir_budget = 11, take = 10, from = 15 → snap after "host/"
        let d = clip_dir_left("crates/superzej-host/src/", 9, 20);
        assert!(d.starts_with('…'), "got: {d}");
        assert!(d.ends_with('/'), "should end with /: {d}");
        assert!(!d.contains("crates"), "leading dir should be clipped: {d}");
    }

    #[test]
    fn clip_dir_left_drops_dir_when_budget_too_tight() {
        // budget barely fits the name (10) — no room for any dir
        assert_eq!(clip_dir_left("src/long/path/", 10, 10), "");
        // budget = 0 or 1 → empty
        assert_eq!(clip_dir_left("src/", 3, 1), "");
        assert_eq!(clip_dir_left("src/", 3, 0), "");
    }

    #[test]
    fn clip_dir_left_shows_name_only_when_no_slash_in_range() {
        // dir = "longname/" (9 chars), name = 5, budget = 8 → dir_budget = 3, take = 2
        // chars["longname/"][from=7..] = "e/" — no previous slash to snap to
        let d = clip_dir_left("longname/", 5, 8);
        // Either shows "…e/" or falls back to empty — either is acceptable
        assert!(d.is_empty() || d.starts_with('…'));
    }
}
