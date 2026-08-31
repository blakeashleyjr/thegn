//! The PR-queue section: per-pull-request blocker/status from the `pr_queue`
//! cache, with the queue-management action keys.
//!
//! Reads `model.panel.pr_queue` (populated from the `pr_queue` table each model
//! build, and patched in place by the live drain — see `handlers::pr_queue`).
//! Each row carries a cursor hit aligned with `ui.cursor`; the action keys are
//! dispatched by the loop's section-key arm to `handlers::pr_queue::section_key`,
//! so the hint row below can never drift from the dispatch.

use thegn_core::pr_queue::PrqStatus;
use thegn_core::theme::Hue;

use crate::seg::{Line, Seg, seg};

use super::{PanelHit, PanelRow, Section, SectionCtx, d, g, g2, hint_row, hue};

/// The hued glyph for a PR-queue status. Deliberately the same vocabulary as the
/// merge queue's — a blocked row is a blocked row — so a reader who has learned
/// one section can read the other.
pub(super) fn status_glyph(status: &str) -> Seg {
    let gl = crate::caps::active_glyphs();
    match PrqStatus::parse(status) {
        Some(PrqStatus::Merged) => seg(hue(Hue::Green), gl.check),
        Some(PrqStatus::Ready) => seg(hue(Hue::Blue), gl.check),
        Some(PrqStatus::AgentRunning) => seg(hue(Hue::Amber), gl.dot_filled),
        Some(PrqStatus::Merging) => seg(hue(Hue::Blue), gl.dot_filled),
        Some(PrqStatus::NeedsHuman)
        | Some(PrqStatus::BlockedCi)
        | Some(PrqStatus::BlockedConflict) => seg(hue(Hue::Red), gl.cross),
        // Awaiting a human's review is the normal resting state of a healthy
        // pull request, not a failure — amber, never red.
        Some(PrqStatus::BlockedReview) => seg(hue(Hue::Amber), gl.dot_hollow),
        // Dim (no hue) for the quiet states, like the merge queue's unknown arm.
        Some(PrqStatus::Closed) | Some(PrqStatus::Watching) | None => seg(g(), gl.dot_hollow),
    }
}

pub(super) fn content(ctx: &SectionCtx) -> Vec<PanelRow> {
    let rows = &ctx.model.panel.pr_queue;
    if rows.is_empty() {
        return vec![
            PanelRow::plain(Line::segs(vec![seg(d(), "PR queue empty")])),
            prq_hint_row(),
        ];
    }
    let mut out: Vec<PanelRow> = Vec::new();
    let mut display_index = 0;
    for r in rows {
        let mut left = vec![
            status_glyph(&r.status),
            seg(d(), format!(" #{} ", r.number)),
            seg(g2(), r.branch.clone()),
        ];
        // A PR with no local checkout can be watched but never agent-fixed, so
        // say so on the row rather than leaving the reader to wonder why nothing
        // is happening to it.
        if r.worktree.as_deref().is_none_or(str::is_empty) {
            left.push(seg(g(), " (no worktree)"));
        }
        if let Some(detail) = r.detail.as_deref().filter(|s| !s.is_empty()) {
            let head = detail.lines().next().unwrap_or(detail);
            left.push(seg(g(), "  "));
            left.push(match r.status.as_str() {
                "needs_human" | "blocked_ci" | "blocked_conflict" => seg(hue(Hue::Red), head),
                "blocked_review" | "agent_running" => seg(hue(Hue::Amber), head),
                _ => seg(g(), head),
            });
        }
        out.push(
            PanelRow::plain(Line::split(left, vec![seg(g2(), r.status.clone())]))
                .with_hit(PanelHit::Row(Section::PrQueue, display_index)),
        );
        display_index += 1;
        for task in ctx
            .model
            .panel
            .review_tasks
            .iter()
            .filter(|task| task.pr_number == r.number)
        {
            let location = match task.line {
                Some(line) if !task.path.is_empty() => format!("{}:{line}", task.path),
                _ if task.path.is_empty() => "unanchored".to_string(),
                _ => task.path.clone(),
            };
            let role = if task.role.trim().is_empty() {
                "explicit command"
            } else {
                task.role.as_str()
            };
            let task_hue = match task.status {
                thegn_core::issue::AgentDispatchStatus::Queued => Hue::Blue,
                thegn_core::issue::AgentDispatchStatus::Running
                | thegn_core::issue::AgentDispatchStatus::Spawning => Hue::Amber,
                thegn_core::issue::AgentDispatchStatus::WaitingHuman
                | thegn_core::issue::AgentDispatchStatus::Failed => Hue::Red,
                _ => Hue::Green,
            };
            let left = vec![
                seg(g(), "   "),
                seg(hue(task_hue), crate::caps::active_glyphs().diamond_hollow),
                seg(d(), format!(" #{} / {} ", task.pr_number, task.thread_id)),
                seg(g2(), location),
                seg(g(), format!("  role {role}")),
            ];
            let right = vec![seg(
                g2(),
                format!("{} · rev {}", task.status.as_str(), task.source_revision),
            )];
            out.push(
                PanelRow::plain(Line::split(left, right))
                    .with_hit(PanelHit::Row(Section::PrQueue, display_index)),
            );
            display_index += 1;
        }
    }
    out.push(prq_hint_row());
    out
}

/// The per-section key hints (the same keys the event loop dispatches to
/// `handlers::pr_queue::section_key`, so they can't drift).
fn prq_hint_row() -> PanelRow {
    hint_row(&[
        ("a", "add"),
        ("x", "rm"),
        ("r", "re-watch"),
        ("c", "clear"),
        ("D", "refresh"),
        ("o", "browser"),
        ("h", "handle"),
    ])
}
