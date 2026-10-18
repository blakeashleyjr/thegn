//! Procs tab renderer and tab-specific monitor actions.

use super::super::build::*;
use super::super::procs_columns::{
    self, Availability, ColId, FitFlags, Preset, availability, format_bytes, format_cpu,
    proc_row_caps,
};
use super::super::procs_view::{self, ProcRow};
use super::super::{Confirm, MonitorOutcome, MonitorOverlay, ProcSort};
use crate::chrome::S;
use crate::sections::{Cell, Section, TableSection};
use crate::seg::Tok;
use thegn_core::numfmt::{NumFmt, Reading, Unit};
use thegn_core::theme::Hue;

pub(super) struct ProcDisplay<'a> {
    pub(super) filter: &'a str,
    pub(super) filtering: bool,
    pub(super) tree: bool,
    pub(super) sort: ProcSort,
    pub(super) desc: bool,
    pub(super) invalid_terms: &'a [procs_view::procs_query::InvalidTerm],
    pub(super) primed: bool,
}

pub(in crate::monitor) fn procs(
    cx: &Ctx,
    rows: &[ProcRow],
    sel: usize,
    display: ProcDisplay<'_>,
) -> TabBuild {
    let snap = &cx.model.procs;
    // Only the CONFIG can say sampling is off. `ProcSnapshot::default()` is
    // `enabled: false`, so reading the snapshot here told every user whose
    // first sample had not landed yet that their config said something it did
    // not.
    if !cx.model.procs_enabled() {
        return plain(vec![heading(
            "process sampling is off ([monitor] processes = false)",
            None,
        )]);
    }
    if let crate::model_eq::ProcessViewState::Failed(reason) = cx.model.process_state {
        let label = format!("process sampling unavailable: {reason}");
        return plain(vec![heading(&label, None)]);
    }
    if !snap.enabled || snap.procs.is_empty() {
        // The gate is open but no sample has landed: either the model still
        // holds `ProcSnapshot::default()` (the first frame after the tab
        // opens), or the first scan has no CPU delta yet. Say so rather than
        // showing an empty table, which reads as broken.
        return plain(vec![heading("sampling…", None)]);
    }

    // Header note: how many of the sampled set are shown, the sort, and the
    // active view toggles (filter/tree), so the list never lies about scope.
    let mut note = if display.primed {
        format!("{} of {} processes", rows.len(), snap.total)
    } else {
        // CPU is a delta; the first sample has nothing to diff against.
        format!("{} of {} · cpu warming up", rows.len(), snap.total)
    };
    note.push_str(&format!(
        " · {}{}",
        display.sort.label(),
        if display.desc { "↓" } else { "↑" }
    ));
    if display.tree {
        note.push_str(" · tree");
    }
    if !display.filter.trim().is_empty() || display.filtering {
        note.push_str(&format!(" · /{}", display.filter));
    }
    let mut out = vec![heading("processes", Some(note))];

    if !display.filtering && !display.invalid_terms.is_empty() {
        let ignored = display
            .invalid_terms
            .iter()
            .map(|term| format!("{} ({})", term.text, term.reason))
            .collect::<Vec<_>>()
            .join(", ");
        out.push(Section::HeadingToned {
            label: "ignored filter terms".into(),
            label_tone: Tok::Hue(Hue::Red),
            note: ignored,
            tone: Tok::Hue(Hue::Red),
        });
    }

    if rows.is_empty() {
        // Filtered everything out — say so rather than show an empty table.
        out.push(heading("no matching processes", None));
        return plain(out);
    }

    let fitted = process_columns(cx.cols, display.tree);
    let widths: Vec<usize> = fitted.iter().map(|(_, width)| *width).collect();
    let headers = fitted.iter().map(|(id, _)| column_title(*id)).collect();
    let body = process_cells(rows, &fitted, display.tree, display.primed);
    let row_y = row_ys(&out, body.len(), true);
    out.push(Section::FixedTable {
        table: TableSection {
            header: headers,
            rows: body,
            sel: Some(sel),
        },
        widths,
    });
    TabBuild {
        sections: out,
        row_y,
    }
}

fn process_name(row: &ProcRow, tree: bool, name_width: usize) -> String {
    let mut name = String::new();
    if tree && row.depth > 0 {
        name.push_str(
            &" ".repeat(
                row.depth
                    .saturating_mul(2)
                    .min(name_width.saturating_sub(1)),
            ),
        );
    }
    if row.elided_parent && tree {
        name.push_str("… ");
    }
    name.push_str(&row.name);
    name
}

