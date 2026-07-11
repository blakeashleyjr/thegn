//! The CI-badge modal's in-place run drill (AV group). Replaces the old
//! `RunCommand(["ci","view",<id>])` that spawned a one-shot `thegn ci view`
//! pane — that pane printed its output and exited instantly, reading as a crash.
//! Instead the overlay drills *in place*: the header paints from the cached run,
//! then an off-loop fetch fills jobs/steps + the failing-log tail via
//! [`apply_ci_detail`]. A child module of `detail`, so it reaches the private
//! `DetailOverlay` fields; split out only to keep `detail.rs` under the god-file
//! cap.

use super::{Cell, DetailContent, DetailOverlay, Section, SectionsDetail, TableSection};
use crate::chrome::S;
use crate::seg::Tok;
use thegn_core::theme::Hue;

/// The async result of a CI-run drill: the fully-fetched run (jobs/steps
/// populated) plus a failing-log tail, delivered back into the live overlay by
/// [`apply_ci_detail`]. Carried (boxed) over the loop's refresh channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CiDetailPayload {
    pub run: thegn_core::ci::CiRun,
    pub log_tail: Vec<String>,
}

/// Glyph + marker tone for a CI state — caps-routed so it degrades to ASCII.
/// Shared by the run list rows and the drilled job/step view.
pub(crate) fn ci_glyph_marker(state: thegn_core::ci::CiState) -> (&'static str, Tok) {
    use thegn_core::ci::CiState;
    let gl = crate::caps::active_glyphs();
    match state {
        CiState::Fail => (gl.cross, Tok::Hue(Hue::Red)),
        CiState::Running => (gl.dot_filled, Tok::Hue(Hue::Amber)),
        CiState::Pass => (gl.check, Tok::Hue(Hue::Green)),
        _ => (gl.middot, Tok::Slot(S::Dim)),
    }
}

/// A one-word outcome label for a CI state (row text + drill header).
pub(crate) fn ci_state_word(state: thegn_core::ci::CiState) -> &'static str {
    use thegn_core::ci::CiState;
    match state {
        CiState::Pending => "pending",
        CiState::Running => "running",
        CiState::Pass => "passed",
        CiState::Fail => "failed",
        CiState::Cancelled => "cancelled",
        CiState::Skipped => "skipped",
    }
}

/// Human duration for a CI run/job (`1m42s`, `9s`), from a `duration_secs`.
pub(crate) fn ci_fmt_secs(s: i64) -> String {
    if s >= 60 {
        format!("{}m{:02}s", s / 60, s % 60)
    } else {
        format!("{s}s")
    }
}

/// The drilled CI view's title: `CI ▸ <name> #<run>`.
fn ci_drill_title(run: &thegn_core::ci::CiRun) -> String {
    match run.run_number {
        Some(n) => format!("CI \u{25b8} {} #{n}", run.name),
        None => format!("CI \u{25b8} {}", run.name),
    }
}

/// The `key … value` header block for a drilled CI run, from the cached fields
/// (available before the async jobs/log fetch lands).
fn ci_header_section(run: &thegn_core::ci::CiRun) -> Section {
    let (_, marker) = ci_glyph_marker(run.state);
    let outcome = match run.conclusion_raw.as_deref() {
        Some(c) if !c.is_empty() => format!("{} ({c})", ci_state_word(run.state)),
        _ => ci_state_word(run.state).to_string(),
    };
    let mut pairs: Vec<(String, String, Tok)> = vec![("state".into(), outcome, marker)];
    if !run.title.is_empty() {
        pairs.push(("title".into(), run.title.clone(), Tok::Slot(S::Text)));
    }
    if !run.event.is_empty() {
        pairs.push(("event".into(), run.event.clone(), Tok::Slot(S::Dim)));
    }
    if !run.branch.is_empty() {
        pairs.push(("branch".into(), run.branch.clone(), Tok::Slot(S::Dim)));
    }
    if !run.sha.is_empty() {
        let sha = run.sha.chars().take(9).collect::<String>();
        pairs.push(("commit".into(), sha, Tok::Slot(S::Dim)));
    }
    if let Some(secs) = run.duration_secs(thegn_core::util::now()) {
        pairs.push(("duration".into(), ci_fmt_secs(secs), Tok::Slot(S::Dim)));
    }
    if !run.url.is_empty() {
        pairs.push(("url".into(), run.url.clone(), Tok::Slot(S::Ghost)));
    }
    Section::KeyVal(pairs)
}

impl DetailOverlay {
    /// Swap this overlay's content in place for a CI run's detail: the header
    /// (paints instantly from the cached run) plus a "fetching jobs…" placeholder
    /// that [`DetailOverlay::set_ci_detail`] replaces once the off-loop fetch
    /// lands. Records `pending_ci` so a late/stale result for a different run is
    /// dropped.
    pub(crate) fn enter_ci_view(&mut self, run: &thegn_core::ci::CiRun) {
        self.content = DetailContent::Sections(SectionsDetail {
            sections: vec![
                ci_header_section(run),
                Section::Heading {
                    label: "fetching jobs\u{2026}".into(),
                    note: None,
                },
            ],
        });
        self.title = ci_drill_title(run);
        self.cols = 76;
        self.rows = self.content_rows().clamp(3, 22);
        self.hint = None;
        self.scroll = 0;
        self.sel = 0;
        self.pending_ci = Some(run.id.clone());
        self.live_ci = None;
    }

