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
    /// The worktree the fetch ran against; a drill fills only from its own worktree.
    pub origin: std::path::PathBuf,
    pub discarded_jobs: usize,
    pub incomplete: Option<String>,
    /// Why the full run detail could not be fetched; the drill then shows the cached run.
    pub detail_error: Option<String>,
    pub log_tail: Vec<String>,
    /// Cache metadata for each displayed failed-job excerpt.  The text is
    /// already bounded and redacted by the producer.
    pub log_entries: Vec<thegn_core::ci_log::CiLogEntry>,
}

/// The CI drill fetch in flight: which run, and the worktree it is fetched
/// against. `origin` is None until the caller that spawns the fetch binds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CiPending {
    pub(crate) run_id: String,
    pub(crate) origin: Option<std::path::PathBuf>,
}

/// A drilled run still in flight and the worktree its fills came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LiveCi {
    pub(crate) run: thegn_core::ci::CiRun,
    pub(crate) origin: std::path::PathBuf,
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
    let arrow = crate::caps::active_glyphs().arrow_right;
    match run.run_number {
        Some(n) => format!("CI {arrow} {} #{n}", run.name),
        None => format!("CI {arrow} {}", run.name),
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
        self.pending_ci = Some(CiPending {
            run_id: run.id.clone(),
            origin: None,
        });
        self.live_ci = None;
        self.ci_autofix = None;
    }

    /// Bind the outstanding drill fetch to the worktree its worker will read.
    pub(crate) fn bind_ci_origin(&mut self, run_id: &str, origin: &std::path::Path) -> bool {
        if let Some(pending) = self.pending_ci.as_mut()
            && pending.run_id == run_id
        {
            pending.origin = Some(origin.to_path_buf());
            return true;
        }
        false
    }

    /// The drilled run to re-poll while it's still in flight (live drill
    /// updates, ridden by the loop's CI tick): re-arms `pending_ci` so the
    /// refreshed fill lands, and returns the run and its origin for the fetch.
    /// `None` when the drill is closed, a fetch is already in flight, or the
    /// run has reached a terminal state.
    pub(crate) fn live_ci_repoll(&mut self) -> Option<(thegn_core::ci::CiRun, std::path::PathBuf)> {
        if self.pending_ci.is_some() {
            return None;
        }
        let live = self.live_ci.clone()?;
        self.pending_ci = Some(CiPending {
            run_id: live.run.id.clone(),
            origin: Some(live.origin.clone()),
        });
        Some((live.run, live.origin))
    }

    /// Replace the drilled CI view with the fully-fetched run: header + per-job
    /// headings, each job's steps as a table (first failure highlighted), and a
    /// failing-log tail. Clears `pending_ci` (the fetch is done).
    pub(crate) fn set_ci_detail(
        &mut self,
        run: &thegn_core::ci::CiRun,
        discarded_jobs: usize,
        log_tail: Vec<String>,
        log_entries: Vec<thegn_core::ci_log::CiLogEntry>,
        incomplete: Option<&str>,
        detail_error: Option<&str>,
    ) {
        let origin = self.pending_ci.take().and_then(|pending| pending.origin);
        let now = thegn_core::util::now();
        let mut secs = vec![ci_header_section(run)];
        let mut note_parts = Vec::new();
        if let Some(error) = detail_error {
            note_parts.push(format!(
                "job detail unavailable: {error}; showing the cached run"
            ));
        }
        if discarded_jobs > 0 {
            note_parts.push(format!("{discarded_jobs} malformed job row(s) omitted"));
        }
        if let Some(reason) = incomplete {
            note_parts.push(format!("job list incomplete: {reason}"));
        }
        let note = (!note_parts.is_empty()).then(|| note_parts.join("; "));
        if run.jobs.is_empty() {
            secs.push(Section::Heading {
                label: "no job detail".into(),
                note: note.clone(),
            });
        } else {
            secs.push(Section::Heading {
                label: "jobs".into(),
                note,
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
                        sel: None,
                    }));
                }
            }
        }
        if !log_tail.is_empty() || !log_entries.is_empty() {
            secs.push(Section::Heading {
                label: "log tail".into(),
                note: Some(
                    if log_entries.iter().any(|entry| {
                        !entry.text.trim().is_empty() && !entry.head_sha.trim().is_empty()
                    }) && incomplete.is_none()
                    {
                        format!("{} · f authorize fix", log_metadata(&log_entries))
                    } else {
                        log_metadata(&log_entries)
                    },
                ),
            });
            let lines: Vec<String> = if log_entries.is_empty() {
                log_tail
            } else {
                log_entries
                    .iter()
                    .flat_map(|entry| {
                        let hline = crate::caps::active_glyphs().box_h;
                        let heading = format!("{hline}{hline} {} {hline}{hline}", entry.job_name);
                        std::iter::once(heading)
                            .chain(entry.text.lines().map(str::to_string))
                            .collect::<Vec<_>>()
                    })
                    .collect()
            };
            let rows: Vec<Vec<Cell>> = lines
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
                sel: None,
            }));
        }
        self.title = ci_drill_title(run);
        self.content = DetailContent::Sections(SectionsDetail { sections: secs });
        self.rows = self.content_rows().clamp(3, 22);
        // A live re-fill of the same in-flight run keeps the user's scroll
        // position; a fresh drill starts at the top.
        if self
            .live_ci
            .as_ref()
            .is_none_or(|live| live.run.id != run.id)
        {
            self.scroll = 0;
        }
        // A single `f` action authorizes the first available failed-job entry;
        // its complete candidate identity is carried into the blocking worker.
        // Entries without text or a head SHA fail closed and never become UI
        // actions.
        self.ci_autofix = if incomplete.is_some() || detail_error.is_some() {
            Some(super::CiAutofixAvailability::Refused(
                super::DetailRefusal::IncompleteCiJobList,
            ))
        } else {
            log_entries
                .iter()
                .find(|entry| !entry.text.trim().is_empty() && !entry.head_sha.trim().is_empty())
                .map(|entry| {
                    super::CiAutofixAvailability::Available(super::DetailAction::CiAutofix {
                        candidate: entry.candidate(),
                    })
                })
        };
        // Keep re-polling until the run settles (live drill updates).
        self.live_ci = if run.state.is_terminal() {
            None
        } else {
            origin.map(|origin| LiveCi {
                run: run.clone(),
                origin,
            })
        };
    }
}