/// Reflow the already-owned process body without observing a newer snapshot.
/// Its headings, values and selected identity remain frozen while paused.
pub(in crate::monitor) fn reflow_process_geometry(
    body: &mut [Section],
    rows: &[ProcRow],
    tree: bool,
    cols: usize,
    primed: bool,
) {
    let fitted = process_columns(cols, tree);
    let widths: Vec<usize> = fitted.iter().map(|(_, width)| *width).collect();
    let headers = fitted.iter().map(|(id, _)| column_title(*id)).collect();
    for section in body {
        if let Section::FixedTable {
            table,
            widths: current_widths,
        } = section
        {
            *current_widths = widths.clone();
            table.header.clone_from(&headers);
            table.rows = process_cells(rows, &fitted, tree, primed);
        }
    }
}

/// Fit the default preset, remove columns the sampled process rows cannot
/// provide, and let COMMAND take the remaining viewport width.
fn process_columns(cols: usize, tree: bool) -> Vec<(ColId, usize)> {
    if cols < 2 {
        return vec![(ColId::Command, 0)];
    }
    let mut fitted = procs_columns::fit_columns(
        Preset::Default,
        cols,
        FitFlags {
            tree,
            detail_open: false,
        },
    );
    let caps = proc_row_caps();
    fitted.retain(|(id, _)| availability(*id, &caps) == Availability::Available);
    if let Some((_, pid_width)) = fitted.iter_mut().find(|(id, _)| *id == ColId::Pid) {
        *pid_width = if cols >= 60 {
            (*pid_width).max(7)
        } else {
            (*pid_width).min(cols.saturating_sub(2))
        };
    }
    let other_widths: usize = fitted
        .iter()
        .filter(|(id, _)| *id != ColId::Command)
        .map(|(_, width)| *width)
        .sum();
    let gutters = fitted.len().saturating_sub(1);
    let command = fitted
        .iter_mut()
        .find(|(id, _)| *id == ColId::Command)
        .expect("COMMAND is always fitted and available");
    command.1 = cols.saturating_sub(1 + gutters + other_widths);
    fitted
}

fn column_title(id: ColId) -> String {
    procs_columns::CATALOG
        .iter()
        .find(|spec| spec.id == id)
        .map(|spec| spec.title)
        .expect("fitted column is in the catalogue")
        .into()
}

fn process_cells(
    rows: &[ProcRow],
    fitted: &[(ColId, usize)],
    tree: bool,
    primed: bool,
) -> Vec<Vec<Cell>> {
    rows.iter()
        .map(|row| {
            fitted
                .iter()
                .map(|(id, width)| match id {
                    ColId::Pid => Cell::Text(row.pid.to_string(), Tok::Slot(S::Ghost)),
                    ColId::Owner => {
                        Cell::Text(procs_view::owner_label(row.owner), Tok::Slot(S::Ghost))
                    }
                    ColId::Cpu => Cell::Text(
                        format_cpu(primed.then_some(f64::from(row.cpu_pct))),
                        Tok::Hue(Hue::Teal),
                    ),
                    ColId::CpuBar if primed => Cell::Bar(
                        // This currently visualizes total CPU clamped to one core;
                        // user/system split and multi-core scaling are deferred.
                        (row.cpu_pct / 100.0).clamp(0.0, 1.0),
                        *width,
                        Tok::Hue(Hue::Teal),
                    ),
                    ColId::CpuBar => Cell::Text(format_cpu(None), Tok::Hue(Hue::Teal)),
                    ColId::Rss => {
                        Cell::Text(format_bytes(Some(row.rss_bytes)), Tok::Hue(Hue::Purple))
                    }
                    ColId::Command => {
                        // Owner tint stays on the sampled command; tree
                        // indentation remains inside the COMMAND column.
                        Cell::Text(process_name(row, tree, *width), owner_tone(row.owner))
                    }
                    _ => Cell::Text(
                        NumFmt::default().reading(Reading::Unknown, Unit::Count, *width),
                        Tok::Slot(S::Ghost),
                    ),
                })
                .collect()
        })
        .collect()
}

fn owner_tone(o: thegn_metrics::ProcOwner) -> Tok {
    match o {
        thegn_metrics::ProcOwner::Other => Tok::Slot(S::Text),
        thegn_metrics::ProcOwner::ThegnSelf | thegn_metrics::ProcOwner::ThegnDaemon => {
            Tok::Hue(Hue::Green)
        }
        thegn_metrics::ProcOwner::Pane(_) => Tok::Hue(Hue::Blue),
    }
}