    /// The drilled run to re-poll while it's still in flight (live drill
    /// updates, ridden by the loop's CI tick): re-arms `pending_ci` so the
    /// refreshed fill lands, and returns the run for the off-loop fetch.
    /// `None` when the drill is closed, a fetch is already in flight, or the
    /// run has reached a terminal state.
    pub(crate) fn live_ci_repoll(&mut self) -> Option<thegn_core::ci::CiRun> {
        if self.pending_ci.is_some() {
            return None;
        }
        let run = self.live_ci.clone()?;
        self.pending_ci = Some(run.id.clone());
        Some(run)
    }

    /// Replace the drilled CI view with the fully-fetched run: header + per-job
    /// headings, each job's steps as a table (first failure highlighted), and a
    /// failing-log tail. Clears `pending_ci` (the fetch is done).
    pub(crate) fn set_ci_detail(&mut self, run: &thegn_core::ci::CiRun, log_tail: Vec<String>) {
        let now = thegn_core::util::now();
        let mut secs = vec![ci_header_section(run)];
        if run.jobs.is_empty() {
            secs.push(Section::Heading {
                label: "no job detail".into(),
                note: None,
            });
        } else {
            secs.push(Section::Heading {
                label: "jobs".into(),
                note: None,
            });
            for job in &run.jobs {
                let (g, _) = ci_glyph_marker(job.state);
                secs.push(Section::Heading {
                    label: format!("{g} {}", job.name),
                    note: job.duration_secs(now).map(ci_fmt_secs),
                });
                if !job.steps.is_empty() {
                    let rows: Vec<Vec<Cell>> = job
                        .steps
                        .iter()
                        .map(|s| {
                            let (sg, stone) = ci_glyph_marker(s.state);
                            vec![
                                Cell::Text(format!("  {sg}"), stone),
                                Cell::Text(s.name.clone(), Tok::Slot(S::Dim)),
                            ]
                        })
                        .collect();
                    secs.push(Section::Table(TableSection {
                        header: Vec::new(),
                        rows,
                    }));
                }
            }
        }
        if !log_tail.is_empty() {
            secs.push(Section::Heading {
                label: "log tail".into(),
                note: None,
            });
            let rows: Vec<Vec<Cell>> = log_tail
                .iter()
                .map(|line| {
                    let tone = if thegn_core::ci::line_is_failure(line) {
                        Tok::Hue(Hue::Red)
                    } else {
                        Tok::Slot(S::Dim)
                    };
                    vec![Cell::Text(line.clone(), tone)]
                })
                .collect();
            secs.push(Section::Table(TableSection {
                header: Vec::new(),
                rows,
            }));
        }
        self.title = ci_drill_title(run);
        self.content = DetailContent::Sections(SectionsDetail { sections: secs });
        self.rows = self.content_rows().clamp(3, 22);
        // A live re-fill of the same in-flight run keeps the user's scroll
        // position; a fresh drill starts at the top.
        if self.live_ci.as_ref().is_none_or(|r| r.id != run.id) {
            self.scroll = 0;
        }
        self.pending_ci = None;
        // Keep re-polling until the run settles (live drill updates).
        self.live_ci = (!run.state.is_terminal()).then(|| run.clone());
    }
}

