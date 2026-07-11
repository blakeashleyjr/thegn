//! The CI section (AV group): recent runs with a per-run state glyph + duration,
//! expanding the latest run's jobs when the band is deep. Reads
//! `model.panel.ci_runs` (populated from the `ci_runs_cache`). The full
//! drilldown is the CLI (`thegn ci view <id>`); this is the at-a-glance
//! "is the pipeline green?" rollup, the CI-provider analogue of the PR checks.

use thegn_core::ci::{CiState, current_summary};
use thegn_core::theme::Hue;

use crate::seg::{Line, Seg, seg};

use super::{PanelHit, PanelRow, Section, SectionCtx, d, f, fmt_secs, g, g2, hint_row, hue};

/// A state's hued glyph (shared by run rows and job rows).
pub(super) fn state_glyph(s: CiState) -> Seg {
    match s {
        CiState::Pass => seg(hue(Hue::Green), "✓"),
        CiState::Fail => seg(hue(Hue::Red), "✗"),
        CiState::Running => seg(hue(Hue::Amber), "●"),
        CiState::Pending => seg(g(), "○"),
        CiState::Cancelled => seg(g(), "⊘"),
        CiState::Skipped => seg(f(), "–"),
    }
}

/// The rollup summary line shared by the list and full views: per-state counts
/// from [`current_summary`] (one entry per workflow, judged by its most recent
/// run — never a wall of historical failures), plus the cache's fetch age so
/// stale data reads as stale.
fn summary_line(label: &str, data: &crate::panel::PanelData, now: i64) -> Line {
    let sum = current_summary(&data.ci_runs);
    let mut segs = vec![seg(d(), label.to_string())];
    if sum.passed > 0 {
        segs.push(seg(hue(Hue::Green), format!(" {}✓", sum.passed)));
    }
    if sum.failed > 0 {
        segs.push(seg(hue(Hue::Red), format!(" {}✗", sum.failed)));
    }
    if sum.running > 0 {
        segs.push(seg(hue(Hue::Amber), format!(" {}●", sum.running)));
    }
    if sum.pending > 0 {
        segs.push(seg(g(), format!(" {}○", sum.pending)));
    }
    if sum.other > 0 {
        segs.push(seg(g(), format!(" {}", sum.other)));
    }
    let age = data
        .ci_fetched_at
        .map(|at| format!("{} ago", fmt_secs(now.saturating_sub(at).max(0))))
        .unwrap_or_default();
    Line::split(segs, vec![seg(f(), age)])
}

pub(super) fn content(ctx: &SectionCtx) -> Vec<PanelRow> {
    if ctx.full() {
        return full(ctx);
    }
    list(ctx)
}

/// Normal/Half: recent runs (a handful, or as many as fit when deep), expanding
/// the latest run's jobs in the deep (Half) view.
fn list(ctx: &SectionCtx) -> Vec<PanelRow> {
    let data = &ctx.model.panel;
    let now = thegn_core::util::now();
    if data.ci_runs.is_empty() {
        return vec![PanelRow::plain(Line::segs(vec![seg(
            d(),
            data.ci_note.as_deref().unwrap_or("no CI runs"),
        )]))];
    }
    let mut rows: Vec<PanelRow> = Vec::new();
    let limit = if ctx.deep() { ctx.rows.max(1) } else { 5 };

    // Summary row: current state (latest run per workflow), not history,
    // stamped with the cache's fetch age so stale data is visibly stale.
    rows.push(PanelRow::plain(summary_line("CI ", data, now)));
    if let Some(note) = &data.ci_note {
        rows.push(PanelRow::plain(Line::segs(vec![seg(
            hue(Hue::Amber),
            format!("⚠ {note}"),
        )])));
    }

    // Each run row carries a `Row` hit (the summary/job rows don't), so the
    // enumerate index lines up with `ui.cursor` and with `ci_runs`.
    for (i, r) in data.ci_runs.iter().take(limit).enumerate() {
        let dur = r
            .duration_secs(now)
            .map(fmt_secs)
            .unwrap_or_else(|| "—".into());
        let mut left = vec![state_glyph(r.state), seg(d(), format!(" {}", r.name))];
        if !r.branch.is_empty() {
            left.push(seg(g(), format!("  {}", r.branch)));
        }
        rows.push(
            PanelRow::plain(Line::split(left, vec![seg(g(), dur)]))
                .with_hit(PanelHit::Row(Section::Ci, i)),
        );
    }
    // Deep (Half): expand the most-recent run's jobs.
    if ctx.deep()
        && let Some(latest) = data.ci_runs.first()
    {
        for j in &latest.jobs {
            rows.push(PanelRow::plain(Line::segs(vec![
                seg(g(), "  "),
                state_glyph(j.state),
                seg(d(), format!(" {}", j.name)),
            ])));
        }
    }
    rows.push(ci_hint_row());
    rows
}

