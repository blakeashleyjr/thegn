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

#[derive(Clone, Copy)]
struct PriorityCalls {
    get_nice: fn(u32, crate::platform::ProcStart) -> Result<i32, crate::platform::PriorityError>,
    set_nice:
        fn(u32, crate::platform::ProcStart, i32) -> Result<(), crate::platform::PriorityError>,
    get_ioprio: fn(
        u32,
        crate::platform::ProcStart,
    ) -> Result<crate::platform::IoClass, crate::platform::PriorityError>,
    set_ioprio: fn(
        u32,
        crate::platform::ProcStart,
        crate::platform::IoClass,
    ) -> Result<(), crate::platform::PriorityError>,
}

const SYSTEM_PRIORITY_CALLS: PriorityCalls = PriorityCalls {
    get_nice: crate::platform::get_nice,
    set_nice: crate::platform::set_nice,
    get_ioprio: crate::platform::get_ioprio,
    set_ioprio: crate::platform::set_ioprio,
};

#[cfg(test)]
thread_local! {
    static TEST_PRIORITY_CALLS: std::cell::Cell<Option<PriorityCalls>> = const { std::cell::Cell::new(None) };
}

fn priority_calls() -> PriorityCalls {
    #[cfg(test)]
    if let Some(calls) = TEST_PRIORITY_CALLS.get() {
        return calls;
    }
    SYSTEM_PRIORITY_CALLS
}

#[cfg(test)]
type SignalCall = fn(
    u32,
    crate::platform::ProcStart,
    crate::platform::ProcSignal,
) -> Result<(), crate::platform::SignalError>;

#[cfg(test)]
thread_local! {
    static TEST_SIGNAL_CALL: std::cell::Cell<Option<SignalCall>> = const { std::cell::Cell::new(None) };
}

fn deliver_signal(
    pid: u32,
    start: crate::platform::ProcStart,
    signal: crate::platform::ProcSignal,
) -> Result<(), crate::platform::SignalError> {
    #[cfg(test)]
    if let Some(call) = TEST_SIGNAL_CALL.get() {
        return call(pid, start, signal);
    }
    crate::platform::signal_identity(pid, start, signal)
}

#[cfg(test)]
fn set_test_signal_call(call: SignalCall) {
    TEST_SIGNAL_CALL.set(Some(call));
}

fn priority_error(label: &str, error: crate::platform::PriorityError, denial_hint: &str) -> String {
    match error {
        crate::platform::PriorityError::Changed => {
            format!("{label}: process changed; select it again")
        }
        crate::platform::PriorityError::Denied(_) if !denial_hint.is_empty() => {
            format!("{label}: {denial_hint} (not retried)")
        }
        crate::platform::PriorityError::Denied(error) => format!("{label}: {error} (not retried)"),
        crate::platform::PriorityError::Failed(message) => format!("{label}: {message}"),
        crate::platform::PriorityError::Unsupported => {
            format!("{label}: not supported on this platform")
        }
    }
}

fn process_label(pid: u32, name: &str, owner: thegn_metrics::ProcOwner) -> String {
    let owner = procs_view::owner_label(owner);
    if owner.is_empty() {
        format!("pid {pid} {name}")
    } else {
        format!("pid {pid} {name} ({owner})")
    }
}

pub(in crate::monitor) fn ioprio_label(class: crate::platform::IoClass) -> String {
    match class {
        crate::platform::IoClass::Idle => "idle".to_string(),
        crate::platform::IoClass::BestEffort(level) => format!("best-effort {level}"),
        crate::platform::IoClass::RealTime(_) => "realtime".to_string(),
        crate::platform::IoClass::None => "none".to_string(),
    }
}