/// Deliver an async CI-drill result into the live overlay, iff it's still the
/// overlay drilling that exact run (the user may have closed the modal or
/// drilled a different run in the meantime). Called from the loop when a
/// [`CiDetailPayload`] arrives on the refresh channel; returns `true` when it
/// filled the overlay, so the loop can mark the frame dirty to repaint it.
pub fn apply_ci_detail(slot: &mut Option<DetailOverlay>, payload: CiDetailPayload) -> bool {
    if let Some(ov) = slot.as_mut()
        && ov.pending_ci.as_deref() == Some(payload.run.id.as_str())
    {
        ov.set_ci_detail(&payload.run, payload.log_tail);
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chrome::{BarBadge, BarItemId, FrameModel};
    use crate::compositor::Rect;
    use crate::detail::{DetailAction, DetailContent, DetailOutcome, open_detail_for};
    use crate::telemetry::TelemetryHistory;
    use termwiz::input::{KeyCode, Modifiers};

    fn screen() -> Rect {
        Rect {
            x: 0,
            y: 0,
            cols: 120,
            rows: 40,
        }
    }

    fn item_at(y: usize) -> Rect {
        Rect {
            x: 80,
            y,
            cols: 8,
            rows: 1,
        }
    }

    #[test]
    fn ci_rows_are_enriched_and_drill_fills_in_place() {
        use thegn_core::ci::{CiJob, CiRun, CiState, CiStep};
        let run = CiRun {
            id: "42".into(),
            name: "CI".into(),
            title: "fix: DNS".into(),
            event: "push".into(),
            branch: "main".into(),
            run_number: Some(128),
            state: CiState::Fail,
            url: "https://example/42".into(),
            ..Default::default()
        };
        let model = FrameModel {
            panel: crate::panel::PanelData {
                ci_runs: vec![run.clone()],
                ..Default::default()
            },
            ..Default::default()
        };
        let mut ov = open_detail_for(
            &BarItemId::Badge(BarBadge::Ci),
            item_at(39),
            screen(),
            &model,
            &TelemetryHistory::default(),
        )
        .expect("ci badge opens");
        // The row text carries name + outcome + title; the note carries the
        // context (run #, event, branch) that used to be hidden behind the glyph.
        let DetailContent::List(l) = &ov.content else {
            panic!("expected a list");
        };
        assert!(l.rows[0].text.contains("CI"));
        assert!(l.rows[0].text.contains("failed"));
        assert!(l.rows[0].text.contains("fix: DNS"));
        let note = l.rows[0].note.as_deref().unwrap_or_default();
        assert!(note.contains("#128"), "note missing run #: {note:?}");
        assert!(note.contains("push") && note.contains("main"));
        // `o` still opens the run URL from the list.
        assert_eq!(
            ov.action_for('o'),
            Some(DetailAction::OpenUrl("https://example/42".into()))
        );
        // Enter drills IN PLACE: it returns DrillCiRun (so the loop kicks the
        // off-loop fetch) and swaps the overlay to the run header — no pane spawns.
        match ov.handle_key(&KeyCode::Enter, Modifiers::NONE) {
            DetailOutcome::Act(DetailAction::DrillCiRun { run }) => assert_eq!(run.id, "42"),
            other => panic!("expected in-place drill, got {other:?}"),
        }
        assert!(matches!(ov.content, DetailContent::Sections(_)));
        assert_eq!(ov.pending_ci.as_deref(), Some("42"));

        // A stale result for a different run is ignored (user drilled elsewhere).
        let mut slot = Some(ov);
        apply_ci_detail(
            &mut slot,
            CiDetailPayload {
                run: CiRun {
                    id: "999".into(),
                    ..run.clone()
                },
                log_tail: vec![],
            },
        );
        assert_eq!(
            slot.as_ref().and_then(|o| o.pending_ci.as_deref()),
            Some("42"),
            "stale fill must not land"
        );

        // The matching async detail fills the same overlay in place.
        let filled = CiRun {
            jobs: vec![CiJob {
                id: "j1".into(),
                name: "build".into(),
                state: CiState::Fail,
                steps: vec![CiStep {
                    name: "cargo test".into(),
                    state: CiState::Fail,
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..run.clone()
        };
        apply_ci_detail(
            &mut slot,
            CiDetailPayload {
                run: filled,
                log_tail: vec!["error: boom".into()],
            },
        );
        let ov = slot.expect("overlay retained after fill");
        let DetailContent::Sections(d) = &ov.content else {
            panic!("expected sections after fill");
        };
        // header + "jobs" + build heading + steps table + "log tail" + log table.
        assert!(d.sections.len() >= 5, "sparse fill: {}", d.sections.len());
        assert_eq!(ov.pending_ci, None, "pending cleared after fill");
    }

    #[test]
    fn live_drill_repolls_until_terminal() {
        use thegn_core::ci::{CiRun, CiState};
        let running = CiRun {
            id: "7".into(),
            name: "CI".into(),
            state: CiState::Running,
            ..Default::default()
        };
        let model = FrameModel {
            panel: crate::panel::PanelData {
                ci_runs: vec![running.clone()],
                ..Default::default()
            },
            ..Default::default()
        };
        let mut ov = open_detail_for(
            &BarItemId::Badge(BarBadge::Ci),
            item_at(39),
            screen(),
            &model,
            &TelemetryHistory::default(),
        )
        .expect("ci badge opens");
        ov.enter_ci_view(&running);
        // While the first fetch is in flight, no repoll piles on.
        assert!(ov.live_ci_repoll().is_none());
        // A fill that's still running arms the live repoll…
        ov.set_ci_detail(&running, vec![]);
        ov.scroll = 3;
        let again = ov.live_ci_repoll().expect("running run repolls");
        assert_eq!(again.id, "7");
        assert_eq!(ov.pending_ci.as_deref(), Some("7"), "pending re-armed");
        // …a live re-fill of the same run preserves the scroll position…
        ov.set_ci_detail(&running, vec![]);
        assert_eq!(ov.scroll, 3);
        // …and a terminal fill stops the polling.
        let done = CiRun {
            state: CiState::Pass,
            ..running
        };
        ov.set_ci_detail(&done, vec![]);
        assert!(
            ov.live_ci_repoll().is_none(),
            "terminal run stops repolling"
        );
    }

    #[test]
    fn only_the_ci_drill_keeps_the_overlay() {
        use thegn_core::ci::CiRun;
        assert!(
            DetailAction::DrillCiRun {
                run: Box::new(CiRun::default())
            }
            .keeps_overlay()
        );
        assert!(!DetailAction::OpenUrl("x".into()).keeps_overlay());
        assert!(!DetailAction::ClearNotifications.keeps_overlay());
    }
}
