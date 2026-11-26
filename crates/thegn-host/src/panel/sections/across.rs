//! The `Attention` section (internal id `Across`) — a cross-worktree attention
//! stream (multibuffer-style). Reads `model.panel.across` (built off-loop during
//! hydration from the CI caches of the active repo's worktrees — or every
//! workspace's, under the `a` toggle): failing CI — and, as those producers
//! land, dirty files / content matches — grouped by worktree with per-row
//! source labels. Each excerpt row carries a `PanelHit::Row(Across, i)`;
//! Enter resolves it via `Aggregation::jump_target` and activates its open tab
//! or registered worktree.

use thegn_core::aggregate::{AggRow, Aggregation, ExcerptKind};
use thegn_core::pipeline_report::display_cell;
use thegn_core::theme::Hue;

use crate::seg::{Line, Seg, seg};

use super::{PanelRow, SectionCtx, d, f, g, hue};
use crate::panel::{PanelHit, Section};

/// The hued glyph for an excerpt kind.
fn kind_glyph(kind: ExcerptKind) -> Seg {
    match kind {
        ExcerptKind::CiFailure => seg(hue(Hue::Red), "✗"),
        ExcerptKind::DirtyFile => seg(hue(Hue::Amber), "●"),
        ExcerptKind::ContentMatch => seg(g(), "·"),
    }
}

/// The one-line scope hint: what the section covers and what `a` switches to.
/// Spelled out in words so the scope is never a bare symbol to decode.
fn scope_hint(all: bool) -> &'static str {
    if all {
        " (all projects · a = this project only)"
    } else {
        " (this project · a = all projects)"
    }
}

pub(super) fn content(ctx: &SectionCtx) -> Vec<PanelRow> {
    if ctx.full() && !ctx.model.panel.across.is_empty() {
        return full_view(ctx);
    }
    build_rows(&ctx.model.panel.across, ctx.deep())
}

/// Full: excerpt list + a detail column for the cursor excerpt (worktree
/// path, kind, source location, full text + detail, and where ↵ lands).
fn full_view(ctx: &SectionCtx) -> Vec<PanelRow> {
    use super::{g2, list_detail, rule, t, wrap_text};
    let agg = &ctx.model.panel.across;
    let cols = ctx.cols;
    let all = crate::panel::scope::across_all();
    let mut rows: Vec<PanelRow> = Vec::new();

    let s = agg.summary();
    rows.push(PanelRow::plain(Line::segs(vec![
        seg(d(), "ATTENTION"),
        seg(
            f(),
            format!(
                " · {}✗ {}● {}· {} worktrees{}",
                s.failures,
                s.dirty,
                s.matches,
                s.worktrees,
                scope_hint(all)
            ),
        ),
    ])));
    rows.push(rule());

    // The left list keeps the grouped shape (group dividers interleaved);
    // hits and the cursor walk the excerpt rows only, in agg order.
    let mut list_rows: Vec<Vec<Seg>> = Vec::new();
    // For row i of the list: Some(excerpt index) when it's an excerpt row.
    let mut list_hits: Vec<Option<usize>> = Vec::new();
    let mut excerpts: Vec<usize> = Vec::new();
    for row in agg.rows() {
        match row {
            AggRow::Group { label, count } => {
                list_rows.push(vec![
                    seg(hue(Hue::Blue), display_cell(&label)),
                    seg(d(), format!(" ·{count}")),
                ]);
                list_hits.push(None);
            }
            AggRow::Excerpt(i) => {
                let Some(e) = agg.jump_target(i) else {
                    continue;
                };
                let sel = excerpts.len() == ctx.ui.cursor;
                list_rows.push(vec![
                    seg(if sel { t() } else { g() }, if sel { "▶ " } else { "  " }),
                    kind_glyph(e.kind),
                    seg(
                        if sel { t() } else { d() },
                        format!(" {}", display_cell(&e.text)),
                    ),
                ]);
                list_hits.push(Some(i));
                excerpts.push(i);
            }
        }
    }

    let list_w = 45_usize.min(cols / 2);
    let detail_w = cols.saturating_sub(list_w + 2);
    let cursor = ctx.ui.cursor.min(excerpts.len().saturating_sub(1));
    let detail: Vec<Vec<Seg>> = match excerpts.get(cursor).and_then(|&i| agg.jump_target(i)) {
        Some(e) => {
            let kind = match e.kind {
                ExcerptKind::CiFailure => "CI failure",
                ExcerptKind::DirtyFile => "dirty file",
                ExcerptKind::ContentMatch => "content match",
            };
            let mut d_rows: Vec<Vec<Seg>> = vec![
                vec![kind_glyph(e.kind), seg(t(), format!(" {kind}")).bold()],
                vec![
                    seg(g2(), "worktree  "),
                    seg(d(), display_cell(&e.worktree_label)),
                    seg(f(), format!("  {}", display_cell(&e.worktree))),
                ],
            ];
            if !e.file.is_empty() {
                let loc = match e.line {
                    Some(n) => format!("{}:{n}", e.file),
                    None => e.file.clone(),
                };
                d_rows.push(vec![seg(g2(), "at  "), seg(d(), display_cell(&loc))]);
            }
            d_rows.push(Vec::new());
            for chunk in wrap_text(&display_cell(&e.text), detail_w) {
                d_rows.push(vec![seg(t(), chunk)]);
            }
            if !e.detail.is_empty() {
                for chunk in wrap_text(&display_cell(&e.detail), detail_w) {
                    d_rows.push(vec![seg(f(), chunk)]);
                }
            }
            d_rows.push(Vec::new());
            d_rows.push(vec![seg(g2(), "↵ jumps to this worktree's tab")]);
            d_rows
        }
        None => vec![vec![seg(g2(), "select an excerpt")]],
    };

    let entries: Vec<(Vec<Seg>, Option<usize>)> = list_rows.into_iter().zip(list_hits).collect();
    rows.extend(list_detail(
        Section::Across,
        &entries,
        &detail,
        cursor,
        list_w,
        ctx.rows.saturating_sub(rows.len()),
    ));
    rows
}

