//! The merge-queue section (the fold-actor): per-branch land/defer status from
//! the `merge_queue` cache, with the queue-management action keys. Reads
//! `model.panel.merge_queue` (populated from the `merge_queue` table each model
//! build, and patched in place by the live drain — see
//! `handlers::merge_queue`). Each queue row carries a cursor hit aligned with
//! `ui.cursor`; the action keys (`a/A/x/l/r/c/D`) are dispatched by the loop's
//! section-key arm to `handlers::merge_queue::section_key`, so the hint row
//! below can never drift from the dispatch.

use superzej_core::theme::Hue;

use crate::seg::{Line, Seg, seg};

use super::{PanelHit, PanelRow, Section, SectionCtx, d, g, g2, hint_row, hue};

/// The hued glyph for a queue row's status string.
pub(super) fn status_glyph(status: &str) -> Seg {
    match status {
        "landed" => seg(hue(Hue::Green), "✓"),
        "ready" => seg(hue(Hue::Green), "◆"), // gated green, awaiting a land
        "deferred" | "gate_failed" => seg(hue(Hue::Red), "⚑"),
        "needs_human" => seg(hue(Hue::Red), "✋"), // agent tried and gave up
        "folding" | "verifying" => seg(hue(Hue::Amber), "●"),
        "agent_running" => seg(hue(Hue::Amber), "◐"), // agent fixing the branch
        _ => seg(g(), "○"),                           // queued / unknown
    }
}

pub(super) fn content(ctx: &SectionCtx) -> Vec<PanelRow> {
    let rows = &ctx.model.panel.merge_queue;
    if rows.is_empty() {
        return vec![
            PanelRow::plain(Line::segs(vec![seg(d(), "merge queue empty")])),
            mq_hint_row(),
        ];
    }
    let mut out: Vec<PanelRow> = Vec::new();
    for (i, r) in rows.iter().enumerate() {
        let mut left = vec![status_glyph(&r.status), seg(d(), format!(" {}", r.branch))];
        // Blocked rows carry the reason: the conflicting paths, "breaks build"
        // for a gate failure, or the recorded detail when an agent gave up.
        if r.status == "deferred" || r.status == "gate_failed" || r.status == "needs_human" {
            let reason = if r.status == "gate_failed" {
                "breaks build".to_string()
            } else if let Some(d) = r.error_detail.as_deref().filter(|s| !s.is_empty()) {
                d.replace('\n', ", ")
            } else {
                match r.conflict_paths.as_deref() {
                    Some(p) if !p.is_empty() => p.replace('\n', ", "),
                    _ => "conflict".to_string(),
                }
            };
            left.push(seg(g(), "  "));
            left.push(seg(hue(Hue::Red), reason));
        }
        // Each queue row carries a `Row` hit so the enumerate index lines up
        // with `ui.cursor` and with `model.panel.merge_queue`.
        out.push(
            PanelRow::plain(Line::split(left, vec![seg(g2(), r.status.clone())]))
                .with_hit(PanelHit::Row(Section::MergeQueue, i)),
        );
    }
    out.push(mq_hint_row());
    out
}

/// The per-section key hints (the same keys the event loop dispatches to
/// `handlers::merge_queue::section_key`, so they can't drift).
fn mq_hint_row() -> PanelRow {
    hint_row(&[
        ("a/A", "add"),
        ("x", "rm"),
        ("l", "land"),
        ("r", "retry"),
        ("c", "clear ✓"),
        ("D", "drain"),
    ])
}