pub(in crate::monitor) fn next_ioprio(
    class: crate::platform::IoClass,
    forward: bool,
) -> crate::platform::IoClass {
    use crate::platform::IoClass::{BestEffort, Idle, RealTime};
    match (class, forward) {
        (Idle, true) => BestEffort(0),
        (BestEffort(level), true) if level < 7 => BestEffort(level + 1),
        (BestEffort(_), true) => RealTime(4),
        (RealTime(_), true) => Idle,
        (Idle, false) => RealTime(4),
        (RealTime(_), false) => BestEffort(7),
        (BestEffort(level), false) if level > 0 => BestEffort(level - 1),
        (BestEffort(_), false) => Idle,
        (crate::platform::IoClass::None, true) => BestEffort(5),
        (crate::platform::IoClass::None, false) => BestEffort(3),
    }
}

pub(in crate::monitor) struct ProcDisplay<'a> {
    pub(in crate::monitor) filter: &'a str,
    pub(in crate::monitor) filtering: bool,
    pub(in crate::monitor) tree: bool,
    pub(in crate::monitor) sort: ProcSort,
    pub(in crate::monitor) desc: bool,
    pub(in crate::monitor) invalid_terms: &'a [procs_view::procs_query::InvalidTerm],
    pub(in crate::monitor) primed: bool,
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

    // The strip uses the same 120-column floor as the documented table
    // layout. Narrow terminals keep the existing table height and geometry.
    if cx.cols >= 120
        && let Some(summary) = &snap.summary
    {
        out.extend(summary_sections(summary));
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

fn summary_sections(summary: &thegn_metrics::ProcSummary) -> Vec<Section> {
    let mut sections = vec![heading(
        "tasks",
        Some(format!(
            "{} procs  {} thr  {} kthr",
            summary.tasks, summary.threads, summary.kernel_threads
        )),
    )];
    let state_tone = if summary.disk_sleep > 0 {
        Tok::Hue(Hue::Amber)
    } else {
        Tok::Slot(S::Text)
    };
    sections.push(Section::HeadingToned {
        label: "states".into(),
        label_tone: Tok::Slot(S::Text),
        note: format!(
            "R {}  S {}  D {}  Z {}  T {}  I {}",
            summary.running,
            summary.sleeping,
            summary.disk_sleep,
            summary.zombies,
            summary.stopped,
            summary.idle
        ),
        tone: state_tone,
    });
    sections.push(heading(
        "new / exited",
        Some(format!(
            "+{}  -{}",
            count_mark(summary.new),
            count_mark(summary.exited)
        )),
    ));
    sections.push(owner_share_table(&summary.owners));
    sections
}

fn count_mark(value: Option<usize>) -> String {
    value.map_or_else(
        || {
            crate::monitor::kit::edge_fmt(NumFmt::default()).reading(
                Reading::Unknown,
                Unit::Count,
                5,
            )
        },
        |count| count.to_string(),
    )
}

fn owner_share_table(owners: &thegn_metrics::ProcOwnerSummary) -> Section {
    let classes = [
        ("ui", &owners.ui),
        ("daemon", &owners.daemon),
        ("panes", &owners.panes),
        ("other", &owners.other),
    ];
    let max_cpu = classes
        .iter()
        .filter_map(|(_, totals)| totals.cpu_pct)
        .reduce(f64::max);
    let max_rss = classes
        .iter()
        .filter_map(|(_, totals)| totals.rss_bytes)
        .max();
    let rows = classes
        .iter()
        .map(|(name, totals)| {
            let label = if *name == "panes" {
                format!("panes({})", totals.count)
            } else {
                (*name).to_string()
            };
            vec![
                Cell::Text(label, Tok::Slot(S::Ghost)),
                meter_cell(
                    totals
                        .cpu_pct
                        .map(|value| max_cpu.map_or(0.0, |max| relative_meter(value, max))),
                    Tok::Hue(Hue::Teal),
                ),
                Cell::Text(
                    procs_columns::format_cpu(totals.cpu_pct),
                    Tok::Hue(Hue::Teal),
                ),
                meter_cell(
                    totals.rss_bytes.map(|value| {
                        max_rss.map_or(0.0, |max| relative_meter(value as f64, max as f64))
                    }),
                    Tok::Hue(Hue::Purple),
                ),
                Cell::Text(format_bytes(totals.rss_bytes), Tok::Hue(Hue::Purple)),
            ]
        })
        .collect();
    Section::Table(TableSection {
        header: vec![
            "owner".into(),
            "CPU".into(),
            "%".into(),
            "RSS".into(),
            "bytes".into(),
        ],
        rows,
        sel: None,
    })
}

fn meter_cell(value: Option<f32>, tone: Tok) -> Cell {
    match value {
        Some(value) => Cell::Bar(value, 5, tone),
        None => Cell::Text(
            crate::monitor::kit::edge_fmt(NumFmt::default()).reading(
                Reading::Unknown,
                Unit::Count,
                5,
            ),
            Tok::Slot(S::Ghost),
        ),
    }
}

fn relative_meter(value: f64, max: f64) -> f32 {
    if max > 0.0 {
        (value / max).clamp(0.0, 1.0) as f32
    } else {
        0.0
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
    let non_pid_non_command_widths: usize = fitted
        .iter()
        .filter(|(id, _)| *id != ColId::Pid && *id != ColId::Command)
        .map(|(_, width)| *width)
        .sum();
    let gutters = fitted.len().saturating_sub(1);
    if let Some((_, pid_width)) = fitted.iter_mut().find(|(id, _)| *id == ColId::Pid) {
        let room_for_pid = cols.saturating_sub(1 + gutters + non_pid_non_command_widths + 1);
        *pid_width = (*pid_width).max(7).min(room_for_pid);
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

    pub(in crate::monitor) fn begin_renice(&mut self) -> MonitorOutcome {
        let Some((pid, start_time, start_ticks, name, owner)) =
            self.proc_rows.get(self.sel).map(|row| {
                (
                    row.pid,
                    row.start_time,
                    row.start_ticks,
                    row.name.clone(),
                    row.owner,
                )
            })
        else {
            return MonitorOutcome::Pending;
        };
        let label = process_label(pid, &name, owner);
        let start = crate::platform::ProcStart {
            secs: start_time,
            ticks: start_ticks,
        };
        match (priority_calls().get_nice)(pid, start) {
            Ok(current) => {
                self.confirm = Some(Confirm::Renice {
                    pid,
                    start_time,
                    start_ticks,
                    label,
                    current,
                    target: (current + 1).clamp(-20, 19),
                });
                self.status = None;
            }
            Err(error) => self.status = Some(priority_error(&label, error, "")),
        }
        MonitorOutcome::Pending
    }

    pub(in crate::monitor) fn begin_ionice(&mut self) -> MonitorOutcome {
        let Some((pid, start_time, start_ticks, name, owner)) =
            self.proc_rows.get(self.sel).map(|row| {
                (
                    row.pid,
                    row.start_time,
                    row.start_ticks,
                    row.name.clone(),
                    row.owner,
                )
            })
        else {
            return MonitorOutcome::Pending;
        };
        let label = process_label(pid, &name, owner);
        let start = crate::platform::ProcStart {
            secs: start_time,
            ticks: start_ticks,
        };
        match (priority_calls().get_ioprio)(pid, start) {
            Ok(current) => {
                self.confirm = Some(Confirm::Ionice {
                    pid,
                    start_time,
                    start_ticks,
                    label,
                    current,
                    target: next_ioprio(current, true),
                });
                self.status = None;
            }
            Err(error) => self.status = Some(priority_error(&label, error, "")),
        }
        MonitorOutcome::Pending
    }

    pub(in crate::monitor) fn apply_renice(
        &mut self,
        pid: u32,
        start_time: u64,
        start_ticks: Option<u64>,
        label: &str,
        current: i32,
        target: i32,
    ) {
        if !self
            .proc_rows
            .iter()
            .any(|row| (row.pid, row.start_time) == (pid, start_time))
        {
            self.status = Some(format!(
                "{label}: process changed or left the sampled list; select it again"
            ));
            return;
        }
        let start = crate::platform::ProcStart {
            secs: start_time,
            ticks: start_ticks,
        };
        self.status = match (priority_calls().set_nice)(pid, start, target.clamp(-20, 19)) {
            Ok(()) => Some(format!("{label}: nice set to {}", target.clamp(-20, 19))),
            Err(error) => Some(priority_error(
                label,
                error,
                if target < current {
                    "lowering priority needs privileges"
                } else {
                    ""
                },
            )),
        };
    }

    pub(in crate::monitor) fn apply_ionice(
        &mut self,
        pid: u32,
        start_time: u64,
        start_ticks: Option<u64>,
        label: &str,
        target: crate::platform::IoClass,
    ) {
        if !self
            .proc_rows
            .iter()
            .any(|row| (row.pid, row.start_time) == (pid, start_time))
        {
            self.status = Some(format!(
                "{label}: process changed or left the sampled list; select it again"
            ));
            return;
        }
        let start = crate::platform::ProcStart {
            secs: start_time,
            ticks: start_ticks,
        };
        self.status = match (priority_calls().set_ioprio)(pid, start, target) {
            Ok(()) => Some(format!(
                "{label}: I/O priority set to {}",
                ioprio_label(target)
            )),
            Err(error) => Some(priority_error(
                label,
                error,
                if matches!(target, crate::platform::IoClass::RealTime(_)) {
                    "realtime I/O priority needs privileges"
                } else {
                    ""
                },
            )),
        };
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

    /// Open the platform's signal picker for the selected process.
    pub(in crate::monitor) fn begin_signal_menu(&mut self) -> MonitorOutcome {
        let Some((pid, start_time, start_ticks, name, owner)) = self
            .proc_rows
            .get(self.sel)
            .map(|r| (r.pid, r.start_time, r.start_ticks, r.name.clone(), r.owner))
        else {
            return MonitorOutcome::Pending;
        };
        let owner = procs_view::owner_label(owner);
        let owner = if owner.is_empty() {
            String::new()
        } else {
            format!(" ({owner})")
        };
        self.confirm = Some(Confirm::SignalMenu {
            pid,
            start_time,
            start_ticks,
            label: format!("pid {pid} {name}{owner}"),
            cursor: 0,
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
        let name = format!("SIG{}", stage.label());
        match deliver_signal(pid, start, stage) {
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
    out.push(hint("X", " menu"));
    out.push(hint("=", " nice"));
    if crate::platform::IONICE_SUPPORTED {
        out.push(hint("I", " ionice"));
    }
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

    #[test]
    fn process_summary_has_fixed_rows_and_warns_on_disk_wait() {
        let summary = thegn_metrics::ProcSummary {
            tasks: 4,
            threads: 9,
            kernel_threads: 1,
            running: 1,
            sleeping: 1,
            disk_sleep: 1,
            zombies: 0,
            stopped: 0,
            idle: 1,
            new: Some(2),
            exited: Some(1),
            owners: thegn_metrics::ProcOwnerSummary {
                ui: thegn_metrics::ProcOwnerTotals {
                    count: 1,
                    cpu_pct: Some(12.0),
                    rss_bytes: Some(4096),
                },
                ..thegn_metrics::ProcOwnerSummary::default()
            },
        };
        let sections = summary_sections(&summary);
        assert_eq!(crate::sections::stack_height(&sections), 8);
        assert!(matches!(
            &sections[1],
            Section::HeadingToned {
                label_tone: Tok::Slot(S::Text),
                note,
                tone: Tok::Hue(Hue::Amber),
                ..
            } if note.contains("D 1")
        ));
        assert!(matches!(
            &sections[3],
            Section::Table(table)
                if table.rows.len() == 4
                    && matches!(&table.rows[0][1], Cell::Bar(frac, 5, Tok::Hue(Hue::Teal)) if *frac == 1.0)
                    && matches!(&table.rows[0][2], Cell::Text(cpu, _) if cpu.contains("12.0"))
                    && matches!(&table.rows[0][3], Cell::Bar(frac, 5, Tok::Hue(Hue::Purple)) if *frac == 1.0)
        ));
    }

    #[test]
    fn summary_unknown_marks_follow_ascii_glyph_selection() {
        crate::caps::test_override::with_unicode(thegn_core::termcaps::UnicodeLevel::Ascii, || {
            let expected = crate::monitor::kit::edge_fmt(NumFmt::default()).reading(
                Reading::Unknown,
                Unit::Count,
                5,
            );
            assert_eq!(count_mark(None), expected);
            assert!(matches!(
                meter_cell(None, Tok::Hue(Hue::Teal)),
                Cell::Text(mark, Tok::Slot(S::Ghost)) if mark == expected
            ));
        });
    }

    #[test]
    fn summary_appears_at_wide_widths_and_table_row_y_tracks_it() {
        let row = ProcRow {
            pid: 41,
            start_time: 1,
            start_ticks: None,
            name: "worker".into(),
            owner: thegn_metrics::ProcOwner::Other,
            cpu_pct: 1.0,
            rss_bytes: 4096,
            depth: 0,
            elided_parent: false,
        };
        let model = crate::chrome::FrameModel {
            procs: thegn_metrics::ProcSnapshot {
                procs: vec![thegn_metrics::ProcSample {
                    pid: 41,
                    ppid: None,
                    start_time: 1,
                    start_ticks: None,
                    name: "worker".into(),
                    cpu_pct: 1.0,
                    rss_bytes: 4096,
                    run_secs: 1,
                    owner: thegn_metrics::ProcOwner::Other,
                }],
                total: 1,
                primed: true,
                enabled: true,
                summary: Some(thegn_metrics::ProcSummary {
                    tasks: 1,
                    threads: 1,
                    ..thegn_metrics::ProcSummary::default()
                }),
            },
            ..crate::chrome::FrameModel::default()
        };
        let rows = [row];
        let hist = crate::telemetry::TelemetryHistory::default();
        for cols in [160, 240] {
            let cx = Ctx {
                model: &model,
                hist: &hist,
                prefs: super::super::TabPrefs::default(),
                cols,
                now_ms: 0,
            };
            let build = procs(
                &cx,
                &rows,
                0,
                ProcDisplay {
                    filter: "",
                    filtering: false,
                    tree: false,
                    sort: ProcSort::Cpu,
                    desc: true,
                    invalid_terms: &[],
                    primed: true,
                },
            );
            assert_eq!(
                build.row_y,
                vec![
                    2 + crate::sections::stack_height(&summary_sections(
                        model.procs.summary.as_ref().expect("summary")
                    ))
                ]
            );
            assert!(matches!(
                build.sections.last(),
                Some(Section::FixedTable { table, .. })
                    if matches!(&table.rows[0][0], Cell::Text(pid, _) if pid == "41")
            ));
        }
        let cx = Ctx {
            model: &model,
            hist: &hist,
            prefs: super::super::TabPrefs::default(),
            cols: 119,
            now_ms: 0,
        };
        let build = procs(
            &cx,
            &rows,
            0,
            ProcDisplay {
                filter: "",
                filtering: false,
                tree: false,
                sort: ProcSort::Cpu,
                desc: true,
                invalid_terms: &[],
                primed: true,
            },
        );
        assert_eq!(build.row_y, vec![2]);
    }
}

#[cfg(test)]
#[path = "procs_renice_tests.rs"]
mod renice_tests;

#[cfg(test)]
#[path = "procs_signal_tests.rs"]
mod signal_tests;