/// Render the aggregation into panel rows. Pure over the model + view depth so
/// it is unit-testable without a full `FrameModel`.
fn build_rows(agg: &Aggregation, deep: bool) -> Vec<PanelRow> {
    let all = crate::panel::scope::across_all();
    let scope_tail = scope_hint(all);
    if agg.is_empty() {
        return vec![
            PanelRow::plain(Line::segs(vec![seg(
                d(),
                format!("nothing needs attention in any worktree{scope_tail}"),
            )])),
            PanelRow::plain(Line::segs(vec![seg(
                f(),
                "failing CI in your worktrees shows here; ↵ jumps to it",
            )])),
        ];
    }

    let mut rows: Vec<PanelRow> = Vec::new();

    // Summary line: "3✗ · 1● · 2 worktrees (this project · a = all projects)".
    let s = agg.summary();
    let mut sum: Vec<Seg> = Vec::new();
    if s.failures > 0 {
        sum.push(seg(hue(Hue::Red), format!("{}✗", s.failures)));
    }
    if s.dirty > 0 {
        sum.push(seg(hue(Hue::Amber), format!(" {}●", s.dirty)));
    }
    if s.matches > 0 {
        sum.push(seg(g(), format!(" {}·", s.matches)));
    }
    sum.push(seg(d(), format!(" in {} worktrees", s.worktrees)));
    sum.push(seg(f(), scope_tail.to_string()));
    rows.push(PanelRow::plain(Line::segs(sum)));

    for row in agg.rows() {
        match row {
            AggRow::Group { label, count } => {
                rows.push(PanelRow::plain(Line::segs(vec![
                    seg(hue(Hue::Blue), label),
                    seg(d(), format!(" ·{count}")),
                ])));
            }
            AggRow::Excerpt(i) => {
                let Some(e) = agg.jump_target(i) else {
                    continue;
                };
                let mut segs = vec![kind_glyph(e.kind), seg(d(), " ")];
                // Source location (file:line) in the deep views; text always.
                if deep && !e.file.is_empty() {
                    let loc = match e.line {
                        Some(n) => format!("{}:{n} ", display_cell(&e.file)),
                        None => format!("{} ", display_cell(&e.file)),
                    };
                    segs.push(seg(f(), loc));
                }
                segs.push(seg(d(), display_cell(&e.text)));
                if deep && !e.detail.is_empty() {
                    segs.push(seg(f(), format!("  {}", display_cell(&e.detail))));
                }
                rows.push(
                    PanelRow::plain(Line::segs(segs)).with_hit(PanelHit::Row(Section::Across, i)),
                );
            }
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::aggregate::Excerpt;

    fn ex(label: &str, kind: ExcerptKind, file: &str, text: &str) -> Excerpt {
        Excerpt {
            worktree: format!("/wt/{label}"),
            worktree_label: label.to_string(),
            kind,
            file: file.to_string(),
            line: Some(7),
            text: text.to_string(),
            detail: "d".into(),
        }
    }

    #[test]
    fn empty_shows_placeholder() {
        let rows = build_rows(&Aggregation::default(), true);
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.hit.is_none()));
        // The layout pins the plain-words wording and the `a` scope hint.
        assert_eq!(
            row_text(&rows[0]),
            "nothing needs attention in any worktree (this project · a = all projects)"
        );
        assert!(row_text(&rows[1]).contains("↵ jumps to it"));
    }

    fn row_text(row: &PanelRow) -> String {
        match &row.line {
            Line::Segs(segs) => segs.iter().map(|s| s.text.as_str()).collect(),
            _ => String::new(),
        }
    }

    #[test]
    fn summary_names_worktrees_and_scope_in_words() {
        let agg = Aggregation::from_excerpts(vec![
            ex("alpha", ExcerptKind::CiFailure, "", "build"),
            ex("zeta", ExcerptKind::CiFailure, "", "test"),
        ]);
        let rows = build_rows(&agg, false);
        assert_eq!(
            row_text(&rows[0]),
            "2✗ in 2 worktrees (this project · a = all projects)"
        );
        assert!(!row_text(&rows[0]).contains("across"));
    }

    #[test]
    fn renders_summary_groups_and_hittable_excerpts() {
        let agg = Aggregation::from_excerpts(vec![
            ex("alpha", ExcerptKind::CiFailure, "", "build"),
            ex("alpha", ExcerptKind::DirtyFile, "a.rs", "a.rs"),
            ex("zeta", ExcerptKind::CiFailure, "", "test"),
        ]);
        let rows = build_rows(&agg, true);
        // 1 summary + 2 group dividers + 3 excerpts.
        assert_eq!(rows.len(), 6);
        // Exactly the excerpt rows are hittable, and their indices resolve.
        let hits: Vec<usize> = rows
            .iter()
            .filter_map(|r| match r.hit {
                Some(PanelHit::Row(Section::Across, i)) => Some(i),
                _ => None,
            })
            .collect();
        assert_eq!(hits, vec![0, 1, 2]);
        for i in hits {
            assert!(agg.jump_target(i).is_some());
        }
    }
}
