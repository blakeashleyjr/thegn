//! The CI section (AV group): recent runs with a per-run state glyph + duration,
//! expanding the cursor run's jobs when the band is deep. Reads
//! `model.panel.ci_runs` (populated from the `ci_runs_cache`). The full
//! drilldown is the CLI (`thegn ci view <id>`); this is the at-a-glance
//! "is the pipeline green?" rollup, the CI-provider analogue of the PR checks.

use thegn_core::ci::{CiState, current_summary};
use thegn_core::pipeline_report::{display_cell, display_clip};
use thegn_core::theme::Hue;

use crate::seg::{Line, Seg, seg};

use super::{
    PanelHit, PanelRow, Section, SectionCtx, d, f, fmt_secs, g, g2, hint_row, hue, list_detail,
};

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

/// The runs the section actually renders: one row per workflow, judged by its
/// most recent run ([`thegn_core::ci::latest_per_workflow`]) — the same set
/// [`current_summary`] counts, so the header numbers and the visible rows can
/// never disagree. History stays reachable through the drill-in (`↵`/`v`).
/// The event-loop dispatch (`actions::CiActionCtx`) resolves the row cursor
/// through this same list so Enter/`o`/`r`/`c` hit the run the user sees.
pub(crate) fn display_runs(panel: &crate::panel::PanelData) -> Vec<&thegn_core::ci::CiRun> {
    thegn_core::ci::latest_per_workflow(&panel.ci_runs)
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

/// Normal/Half: recent runs, expanding the cursor run's jobs in the deep (Half) view.
fn list(ctx: &SectionCtx) -> Vec<PanelRow> {
    let data = &ctx.model.panel;
    let now = thegn_core::util::now();
    if data.ci_runs.is_empty() {
        return vec![PanelRow::plain(Line::segs(vec![seg(
            d(),
            display_cell(data.ci_note.as_deref().unwrap_or("no CI runs")),
        )]))];
    }
    let mut rows: Vec<PanelRow> = Vec::new();

    // Summary row: current state (latest run per workflow), not history,
    // stamped with the cache's fetch age so stale data is visibly stale.
    rows.push(PanelRow::plain(summary_line("CI ", data, now)));
    if let Some(note) = &data.ci_note {
        rows.push(PanelRow::plain(Line::segs(vec![seg(
            hue(Hue::Amber),
            format!("⚠ {}", display_cell(note)),
        )])));
    }

    // One row per workflow (the set the summary counts). Each run row carries
    // a `Row` hit (the summary/job rows don't), so the enumerate index lines
    // up with `ui.cursor` and with `display_runs` — which the dispatch also
    // resolves through.
    let display = display_runs(data);
    for (i, r) in display.iter().enumerate() {
        let dur = r
            .duration_secs(now)
            .map(fmt_secs)
            .unwrap_or_else(|| "—".into());
        let mut left = vec![
            state_glyph(r.state),
            seg(d(), format!(" {}", display_cell(&r.name))),
        ];
        if !r.branch.is_empty() {
            left.push(seg(g(), format!("  {}", display_cell(&r.branch))));
        }
        rows.push(
            PanelRow::plain(Line::split(left, vec![seg(g(), dur)]))
                .with_hit(PanelHit::Row(Section::Ci, i)),
        );
    }
    // Deep (Half): expand the cursor run's jobs.
    if ctx.deep()
        && let Some(latest) = display.get(ctx.ui.cursor)
    {
        for j in &latest.jobs {
            rows.push(PanelRow::plain(Line::segs(vec![
                seg(g(), "  "),
                state_glyph(j.state),
                seg(d(), format!(" {}", display_cell(&j.name))),
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

/// Full: the whole-band drilldown — a `CI RUNS` header, richer run rows (event +
/// duration), then the cursor run's jobs **with their steps**. Distinct from the
/// Normal/Half list bodies (the panel's three-view contract).
fn full(ctx: &SectionCtx) -> Vec<PanelRow> {
    let data = &ctx.model.panel;
    let now = thegn_core::util::now();
    if data.ci_runs.is_empty() {
        return vec![
            PanelRow::plain(Line::segs(vec![seg(d(), "CI RUNS")])),
            PanelRow::plain(Line::segs(vec![seg(
                g(),
                display_cell(data.ci_note.as_deref().unwrap_or("no CI runs")),
            )])),
        ];
    }
    let mut rows = vec![PanelRow::plain(summary_line("CI RUNS ", data, now))];
    if let Some(note) = &data.ci_note {
        rows.push(PanelRow::plain(Line::segs(vec![seg(
            hue(Hue::Amber),
            format!("⚠ {}", display_cell(note)),
        )])));
    }
    let display = display_runs(data);
    let list_w = 45_usize.min(ctx.cols / 2);
    let list: Vec<(Vec<Seg>, Option<usize>)> = display
        .iter()
        .enumerate()
        .map(|(i, run)| {
            let dur = run
                .duration_secs(now)
                .map(fmt_secs)
                .unwrap_or_else(|| "—".into());
            let mut left = vec![
                seg(
                    if i == ctx.ui.cursor { g2() } else { g() },
                    if i == ctx.ui.cursor { "▶ " } else { "  " },
                ),
                state_glyph(run.state),
                seg(d(), format!(" {}", display_cell(&run.name))),
            ];
            if !run.event.is_empty() {
                left.push(seg(g(), format!("  {}", display_cell(&run.event))));
            }
            if !run.branch.is_empty() {
                left.push(seg(g(), format!("  {}", display_cell(&run.branch))));
            }
            if !run.title.is_empty() {
                left.push(seg(g2(), format!("  {}", display_clip(&run.title, 40))));
            }
            left.push(seg(g(), format!("  {dur}")));
            (left, Some(i))
        })
        .collect();
    let mut detail: Vec<Vec<Seg>> = Vec::new();
    if let Some(run) = display.get(ctx.ui.cursor) {
        if !run.jobs.is_empty() {
            detail.push(vec![seg(d(), "RUN JOBS")]);
        }
        for job in &run.jobs {
            let dur = job
                .duration_secs(now)
                .map(fmt_secs)
                .unwrap_or_else(|| "—".into());
            detail.push(vec![
                seg(g(), "  "),
                state_glyph(job.state),
                seg(d(), format!(" {}  {dur}", display_cell(&job.name))),
            ]);
            for step in &job.steps {
                detail.push(vec![
                    seg(g(), "      "),
                    state_glyph(step.state),
                    seg(f(), format!(" {}", display_cell(&step.name))),
                ]);
            }
        }
    }
    rows.extend(list_detail(
        Section::Ci,
        &list,
        &detail,
        ctx.ui.cursor,
        list_w,
        ctx.rows.saturating_sub(rows.len() + 1),
    ));
    rows.push(ci_hint_row());
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chrome::FrameModel;
    use crate::panel::PanelUi;
    use thegn_core::ci::CiRun;

    #[test]
    fn hostile_ci_run_fields_render_without_terminal_controls() {
        let mut model = FrameModel::default();
        model.panel.ci_runs = vec![CiRun {
            name: "\u{1b}]0;pwn\u{7}日本語".repeat(12),
            title: "\u{202e}title日本語".repeat(12),
            ..Default::default()
        }];
        let ui = PanelUi::default();
        let rows = content(&SectionCtx {
            model: &model,
            ui: &ui,
            cols: 39,
            rows: 20,
        });
        assert!(rows.iter().all(|row| {
            match &row.line {
                Line::Segs(segs) => segs.iter().all(|seg| {
                    seg.text.chars().all(|c| {
                        !c.is_control() && !thegn_core::pipeline_report::is_format_control(c)
                    })
                }),
                Line::Split { l, r } => l.iter().chain(r).all(|seg| {
                    seg.text.chars().all(|c| {
                        !c.is_control() && !thegn_core::pipeline_report::is_format_control(c)
                    })
                }),
                Line::SplitMinLeft { l, r, .. } => l.iter().chain(r).all(|seg| {
                    seg.text.chars().all(|c| {
                        !c.is_control() && !thegn_core::pipeline_report::is_format_control(c)
                    })
                }),
                Line::Blank | Line::Fill { .. } => true,
            }
        }));
    }
}
