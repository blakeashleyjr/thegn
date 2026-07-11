//! The `Across` section — a read-only, cross-worktree attention stream
//! (multibuffer-style). Reads `model.panel.across` (built off-loop during
//! hydration from every worktree's CI cache): failing CI — and, as those
//! producers land, dirty files / content matches — from *all* worktrees, grouped
//! by worktree with per-row source labels. Each excerpt row carries a
//! `PanelHit::Row(Across, i)` so the cursor can rest on it (and a future
//! one-key "open at source" can resolve it via `Aggregation::jump_target`).

use thegn_core::aggregate::{AggRow, Aggregation, ExcerptKind};
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

pub(super) fn content(ctx: &SectionCtx) -> Vec<PanelRow> {
    build_rows(&ctx.model.panel.across, ctx.deep())
}

/// Render the aggregation into panel rows. Pure over the model + view depth so
/// it is unit-testable without a full `FrameModel`.
fn build_rows(agg: &Aggregation, deep: bool) -> Vec<PanelRow> {
    if agg.is_empty() {
        return vec![PanelRow::plain(Line::segs(vec![seg(
            d(),
            "nothing needs attention across worktrees",
        )]))];
    }

    let mut rows: Vec<PanelRow> = Vec::new();

    // Summary line: "3✗ · 1● · across 2 worktrees".
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
    sum.push(seg(d(), format!(" across {} worktrees", s.worktrees)));
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
                        Some(n) => format!("{}:{n} ", e.file),
                        None => format!("{} ", e.file),
                    };
                    segs.push(seg(f(), loc));
                }
                segs.push(seg(d(), e.text.clone()));
                if deep && !e.detail.is_empty() {
                    segs.push(seg(f(), format!("  {}", e.detail)));
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
        assert_eq!(rows.len(), 1);
        assert!(rows[0].hit.is_none());
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
