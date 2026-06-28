//! The merge-queue section (the fold-actor): per-branch land/defer status from
//! the `merge_queue` cache. A read-only, at-a-glance "what's queued, what landed,
//! what needs a rebase". Reads `model.panel.merge_queue` (populated from the
//! `merge_queue` table each model build). The land/defer machinery is the
//! `szhost integrate` runner; this is just the visibility surface.

use superzej_core::theme::Hue;

use crate::seg::{Line, Seg, seg};

use super::{PanelRow, SectionCtx, d, g, g2, hue};

/// The hued glyph for a queue row's status string.
pub(super) fn status_glyph(status: &str) -> Seg {
    match status {
        "landed" => seg(hue(Hue::Green), "✓"),
        "deferred" | "gate_failed" => seg(hue(Hue::Red), "⚑"),
        "folding" | "verifying" => seg(hue(Hue::Amber), "●"),
        _ => seg(g(), "○"), // queued / unknown
    }
}

pub(super) fn content(ctx: &SectionCtx) -> Vec<PanelRow> {
    let rows = &ctx.model.panel.merge_queue;
    if rows.is_empty() {
        return vec![PanelRow::plain(Line::segs(vec![seg(
            d(),
            "merge queue empty",
        )]))];
    }
    let mut out: Vec<PanelRow> = Vec::new();
    for r in rows {
        let mut left = vec![status_glyph(&r.status), seg(d(), format!(" {}", r.branch))];
        // Deferred rows carry the reason: the conflicting paths, or "breaks
        // build" for a gate failure (which has no textual conflict).
        if r.status == "deferred" || r.status == "gate_failed" {
            let reason = if r.status == "gate_failed" {
                "breaks build".to_string()
            } else {
                match r.conflict_paths.as_deref() {
                    Some(p) if !p.is_empty() => p.replace('\n', ", "),
                    _ => "conflict".to_string(),
                }
            };
            left.push(seg(g(), "  "));
            left.push(seg(hue(Hue::Red), reason));
        }
        out.push(PanelRow::plain(Line::split(
            left,
            vec![seg(g2(), r.status.clone())],
        )));
    }
    out
}