/// The per-section key hints (the same keys the event loop dispatches, so they
/// can't drift): drill in, open in browser, re-run, cancel, refresh.
fn ci_hint_row() -> PanelRow {
    hint_row(&[
        ("↵", "view"),
        ("o", "browser"),
        ("r/R", "rerun"),
        ("c", "cancel"),
        ("g", "refresh"),
    ])
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{}…", cut.trim_end())
}

/// Full: the whole-band drilldown — a `CI RUNS` header, richer run rows (event +
/// duration), then the latest run's jobs **with their steps**. Distinct from the
/// Normal/Half list bodies (the panel's three-view contract).
fn full(ctx: &SectionCtx) -> Vec<PanelRow> {
    let data = &ctx.model.panel;
    let now = thegn_core::util::now();
    if data.ci_runs.is_empty() {
        return vec![
            PanelRow::plain(Line::segs(vec![seg(d(), "CI RUNS")])),
            PanelRow::plain(Line::segs(vec![seg(
                g(),
                data.ci_note.as_deref().unwrap_or("no CI runs"),
            )])),
        ];
    }
    let mut rows: Vec<PanelRow> = vec![];

    // Summary row: current state (latest run per workflow), not history.
    rows.push(PanelRow::plain(summary_line("CI RUNS ", data, now)));
    if let Some(note) = &data.ci_note {
        rows.push(PanelRow::plain(Line::segs(vec![seg(
            hue(Hue::Amber),
            format!("⚠ {note}"),
        )])));
    }

    for (i, r) in data.ci_runs.iter().take(ctx.rows.max(1)).enumerate() {
        let dur = r
            .duration_secs(now)
            .map(fmt_secs)
            .unwrap_or_else(|| "—".into());
        let mut left = vec![state_glyph(r.state), seg(d(), format!(" {}", r.name))];
        if !r.event.is_empty() {
            left.push(seg(g(), format!("  {}", r.event)));
        }
        if !r.branch.is_empty() {
            left.push(seg(g(), format!("  {}", r.branch)));
        }
        if !r.title.is_empty() {
            left.push(seg(g2(), format!("  {}", truncate(&r.title, 40))));
        }
        rows.push(
            PanelRow::plain(Line::split(left, vec![seg(g(), dur)]))
                .with_hit(PanelHit::Row(Section::Ci, i)),
        );
    }

    // Detailed view of the latest run's jobs and steps
    if let Some(latest) = data.ci_runs.first() {
        if !latest.jobs.is_empty() {
            rows.push(PanelRow::plain(Line::segs(vec![seg(g(), "")])));
            rows.push(PanelRow::plain(Line::segs(vec![seg(
                d(),
                "LATEST RUN JOBS",
            )])));
        }
        for j in &latest.jobs {
            let dur = j
                .duration_secs(now)
                .map(fmt_secs)
                .unwrap_or_else(|| "—".into());
            rows.push(PanelRow::plain(Line::split(
                vec![
                    seg(g(), "  "),
                    state_glyph(j.state),
                    seg(d(), format!(" {}", j.name)),
                ],
                vec![seg(g(), dur)],
            )));
            for s in &j.steps {
                rows.push(PanelRow::plain(Line::segs(vec![
                    seg(g(), "      "),
                    state_glyph(s.state),
                    seg(f(), format!(" {}", s.name)),
                ])));
            }
        }
    }
    rows.push(ci_hint_row());
    rows
}