fn log_metadata(entries: &[thegn_core::ci_log::CiLogEntry]) -> String {
    let truncated = entries.iter().filter(|e| e.truncated).count();
    let redacted = entries.iter().filter(|e| e.redacted).count();
    let source = if entries.is_empty() {
        "fallback"
    } else {
        "cached"
    };
    let mut note = source.to_string();
    if truncated > 0 {
        note.push_str(" · tail/truncated");
    }
    if redacted > 0 || !entries.is_empty() {
        note.push_str(" · redacted");
    }
    note
}

/// Build an in-place CI detail overlay for a panel row.  The initial header is
/// painted immediately; the caller starts the blocking fill afterwards.
pub(crate) fn overlay_for_run(run: &thegn_core::ci::CiRun) -> DetailOverlay {
    let mut overlay = super::list("CI run", Vec::new(), "fetching jobs…", 76, 12);
    overlay.enter_ci_view(run);
    overlay
}

/// Deliver an async CI-drill result into the live overlay, iff it's still the
/// overlay drilling that exact run (the user may have closed the modal or
/// drilled a different run in the meantime). Called from the loop when a
/// [`CiDetailPayload`] arrives on the refresh channel; returns `true` when it
/// filled the overlay, so the loop can mark the frame dirty to repaint it.
pub fn apply_ci_detail(slot: &mut Option<DetailOverlay>, payload: CiDetailPayload) -> bool {
    if let Some(ov) = slot.as_mut()
        && ov.pending_ci.as_ref().is_some_and(|pending| {
            pending.run_id == payload.run.id
                && pending.origin.as_deref() == Some(payload.origin.as_path())
        })
    {
        ov.set_ci_detail(
            &payload.run,
            payload.discarded_jobs,
            payload.log_tail,
            payload.log_entries,
            payload.incomplete.as_deref(),
            payload.detail_error.as_deref(),
        );
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chrome::{BarBadge, BarItemId, FrameModel};
    use crate::compositor::Rect;
    use crate::detail::{DetailAction, DetailContent, DetailOutcome, StatusCtx, open_detail_for};
    use crate::telemetry::TelemetryHistory;
    use std::path::{Path, PathBuf};
    use termwiz::input::{KeyCode, Modifiers};

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
            &model,
            &StatusCtx::new_for_test(&TelemetryHistory::default()),
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
        assert_eq!(
            ov.pending_ci
                .as_ref()
                .map(|pending| pending.run_id.as_str()),
            Some("42")
        );
        assert!(ov.bind_ci_origin("42", Path::new("/wt/repo")));

        // A stale result for a different run is ignored (user drilled elsewhere).
        let mut slot = Some(ov);
        apply_ci_detail(
            &mut slot,
            CiDetailPayload {
                run: CiRun {
                    id: "999".into(),
                    ..run.clone()
                },
                origin: "/wt/repo".into(),
                discarded_jobs: 0,
                incomplete: None,
                detail_error: None,
                log_tail: vec![],
                log_entries: vec![],
            },
        );
        assert_eq!(
            slot.as_ref()
                .and_then(|o| o.pending_ci.as_ref().map(|pending| pending.run_id.as_str())),
            Some("42"),
            "stale fill must not land"
        );

        // The matching async detail fills the same overlay in place.
        let filled = CiRun {
            sha: "abc123".into(),
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
                origin: "/wt/repo".into(),
                discarded_jobs: 1,
                incomplete: None,
                detail_error: None,
                log_tail: vec!["error: boom".into()],
                log_entries: vec![thegn_core::ci_log::CiLogEntry {
                    worktree: "/wt/repo".into(),
                    run_id: "42".into(),
                    job_id: "j1".into(),
                    job_name: "build".into(),
                    text: "error: boom\n".into(),
                    truncated: false,
                    redacted: true,
                    policy_version: thegn_core::ci_log::REDACTION_POLICY_VERSION,
                    fetched_at: 7,
                    head_sha: "abc123".into(),
                }],
            },
        );
        let mut ov = slot.expect("overlay retained after fill");
        let DetailContent::Sections(d) = &ov.content else {
            panic!("expected sections after fill");
        };
        // header + "jobs" + build heading + steps table + "log tail" + log table.
        assert!(d.sections.len() >= 5, "sparse fill: {}", d.sections.len());
        assert!(d.sections.iter().any(|section| matches!(
            section,
            Section::Heading { label, note: Some(note) }
                if label == "jobs" && note.contains("1 malformed job row")
        )));
        assert_eq!(ov.pending_ci, None, "pending cleared after fill");
        assert_eq!(
            ov.handle_key(&KeyCode::Char('f'), Modifiers::NONE),
            DetailOutcome::Act(DetailAction::CiAutofix {
                candidate: thegn_core::ci_log::CiLogCandidate {
                    worktree: "/wt/repo".into(),
                    run_id: "42".into(),
                    job_id: "j1".into(),
                    head_sha: "abc123".into(),
                },
            })
        );
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
            &model,
            &StatusCtx::new_for_test(&TelemetryHistory::default()),
        )
        .expect("ci badge opens");
        ov.enter_ci_view(&running);
        assert!(ov.bind_ci_origin("7", Path::new("/wt/repo")));
        // While the first fetch is in flight, no repoll piles on.
        assert!(ov.live_ci_repoll().is_none());
        // A fill that's still running arms the live repoll…
        ov.set_ci_detail(&running, 0, vec![], vec![], None, None);
        ov.scroll = 3;
        let (again, origin) = ov.live_ci_repoll().expect("running run repolls");
        assert_eq!(again.id, "7");
        assert_eq!(origin, PathBuf::from("/wt/repo"));
        assert_eq!(
            ov.pending_ci
                .as_ref()
                .map(|pending| pending.run_id.as_str()),
            Some("7"),
            "pending re-armed"
        );
        // …a live re-fill of the same run preserves the scroll position…
        ov.set_ci_detail(&running, 0, vec![], vec![], None, None);
        assert_eq!(ov.scroll, 3);
        // …and a terminal fill stops the polling.
        let done = CiRun {
            state: CiState::Pass,
            ..running
        };
        ov.set_ci_detail(&done, 0, vec![], vec![], None, None);
        assert!(
            ov.live_ci_repoll().is_none(),
            "terminal run stops repolling"
        );
    }

    #[test]
    fn late_detail_from_another_worktree_is_dropped() {
        let run = thegn_core::ci::CiRun {
            id: "42".into(),
            ..Default::default()
        };
        let mut overlay = overlay_for_run(&run);
        assert!(overlay.bind_ci_origin("42", Path::new("/wt/b")));
        let mut slot = Some(overlay);
        let payload = |origin: &str| CiDetailPayload {
            run: run.clone(),
            origin: origin.into(),
            discarded_jobs: 0,
            incomplete: None,
            detail_error: None,
            log_tail: Vec::new(),
            log_entries: Vec::new(),
        };

        assert!(!apply_ci_detail(&mut slot, payload("/wt/a")));
        assert_eq!(
            slot.as_ref()
                .and_then(|overlay| overlay.pending_ci.as_ref())
                .map(|pending| pending.run_id.as_str()),
            Some("42"),
            "foreign-origin detail leaves the drill pending"
        );
        assert!(apply_ci_detail(&mut slot, payload("/wt/b")));
    }

    #[test]
    fn unbound_drill_accepts_no_detail() {
        let run = thegn_core::ci::CiRun {
            id: "42".into(),
            ..Default::default()
        };
        let overlay = overlay_for_run(&run);
        let expected_pending = overlay.pending_ci.clone();
        let mut slot = Some(overlay);
        let payload = CiDetailPayload {
            run,
            origin: "/wt/any".into(),
            discarded_jobs: 0,
            incomplete: None,
            detail_error: None,
            log_tail: Vec::new(),
            log_entries: Vec::new(),
        };

        assert!(!apply_ci_detail(&mut slot, payload));
        assert_eq!(
            slot.as_ref()
                .and_then(|overlay| overlay.pending_ci.as_ref()),
            expected_pending.as_ref(),
            "unbound drill keeps its pending identity"
        );
    }

    #[test]
    fn live_repoll_keeps_the_drill_origin() {
        let run = thegn_core::ci::CiRun {
            id: "7".into(),
            state: thegn_core::ci::CiState::Running,
            ..Default::default()
        };
        let mut overlay = overlay_for_run(&run);
        assert!(overlay.bind_ci_origin("7", Path::new("/wt/a")));
        let mut slot = Some(overlay);
        assert!(apply_ci_detail(
            &mut slot,
            CiDetailPayload {
                run: run.clone(),
                origin: "/wt/a".into(),
                discarded_jobs: 0,
                incomplete: None,
                detail_error: None,
                log_tail: Vec::new(),
                log_entries: Vec::new(),
            }
        ));
        let overlay = slot.as_mut().expect("overlay remains after fill");
        let (again, origin) = overlay.live_ci_repoll().expect("running run repolls");
        assert_eq!(again.id, "7");
        assert_eq!(origin, PathBuf::from("/wt/a"));
        assert_eq!(
            overlay.pending_ci,
            Some(CiPending {
                run_id: "7".into(),
                origin: Some(PathBuf::from("/wt/a")),
            })
        );
    }

    #[test]
    fn incomplete_job_list_note_is_shown() {
        use thegn_core::ci::{CiJob, CiRun, CiState};
        let run = CiRun {
            id: "42".into(),
            name: "CI".into(),
            state: CiState::Running,
            jobs: vec![CiJob {
                id: "job-1".into(),
                name: "build".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let model = FrameModel {
            panel: crate::panel::PanelData {
                ci_runs: vec![run.clone()],
                ..Default::default()
            },
            ..Default::default()
        };
        let mut overlay = open_detail_for(
            &BarItemId::Badge(BarBadge::Ci),
            item_at(39),
            &model,
            &StatusCtx::new_for_test(&TelemetryHistory::default()),
        )
        .expect("ci badge opens");
        assert!(matches!(
            overlay.handle_key(&KeyCode::Enter, Modifiers::NONE),
            DetailOutcome::Act(DetailAction::DrillCiRun { .. })
        ));
        assert!(overlay.bind_ci_origin("42", Path::new("/wt/repo")));
        let mut slot = Some(overlay);
        assert!(apply_ci_detail(
            &mut slot,
            CiDetailPayload {
                run,
                origin: "/wt/repo".into(),
                discarded_jobs: 0,
                incomplete: Some("job list stopped after 50 pages (page budget)".into()),
                detail_error: None,
                log_tail: Vec::new(),
                log_entries: vec![thegn_core::ci_log::CiLogEntry {
                    worktree: "/wt/repo".into(),
                    run_id: "42".into(),
                    job_id: "j1".into(),
                    job_name: "build".into(),
                    text: "error: boom\n".into(),
                    truncated: false,
                    redacted: true,
                    policy_version: thegn_core::ci_log::REDACTION_POLICY_VERSION,
                    fetched_at: 7,
                    head_sha: "abc123".into(),
                }],
            }
        ));
        let mut overlay = slot.expect("overlay remains");
        let DetailContent::Sections(detail) = &overlay.content else {
            panic!("expected sections");
        };
        let Some(Section::Heading { label, note }) = detail.sections.get(1) else {
            panic!("expected jobs heading");
        };
        assert_eq!(label, "jobs");
        assert!(
            note.as_deref()
                .unwrap_or_default()
                .contains("job list incomplete")
        );
        assert!(
            crate::detail::DetailRefusal::IncompleteCiJobList
                .notice()
                .contains("job list is incomplete")
        );
        assert_eq!(
            overlay.handle_key(&KeyCode::Char('f'), Modifiers::NONE),
            DetailOutcome::Refused(crate::detail::DetailRefusal::IncompleteCiJobList)
        );
    }

    #[test]
    fn detail_error_note_is_shown_and_autofix_refused() {
        use thegn_core::ci::{CiJob, CiRun, CiState};
        let run = CiRun {
            id: "42".into(),
            name: "CI".into(),
            state: CiState::Running,
            jobs: vec![CiJob {
                id: "job-1".into(),
                name: "build".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let model = FrameModel {
            panel: crate::panel::PanelData {
                ci_runs: vec![run.clone()],
                ..Default::default()
            },
            ..Default::default()
        };
        let mut overlay = open_detail_for(
            &BarItemId::Badge(BarBadge::Ci),
            item_at(39),
            &model,
            &StatusCtx::new_for_test(&TelemetryHistory::default()),
        )
        .expect("ci badge opens");
        assert!(matches!(
            overlay.handle_key(&KeyCode::Enter, Modifiers::NONE),
            DetailOutcome::Act(DetailAction::DrillCiRun { .. })
        ));
        assert!(overlay.bind_ci_origin("42", Path::new("/wt/repo")));
        let mut slot = Some(overlay);
        assert!(apply_ci_detail(
            &mut slot,
            CiDetailPayload {
                run,
                origin: "/wt/repo".into(),
                discarded_jobs: 0,
                incomplete: None,
                detail_error: Some("CI provider unreachable".into()),
                log_tail: Vec::new(),
                log_entries: vec![thegn_core::ci_log::CiLogEntry {
                    worktree: "/wt/repo".into(),
                    run_id: "42".into(),
                    job_id: "j1".into(),
                    job_name: "build".into(),
                    text: "error: boom\n".into(),
                    truncated: false,
                    redacted: true,
                    policy_version: thegn_core::ci_log::REDACTION_POLICY_VERSION,
                    fetched_at: 7,
                    head_sha: "abc123".into(),
                }],
            }
        ));
        let mut overlay = slot.expect("overlay remains");
        let DetailContent::Sections(detail) = &overlay.content else {
            panic!("expected sections");
        };
        let Some(Section::Heading { label, note }) = detail.sections.get(1) else {
            panic!("expected jobs heading");
        };
        assert_eq!(label, "jobs");
        assert!(note.as_deref().is_some_and(|note| {
            note.contains("job detail unavailable: CI provider unreachable")
        }));
        assert_eq!(
            overlay.handle_key(&KeyCode::Char('f'), Modifiers::NONE),
            DetailOutcome::Refused(crate::detail::DetailRefusal::IncompleteCiJobList)
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
        assert!(!DetailAction::AckAllAttention.keeps_overlay());
    }
}
