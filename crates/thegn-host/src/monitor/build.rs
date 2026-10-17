//! Per-tab content for the system monitor.
//!
//! Every builder has the same shape — take the model, the history, and the
//! tab's toggles; return a [`Section`] stack — so [`tab`] dispatches with one
//! match and the overlay never needs to know what a tab contains.
//!
//! Rows for metrics this machine doesn't expose are omitted rather than shown
//! empty: the history records `f32::NAN` for an absent reading (see
//! [`crate::telemetry`]), and a confident `0` would be a wrong number rather
//! than a missing one.

use super::procs_view::ProcRow;
use super::{GraphStyle, MonitorTab, ProcSort, TabPrefs};
use crate::chrome::{FrameModel, S};
use crate::sections::{GraphSection, Section};
use crate::seg::Tok;
use crate::telemetry::{Metric, SeriesOut, SeriesReq, TelemetryHistory};
use thegn_core::disk_fill::DiskFillEta;
use thegn_core::series::Agg;
use thegn_core::theme::Hue;
use thegn_core::viz::{self, Unit};

/// Plot height for a tab's headline graph.
pub(super) const MAIN_H: usize = 6;
/// Plot height for a secondary graph (a per-interface or per-disk trace).
pub(super) const SUB_H: usize = 3;

/// One worktree's cached disk usage, for the Disk tab's worktree lane. Built by
/// [`worktree_disk_rows`] and owned by the overlay so the clean action targets
/// exactly the rendered row.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct DiskWtRow {
    pub path: std::path::PathBuf,
    pub name: String,
    pub total_bytes: u64,
    pub target_bytes: u64,
    /// Age of the cached measurement in seconds, `None` when unknown/unstamped.
    pub age_secs: Option<u64>,
    pub measured_at_secs: Option<u64>,
}

/// Everything the overlay hands the renderer for one tab. A struct rather than a
/// dozen positional args: the Processes and Disk tabs both carry precomputed,
/// selectable row lists that must match what the key handler indexes.
pub(super) struct TabInput<'a> {
    pub tab: MonitorTab,
    pub model: &'a FrameModel,
    pub hist: &'a TelemetryHistory,
    pub prefs: TabPrefs,
    pub cols: usize,
    pub now_ms: u64,
    pub sel: usize,
    pub filter: &'a str,
    pub filtering: bool,
    pub tree: bool,
    pub proc_sort: ProcSort,
    pub proc_desc: bool,
    pub proc_rows: &'a [ProcRow],
    pub disk_rows: &'a [DiskWtRow],
    pub disk_eta: Option<DiskFillEta>,
}

/// A built tab: its section stack, plus where the selectable rows landed in it.
///
/// The geometry rides back with the sections because only the builder knows it
/// — Processes puts one table under one heading, Disk puts one under a graph
/// plus two tables plus a grid.
/// Recomputing that in the overlay would be a second copy of the layout, which
/// is exactly the drift `sections.rs`'s header warns about.
pub(super) struct TabBuild {
    pub sections: Vec<Section>,
    /// Stack-relative y of each selectable row, in `sel` order. Empty on a tab
    /// with no row cursor.
    pub row_y: Vec<usize>,
}

/// A tab with no row cursor: sections only.
pub(super) fn plain(sections: Vec<Section>) -> TabBuild {
    TabBuild {
        sections,
        row_y: Vec::new(),
    }
}

/// Stack-relative y of the `n` body rows of a table about to be pushed onto
/// `out`. Measured with `sections::stack_height` — the same function
/// `scroll_max` measures against — so the cursor and the scroll clamp can
/// never disagree about where a row is.
pub(super) fn row_ys(out: &[Section], n: usize, has_header: bool) -> Vec<usize> {
    let base = crate::sections::stack_height(out) + has_header as usize;
    (base..base + n).collect()
}

/// Build the active tab's section stack.
pub(super) fn tab(input: TabInput) -> TabBuild {
    let cx = Ctx {
        model: input.model,
        hist: input.hist,
        prefs: input.prefs,
        cols: input.cols,
        now_ms: input.now_ms,
    };
    match input.tab {
        MonitorTab::Cpu => plain(super::tabs::cpu::cpu(&cx)),
        MonitorTab::Memory => plain(super::tabs::memory::memory(&cx)),
        MonitorTab::Thermal => plain(super::tabs::thermal::thermal(&cx)),
        MonitorTab::Network => plain(super::tabs::network::network(&cx)),
        MonitorTab::Disk => {
            super::tabs::disk::disk(&cx, input.disk_rows, input.sel, input.disk_eta)
        }
        MonitorTab::Gpu => plain(super::tabs::gpu::gpu(&cx)),
        MonitorTab::Power => plain(super::tabs::power::power(&cx)),
        MonitorTab::Procs => super::tabs::procs::procs(
            &cx,
            input.proc_rows,
            input.sel,
            input.filter,
            input.filtering,
            input.tree,
            input.proc_sort,
            input.proc_desc,
        ),
        // Containers renders straight off `cx.model.containers` — the overlay's
        // cached `container_rows` mirrors that exact order, so `input.sel`
        // indexes the same row the key handler resolves.
        MonitorTab::Containers => super::tabs::containers::containers(&cx, input.sel),
        MonitorTab::Notifications => super::tabs::notifications::notifications(&cx),
    }
}