impl MonitorOverlay {
    /// Processes-tab sort keys. Reached only after every global key has had its
    /// chance.
    pub(in crate::monitor) fn proc_key(&mut self, c: char) -> MonitorOutcome {
        let sort = match c {
            'c' => ProcSort::Cpu,
            'm' => ProcSort::Rss,
            'n' => ProcSort::Name,
            'p' => ProcSort::Pid,
            '>' | '<' => self.prefs.proc_sort.next(),
            'r' => {
                self.prefs.proc_desc = !self.prefs.proc_desc;
                return MonitorOutcome::PrefsChanged;
            }
            _ => return MonitorOutcome::Pending,
        };
        self.prefs.proc_sort = sort;
        self.sel = 0;
        MonitorOutcome::PrefsChanged
    }

    /// Open a signal confirmation for the selected process. A second signal on
    /// the just-TERMed pid escalates to KILL (a distinct, explicit second step).
    pub(in crate::monitor) fn begin_signal(&mut self) -> MonitorOutcome {
        // Copy out of the row first so the immutable borrow of `self.proc_rows`
        // ends before `self.confirm`/`self.status` are written.
        let Some((pid, start_time, start_ticks, name, owner)) = self
            .proc_rows
            .get(self.sel)
            .map(|r| (r.pid, r.start_time, r.start_ticks, r.name.clone(), r.owner))
        else {
            return MonitorOutcome::Pending;
        };
        let stage = if self.last_termed == Some((pid, start_time)) {
            crate::platform::ProcSignal::Kill
        } else {
            crate::platform::ProcSignal::Terminate
        };
        let owner = procs_view::owner_label(owner);
        let owner = if owner.is_empty() {
            String::new()
        } else {
            format!(" ({owner})")
        };
        let label = format!("pid {pid} {name}{owner}");
        self.confirm = Some(Confirm::Signal {
            pid,
            start_time,
            start_ticks,
            label,
            stage,
        });
        self.status = None;
        MonitorOutcome::Pending
    }

    /// Deliver the confirmed signal, surfacing the outcome — never swallowed.
    pub(in crate::monitor) fn perform_signal(
        &mut self,
        pid: u32,
        start: crate::platform::ProcStart,
        label: &str,
        stage: crate::platform::ProcSignal,
    ) {
        let name = match stage {
            crate::platform::ProcSignal::Terminate => "SIGTERM",
            crate::platform::ProcSignal::Kill => "SIGKILL",
        };
        match crate::platform::signal_identity(pid, start, stage) {
            Ok(()) => {
                self.status = Some(format!("sent {name} to {label}"));
                if stage == crate::platform::ProcSignal::Terminate {
                    self.last_termed = Some((pid, start.secs));
                }
            }
            Err(crate::platform::SignalError::Changed) => {
                self.status = Some(format!(
                    "{label}: process changed or left the sampled list; select it again"
                ));
            }
            Err(crate::platform::SignalError::Failed(error)) => {
                self.status = Some(format!("{label}: {error}"));
            }
        }
    }
}

pub(super) fn hints(
    input: &super::super::footer::FooterInput<'_>,
    out: &mut Vec<Vec<crate::seg::Seg>>,
) {
    use super::super::footer::hint;
    out.push(hint(
        "c/m/n",
        format!(
            " sort {}{}",
            input.prefs.proc_sort.label(),
            if input.prefs.proc_desc { "↓" } else { "↑" }
        ),
    ));
    out.push(hint("/", " find"));
    out.push(hint(
        "t",
        if input.prefs.proc_tree {
            " flat"
        } else {
            " tree"
        },
    ));
    out.push(hint("x", " signal"));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn available_unrendered_columns_use_unknown_ghost_cells() {
        let row = ProcRow {
            pid: 41,
            start_time: 1,
            start_ticks: None,
            name: "worker".into(),
            owner: thegn_metrics::ProcOwner::Other,
            cpu_pct: 0.0,
            rss_bytes: 0,
            depth: 0,
            elided_parent: false,
        };
        let cells = process_cells(&[row], &[(ColId::Ppid, 4)], false, true);
        assert!(matches!(
            &cells[0][0],
            Cell::Text(value, Tok::Slot(S::Ghost)) if value.trim() == "—"
        ));
    }
}
