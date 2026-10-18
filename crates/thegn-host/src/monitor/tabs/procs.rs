//! Procs tab renderer and tab-specific monitor actions.

use super::super::build::*;
use super::super::procs_view::{self, ProcRow};
use super::super::{Confirm, MonitorOutcome, MonitorOverlay, ProcSort};
use crate::chrome::S;
use crate::sections::{Cell, Section, TableSection};
use crate::seg::Tok;
use thegn_core::theme::Hue;
use thegn_core::viz::Unit;

#[allow(clippy::too_many_arguments)] // one call site; every argument is real view state
pub(in crate::monitor) fn procs(
    cx: &Ctx,
    rows: &[ProcRow],
    sel: usize,
    filter: &str,
    filtering: bool,
    tree: bool,
    sort: ProcSort,
    desc: bool,
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
    let mut note = if snap.primed {
        format!("{} of {} processes", rows.len(), snap.total)
    } else {
        // CPU is a delta; the first sample has nothing to diff against.
        format!("{} of {} · cpu warming up", rows.len(), snap.total)
    };
    note.push_str(&format!(
        " · {}{}",
        sort.label(),
        if desc { "↓" } else { "↑" }
    ));
    if tree {
        note.push_str(" · tree");
    }
    if !filter.trim().is_empty() || filtering {
        note.push_str(&format!(" · /{}", filter));
    }
    let mut out = vec![heading("processes", Some(note))];

    if rows.is_empty() {
        // Filtered everything out — say so rather than show an empty table.
        out.push(heading("no matching processes", None));
        return plain(out);
    }

    let widths = process_column_widths(cx.cols);
    let name_width = widths[1];
    let body: Vec<Vec<Cell>> = rows
        .iter()
        .map(|p| {
            // The owner tint is the ONLY foreground rule here: the cursor row is
            // the table's `sel`, painted as a background, so selecting a row no
            // longer erases whose process it is.
            let name_tone = owner_tone(p.owner);
            // Tree indent: two spaces per depth, with an elision marker on a row
            // whose real parent fell outside the kept top-N set.
            let name = process_name(p, tree, name_width);
            vec![
                Cell::Text(p.pid.to_string(), Tok::Slot(S::Ghost)),
                Cell::Text(name, name_tone),
                Cell::Text(procs_view::owner_label(p.owner), Tok::Slot(S::Ghost)),
                Cell::Text(
                    if snap.primed {
                        Unit::Percent.fmt(p.cpu_pct)
                    } else {
                        "—".into()
                    },
                    Tok::Hue(Hue::Teal),
                ),
                Cell::Text(Unit::Bytes.fmt(p.rss_bytes as f32), Tok::Hue(Hue::Purple)),
            ]
        })
        .collect();
    let row_y = row_ys(&out, body.len(), true);
    out.push(Section::FixedTable {
        table: TableSection {
            header: vec![
                "pid".into(),
                "name".into(),
                "owner".into(),
                "cpu".into(),
                "mem".into(),
            ],
            rows: body,
            sel: Some(sel),
        },
        widths: widths.to_vec(),
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
) {
    let new_widths = process_column_widths(cols);
    for section in body {
        if let Section::FixedTable { table, widths } = section {
            *widths = new_widths.to_vec();
            for (cells, row) in table.rows.iter_mut().zip(rows) {
                if let Some(Cell::Text(name, _)) = cells.get_mut(1) {
                    *name = process_name(row, tree, new_widths[1]);
                }
            }
        }
    }
}

/// Stable at each viewport width, independent of sampled values and ordering.
/// Include one cursor gutter and four intercolumn spaces in the budget. Preserve
/// numeric/owner room first; the name receives the remaining viewport space.
fn process_column_widths(cols: usize) -> [usize; 5] {
    let mut remaining = cols.saturating_sub(5);
    let mut widths = [0usize; 5];
    for width in &mut widths {
        *width = remaining.min(1);
        remaining -= *width;
    }
    for (index, target) in [(0, 10usize), (3, 7), (4, 9), (2, 9)] {
        let extra = target.saturating_sub(widths[index]).min(remaining);
        widths[index] += extra;
        remaining -= extra;
    }
    widths[1] += remaining;
    widths
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