/// Build the Disk tab's worktree-usage rows from the sidebar's `worktree_disk`
/// cache — sorted by total size (biggest first), with the measurement age. Pure:
/// no filesystem walk, so opening the tab never triggers a `du`.
pub(super) fn worktree_disk_rows(model: &FrameModel, now_secs: u64) -> Vec<DiskWtRow> {
    #[cfg(test)]
    crate::proc_workload_alloc::disk_rows();
    let stamps = &model.sidebar_status.disk_stamps;
    let mut rows: Vec<DiskWtRow> = model
        .sidebar_status
        .disk_sizes
        .iter()
        .map(|(path, (total, target))| {
            let measured_at_secs = stamps.get(path).filter(|&&t| t > 0).map(|&t| t as u64);
            let age_secs = measured_at_secs.map(|stamp| now_secs.saturating_sub(stamp));
            DiskWtRow {
                name: std::path::Path::new(path)
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.clone()),
                path: std::path::PathBuf::from(path),
                total_bytes: (*total).max(0) as u64,
                target_bytes: (*target).max(0) as u64,
                age_secs,
                measured_at_secs,
            }
        })
        .collect();
    // Biggest first — the point is finding the worktree eating the disk. Ties
    // break by name and then the full path, including duplicate basenames.
    rows.sort_by(|a, b| {
        b.total_bytes
            .cmp(&a.total_bytes)
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.path.cmp(&b.path))
    });
    rows
}

/// Everything a tab builder needs, bundled so each takes one argument.
pub(super) struct Ctx<'a> {
    pub(super) model: &'a FrameModel,
    pub(super) hist: &'a TelemetryHistory,
    pub(super) prefs: TabPrefs,
    pub(super) cols: usize,
    pub(super) now_ms: u64,
}

impl Ctx<'_> {
    /// Dot columns available to a plot. Two per cell, minus the axis gutter.
    pub(super) fn buckets(&self) -> usize {
        self.cols.saturating_sub(AXIS_W + 1).max(8) * 2
    }

    /// Fetch one metric under the tab's current window and scale.
    pub(super) fn series(&self, m: Metric) -> SeriesOut {
        self.hist.series(&SeriesReq {
            metric: m,
            window: self.prefs.window,
            scale: TelemetryHistory::scale_for(m, self.prefs.scale),
            buckets: self.buckets(),
            // A band is only meaningful when a column spans several samples;
            // Area draws it, the other styles have no second edge to show.
            agg: if self.prefs.style == GraphStyle::Area {
                Agg::MinMax
            } else {
                Agg::Max
            },
            now_ms: self.now_ms,
        })
    }

    /// A headline graph block for one metric.
    pub(super) fn graph(&self, m: Metric, height: usize, tone: Tok) -> Section {
        let out = self.series(m);
        self.graph_from(m, &out, height, tone, m.label().to_string())
    }

    pub(super) fn graph_from(
        &self,
        m: Metric,
        out: &SeriesOut,
        height: usize,
        tone: Tok,
        label: String,
    ) -> Section {
        let unit = m.unit();
        Section::Graph(GraphSection {
            label,
            cur: if out.last.is_finite() {
                unit.fmt(out.last)
            } else {
                "—".into()
            },
            footer: Some(summary(out, unit)),
            series: out.hi.clone(),
            lo: (self.prefs.style == GraphStyle::Area).then(|| out.lo.clone()),
            axis: axis_for(out, unit, self.prefs.style, height),
            tone,
            height,
            series2: None,
            style: self.prefs.style,
        })
    }

    /// A one-row inline trend.
    pub(super) fn sparkrow(&self, m: Metric, label: &str, tone: Tok) -> Section {
        let out = self.series(m);
        Section::Sparkrow {
            label: label.into(),
            spark: viz::fit(&out.hi, 16),
            cur: if out.last.is_finite() {
                m.unit().fmt(out.last)
            } else {
                "—".into()
            },
            tone,
        }
    }
}

/// Width reserved for the axis gutter.
pub(super) const AXIS_W: usize = 5;

/// Axis labels for a plot, or none when the style has no room for them.
pub(super) fn axis_for(
    out: &SeriesOut,
    unit: Unit,
    style: GraphStyle,
    height: usize,
) -> Vec<String> {
    if style == GraphStyle::Spark || height < 3 {
        return Vec::new();
    }
    viz::axis_labels(out.axis_min, out.axis_max, height, unit)
}

/// `min 4%  avg 31%  max 92%` over the visible window.
pub(super) fn summary(out: &SeriesOut, unit: Unit) -> String {
    let vals: Vec<f32> = out
        .raw_hi
        .iter()
        .copied()
        .filter(|v| v.is_finite())
        .collect();
    if vals.is_empty() {
        return "no samples in window".into();
    }
    let min = vals.iter().copied().fold(f32::INFINITY, f32::min);
    let max = vals.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    let avg = vals.iter().sum::<f32>() / vals.len() as f32;
    format!(
        "min {}  avg {}  max {}",
        unit.fmt(min),
        unit.fmt(avg),
        unit.fmt(max)
    )
}

pub(super) fn kv(k: &str, v: String, tone: Tok) -> (String, String, Tok) {
    (k.into(), v, tone)
}

pub(super) fn heading(label: &str, note: Option<String>) -> Section {
    Section::Heading {
        label: label.into(),
        note,
    }
}

pub(super) fn temp_tone(c: f32) -> Tok {
    if c >= 85.0 {
        Tok::Hue(Hue::Red)
    } else if c >= 70.0 {
        Tok::Hue(Hue::Amber)
    } else {
        Tok::Slot(S::Text)
    }
}

/// Truncate to `max` display cells with an ellipsis.
pub(super) fn trunc(s: &str, max: usize) -> String {
    if crate::seg::cells(s) <= max {
        return s.to_string();
    }
    let mut out = crate::seg::take_cols(s, max.saturating_sub(1)).to_string();
    out.push('…');
    out
}

pub(super) use super::tabs::procs::reflow_process_geometry;
