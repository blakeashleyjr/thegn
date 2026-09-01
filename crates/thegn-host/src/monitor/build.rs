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

use super::procs_view::{self, ProcRow};
use super::{GraphStyle, MonitorTab, ProcSort, TabPrefs};
use crate::chrome::{FrameModel, S};
use crate::sections::{Cell, GraphSection, Section, TableSection, spacer};
use crate::seg::Tok;
use crate::telemetry::{Metric, SeriesOut, SeriesReq, TelemetryHistory};
use thegn_core::disk_fill::DiskFillEta;
use thegn_core::series::Agg;
use thegn_core::theme::Hue;
use thegn_core::viz::{self, Unit};

/// Plot height for a tab's headline graph.
const MAIN_H: usize = 6;
/// Plot height for a secondary graph (a per-interface or per-disk trace).
const SUB_H: usize = 3;

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
fn plain(sections: Vec<Section>) -> TabBuild {
    TabBuild {
        sections,
        row_y: Vec::new(),
    }
}

/// Stack-relative y of the `n` body rows of a table about to be pushed onto
/// `out`. Measured with `sections::stack_height` — the same function
/// `scroll_max` measures against — so the cursor and the scroll clamp can
/// never disagree about where a row is.
fn row_ys(out: &[Section], n: usize, has_header: bool) -> Vec<usize> {
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
        MonitorTab::Cpu => plain(cpu(&cx)),
        MonitorTab::Memory => plain(memory(&cx)),
        MonitorTab::Thermal => plain(thermal(&cx)),
        MonitorTab::Network => plain(network(&cx)),
        MonitorTab::Disk => disk(&cx, input.disk_rows, input.sel, input.disk_eta),
        MonitorTab::Gpu => plain(gpu(&cx)),
        MonitorTab::Power => plain(power(&cx)),
        MonitorTab::Procs => procs(
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
        MonitorTab::Containers => containers(&cx, input.sel),
        MonitorTab::Notifications => notifications(&cx),
    }
}

/// Build the conditional Notifications tab from the worker's counter-only
/// snapshot. Endpoints and payloads never cross into the render model.
fn notifications(cx: &Ctx<'_>) -> TabBuild {
    let rows = cx.model.notification_delivery.rows();
    if rows.is_empty() {
        return plain(vec![heading(
            "delivery",
            Some("no configured sinks".into()),
        )]);
    }
    let table_rows = rows
        .into_iter()
        .map(|row| {
            let outcome_tone = if row.dead_letters > 0 {
                Tok::Hue(Hue::Red)
            } else {
                Tok::Slot(S::Text)
            };
            vec![
                Cell::Text(trunc(&row.name, 18), Tok::Slot(S::Text)),
                Cell::Text(trunc(&row.kind, 10), Tok::Slot(S::Ghost)),
                Cell::Text(row.queue_drops.to_string(), Tok::Slot(S::Dim)),
                Cell::Text(row.rate_limit_drops.to_string(), Tok::Slot(S::Dim)),
                Cell::Text(row.retries.to_string(), Tok::Slot(S::Dim)),
                Cell::Text(row.sent.to_string(), Tok::Slot(S::Text)),
                Cell::Text(row.dead_letters.to_string(), outcome_tone),
            ]
        })
        .collect();
    plain(vec![
        heading("delivery", Some("bounded, best-effort".into())),
        Section::Table(TableSection {
            header: vec![
                "sink".into(),
                "kind".into(),
                "queue drop".into(),
                "rate drop".into(),
                "retry".into(),
                "sent".into(),
                "dead-letter".into(),
            ],
            rows: table_rows,
            sel: None,
        }),
    ])
}

/// Build the Disk tab's worktree-usage rows from the sidebar's `worktree_disk`
/// cache — sorted by total size (biggest first), with the measurement age. Pure:
/// no filesystem walk, so opening the tab never triggers a `du`.
pub(super) fn worktree_disk_rows(model: &FrameModel, now_secs: u64) -> Vec<DiskWtRow> {
    let stamps = &model.sidebar_status.disk_stamps;
    let mut rows: Vec<DiskWtRow> = model
        .sidebar_status
        .disk_sizes
        .iter()
        .map(|(path, (total, target))| {
            let age_secs = stamps
                .get(path)
                .filter(|&&t| t > 0)
                .map(|&t| now_secs.saturating_sub(t as u64));
            DiskWtRow {
                name: std::path::Path::new(path)
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.clone()),
                path: std::path::PathBuf::from(path),
                total_bytes: (*total).max(0) as u64,
                target_bytes: (*target).max(0) as u64,
                age_secs,
            }
        })
        .collect();
    // Biggest first — the point is finding the worktree eating the disk. Ties
    // break by name so the order is stable frame to frame.
    rows.sort_by(|a, b| {
        b.total_bytes
            .cmp(&a.total_bytes)
            .then_with(|| a.name.cmp(&b.name))
    });
    rows
}

/// Everything a tab builder needs, bundled so each takes one argument.
struct Ctx<'a> {
    model: &'a FrameModel,
    hist: &'a TelemetryHistory,
    prefs: TabPrefs,
    cols: usize,
    now_ms: u64,
}

impl Ctx<'_> {
    /// Dot columns available to a plot. Two per cell, minus the axis gutter.
    fn buckets(&self) -> usize {
        self.cols.saturating_sub(AXIS_W + 1).max(8) * 2
    }

    /// Fetch one metric under the tab's current window and scale.
    fn series(&self, m: Metric) -> SeriesOut {
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
    fn graph(&self, m: Metric, height: usize, tone: Tok) -> Section {
        let out = self.series(m);
        self.graph_from(m, &out, height, tone, m.label().to_string())
    }

    fn graph_from(
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
    fn sparkrow(&self, m: Metric, label: &str, tone: Tok) -> Section {
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
const AXIS_W: usize = 5;

/// Axis labels for a plot, or none when the style has no room for them.
fn axis_for(out: &SeriesOut, unit: Unit, style: GraphStyle, height: usize) -> Vec<String> {
    if style == GraphStyle::Spark || height < 3 {
        return Vec::new();
    }
    viz::axis_labels(out.axis_min, out.axis_max, height, unit)
}

/// `min 4%  avg 31%  max 92%` over the visible window.
fn summary(out: &SeriesOut, unit: Unit) -> String {
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

fn kv(k: &str, v: String, tone: Tok) -> (String, String, Tok) {
    (k.into(), v, tone)
}

fn heading(label: &str, note: Option<String>) -> Section {
    Section::Heading {
        label: label.into(),
        note,
    }
}

// --- CPU -----------------------------------------------------------------

fn cpu(cx: &Ctx) -> Vec<Section> {
    let s = &cx.model.stats;
    let mut out = vec![cx.graph(Metric::Cpu, MAIN_H, Tok::Hue(Hue::Teal))];

    // Per-core heat grid — sampled every tick today but shown nowhere in the
    // popups, which is most of the reason this tab exists.
    if !s.cpu_cores.is_empty() {
        out.push(spacer());
        out.push(heading(
            "cores",
            Some(format!("{} logical", s.cpu_cores.len())),
        ));
        out.extend(core_rows(&s.cpu_cores, cx.cols));
    }

    out.push(spacer());
    out.push(heading("load & clock", None));
    let mut cells = Vec::new();
    if let Some((one, five, fifteen)) = s.load_avg {
        let cores = s.cpu_cores.len().max(1) as f32;
        cells.push(kv("load 1m", format!("{one:.2}"), Tok::Slot(S::Text)));
        cells.push(kv("5m", format!("{five:.2}"), Tok::Slot(S::Dim)));
        cells.push(kv("15m", format!("{fifteen:.2}"), Tok::Slot(S::Dim)));
        // Per-core is the comparable number: 4.0 is saturated on a 4-core box
        // and idle on a 64-core one.
        cells.push(kv(
            "per core",
            format!("{:.2}", one / cores),
            Tok::Slot(S::Dim),
        ));
    }
    if let Some(mhz) = s.cpu_freq_mhz {
        cells.push(kv(
            "frequency",
            Unit::Megahertz.fmt(mhz as f32),
            Tok::Slot(S::Text),
        ));
    }
    if let Some(c) = s.cpu_temp_c {
        cells.push(kv("temperature", Unit::Celsius.fmt(c), temp_tone(c)));
    }
    if !cells.is_empty() {
        out.push(Section::Grid { cols: 2, cells });
    }
    if s.load_avg.is_some() {
        out.push(cx.sparkrow(Metric::Load, "load", Tok::Hue(Hue::Blue)));
    }
    out
}

/// Per-core utilization as heat-tinted bars, wrapped to the box width.
fn core_rows(cores: &[u8], cols: usize) -> Vec<Section> {
    // Each core costs `id + bar + pct`; fit as many per row as the box allows.
    const CELL: usize = 12;
    let per_row = (cols / CELL).max(1);
    let rows: Vec<Vec<Cell>> = cores
        .chunks(per_row)
        .enumerate()
        .map(|(chunk, group)| {
            let mut row = Vec::new();
            for (i, &pct) in group.iter().enumerate() {
                let id = chunk * per_row + i;
                let frac = f32::from(pct) / 100.0;
                row.push(Cell::Text(format!("{id:>3}"), Tok::Slot(S::Ghost)));
                row.push(Cell::Bar(frac, 5, Tok::Heat(viz::heat_index(frac) as u8)));
                row.push(Cell::Text(format!("{pct:>3}%"), Tok::Slot(S::Dim)));
            }
            row
        })
        .collect();
    vec![Section::Table(TableSection {
        header: Vec::new(),
        rows,
        sel: None,
    })]
}

fn temp_tone(c: f32) -> Tok {
    if c >= 85.0 {
        Tok::Hue(Hue::Red)
    } else if c >= 70.0 {
        Tok::Hue(Hue::Amber)
    } else {
        Tok::Slot(S::Text)
    }
}

// --- Memory --------------------------------------------------------------

fn memory(cx: &Ctx) -> Vec<Section> {
    let s = &cx.model.stats;
    let mut out = vec![cx.graph(Metric::Mem, MAIN_H, Tok::Hue(Hue::Purple))];

    out.push(spacer());
    out.push(heading("breakdown", None));
    let mut cells = Vec::new();
    if let Some((u, t)) = s.mem_gib {
        cells.push(kv("used", format!("{u:.1}G"), Tok::Slot(S::Text)));
        cells.push(kv(
            "free",
            format!("{:.1}G", (t - u).max(0.0)),
            Tok::Slot(S::Dim),
        ));
        cells.push(kv("total", format!("{t:.0}G"), Tok::Slot(S::Dim)));
        cells.push(kv(
            "in use",
            format!("{:.0}%", if t > 0.0 { u / t * 100.0 } else { 0.0 }),
            Tok::Slot(S::Dim),
        ));
    }
    if !cells.is_empty() {
        out.push(Section::Grid { cols: 2, cells });
    }

    if let Some((u, t)) = s.swap_gib {
        out.push(spacer());
        out.push(heading("swap", Some(format!("{u:.1}G of {t:.0}G"))));
        out.push(cx.graph(Metric::Swap, SUB_H, Tok::Hue(Hue::Blue)));
    }

    // thegn's own footprint — the honest place to notice the UI leaking.
    out.push(spacer());
    out.push(heading("thegn", None));
    let (rss, cpu_pct, drss, dcpu) = cx.hist.last_proc();
    let mut cells = vec![
        kv("rss", Unit::Bytes.fmt(rss as f32), Tok::Slot(S::Text)),
        kv("cpu", Unit::Percent.fmt(cpu_pct), Tok::Slot(S::Dim)),
    ];
    if drss > 0 {
        cells.push(kv(
            "daemon rss",
            Unit::Bytes.fmt(drss as f32),
            Tok::Slot(S::Dim),
        ));
        cells.push(kv("daemon cpu", Unit::Percent.fmt(dcpu), Tok::Slot(S::Dim)));
    }
    out.push(Section::Grid { cols: 2, cells });
    out.push(cx.sparkrow(Metric::SelfRss, "rss", Tok::Hue(Hue::Green)));
    out
}

// --- Thermal -------------------------------------------------------------

fn thermal(cx: &Ctx) -> Vec<Section> {
    let s = &cx.model.stats;
    let mut out = Vec::new();
    if s.cpu_temp_c.is_some() {
        out.push(cx.graph(Metric::Temp, MAIN_H, Tok::Hue(Hue::Amber)));
    }
    // EVERY sensor, not just the hottest — the popup only ever showed one, so
    // a hot NVMe or chipset was invisible.
    if !s.temps.is_empty() {
        out.push(spacer());
        out.push(heading("sensors", Some(format!("{}", s.temps.len()))));
        let rows: Vec<Vec<Cell>> = s
            .temps
            .iter()
            .map(|(name, c)| {
                vec![
                    Cell::Text(trunc(name, 24), Tok::Slot(S::Text)),
                    Cell::Bar((c / 100.0).clamp(0.0, 1.0), 10, temp_tone(*c)),
                    Cell::Text(Unit::Celsius.fmt(*c), temp_tone(*c)),
                ]
            })
            .collect();
        out.push(Section::Table(TableSection {
            header: vec!["sensor".into(), "".into(), "temp".into()],
            rows,
            sel: None,
        }));
    }
    if let Some(c) = s.gpu_temp_c {
        out.push(spacer());
        out.push(Section::KeyVal(vec![kv(
            "gpu",
            Unit::Celsius.fmt(c),
            temp_tone(c),
        )]));
    }
    if out.is_empty() {
        out.push(heading("no temperature sensors on this machine", None));
    }
    out
}

// --- Network -------------------------------------------------------------

fn network(cx: &Ctx) -> Vec<Section> {
    let s = &cx.model.stats;
    let rx = cx.series(Metric::NetRx);
    let tx = cx.series(Metric::NetTx);
    let mut out = vec![Section::Graph(GraphSection {
        label: "NET".into(),
        cur: format!(
            "↓{} ↑{}",
            Unit::BytesPerSec.fmt(rx.last),
            Unit::BytesPerSec.fmt(tx.last)
        ),
        footer: Some("↓ rx (top) · ↑ tx (bottom)".into()),
        series: rx.hi.clone(),
        lo: None,
        axis: Vec::new(),
        tone: Tok::Hue(Hue::Green),
        height: MAIN_H,
        series2: Some((tx.hi.clone(), Tok::Hue(Hue::Blue))),
        style: cx.prefs.style,
    })];

    out.push(spacer());
    out.push(heading(
        "interfaces",
        Some(format!("{}", s.net_ifaces.len())),
    ));
    if s.net_ifaces.is_empty() {
        out.push(Section::KeyVal(vec![kv(
            "interfaces",
            "idle".into(),
            Tok::Slot(S::Ghost),
        )]));
    } else {
        let rows: Vec<Vec<Cell>> = s
            .net_ifaces
            .iter()
            .map(|(name, r, t)| {
                vec![
                    Cell::Text(trunc(name, 16), Tok::Slot(S::Text)),
                    Cell::Text(
                        format!("↓{}", Unit::BytesPerSec.fmt(*r as f32)),
                        Tok::Hue(Hue::Green),
                    ),
                    Cell::Text(
                        format!("↑{}", Unit::BytesPerSec.fmt(*t as f32)),
                        Tok::Hue(Hue::Blue),
                    ),
                ]
            })
            .collect();
        out.push(Section::Table(TableSection {
            header: vec!["iface".into(), "rx".into(), "tx".into()],
            rows,
            sel: None,
        }));
    }
    out
}

// --- Disk ----------------------------------------------------------------

fn disk(cx: &Ctx, wt_rows: &[DiskWtRow], sel: usize, eta: Option<DiskFillEta>) -> TabBuild {
    let s = &cx.model.stats;
    let mut out = vec![cx.graph(Metric::DiskIo, MAIN_H, Tok::Hue(Hue::Blue))];

    out.push(spacer());
    out.push(heading("volumes", Some(format!("{}", s.disks.len()))));
    let rows: Vec<Vec<Cell>> = s
        .disks
        .iter()
        .map(|d| {
            let tone = free_tone(d.free_pct);
            vec![
                Cell::Text(trunc(&d.mount, 20), Tok::Slot(S::Text)),
                Cell::Text(kind_str(d.kind).into(), Tok::Slot(S::Ghost)),
                Cell::Bar(f32::from(d.free_pct) / 100.0, 8, tone),
                Cell::Text(format!("{}% free", d.free_pct), tone),
                Cell::Text(
                    format!("↓{}", Unit::BytesPerSec.fmt(d.read_bps as f32)),
                    Tok::Slot(S::Dim),
                ),
                Cell::Text(
                    format!("↑{}", Unit::BytesPerSec.fmt(d.write_bps as f32)),
                    Tok::Slot(S::Dim),
                ),
            ]
        })
        .collect();
    out.push(Section::Table(TableSection {
        header: vec![
            "mount".into(),
            "kind".into(),
            "free".into(),
            "".into(),
            "read".into(),
            "write".into(),
        ],
        rows,
        sel: None,
    }));

    if let Some((total, avail)) = s.disk_bytes {
        out.push(spacer());
        // The fill projection rides the worktrees-filesystem heading, so it sits
        // right beside the free-space number it extrapolates from.
        out.push(heading("worktrees filesystem", eta.map(fmt_fill_eta)));
        out.push(Section::Grid {
            cols: 2,
            cells: vec![
                kv("total", Unit::Bytes.fmt(total as f32), Tok::Slot(S::Dim)),
                kv(
                    "available",
                    Unit::Bytes.fmt(avail as f32),
                    Tok::Slot(S::Text),
                ),
                kv(
                    "used",
                    Unit::Bytes.fmt(total.saturating_sub(avail) as f32),
                    Tok::Slot(S::Dim),
                ),
            ],
        });
    }

    // The worktree lane: per-worktree usage from the `[disk]` scanner cache, so
    // "where did the disk go?" has an IDE-shaped answer without a du walk.
    out.push(spacer());
    out.push(worktrees_heading(wt_rows));
    let mut row_y = Vec::new();
    if !wt_rows.is_empty() {
        let rows: Vec<Vec<Cell>> = wt_rows
            .iter()
            .map(|w| {
                vec![
                    Cell::Text(trunc(&w.name, 22), Tok::Slot(S::Text)),
                    Cell::Text(Unit::Bytes.fmt(w.total_bytes as f32), Tok::Slot(S::Text)),
                    Cell::Text(
                        format!("target {}", Unit::Bytes.fmt(w.target_bytes as f32)),
                        Tok::Hue(Hue::Amber),
                    ),
                    Cell::Text(fmt_age(w.age_secs), Tok::Slot(S::Ghost)),
                ]
            })
            .collect();
        row_y = row_ys(&out, rows.len(), true);
        out.push(Section::Table(TableSection {
            header: vec![
                "worktree".into(),
                "size".into(),
                "reclaimable".into(),
                "measured".into(),
            ],
            rows,
            sel: Some(sel),
        }));
    }
    TabBuild {
        sections: out,
        row_y,
    }
}

/// The worktrees-lane heading: count plus the grand `target/` reclaimable total,
/// which is the number that answers "how much can I get back".
fn worktrees_heading(rows: &[DiskWtRow]) -> Section {
    if rows.is_empty() {
        return heading(
            "worktrees",
            Some("no cached sizes yet (or [disk] show_sizes = false)".into()),
        );
    }
    let reclaimable: u64 = rows.iter().map(|w| w.target_bytes).sum();
    heading(
        "worktrees",
        Some(format!(
            "{} · {} reclaimable",
            rows.len(),
            Unit::Bytes.fmt(reclaimable as f32)
        )),
    )
}

/// "full in ~2d" / "full in ~5h" for the fill projection. The `~` is honest: it
/// is an extrapolation of a short trend, not a guarantee.
fn fmt_fill_eta(eta: DiskFillEta) -> String {
    let hours = eta.hours;
    let when = if hours >= 48.0 {
        format!("~{}d", (hours / 24.0).round() as u64)
    } else if hours >= 1.0 {
        format!("~{}h", hours.round() as u64)
    } else {
        format!("~{}m", (hours * 60.0).round().max(1.0) as u64)
    };
    format!("filling · full in {when}")
}

/// A cached measurement's age, e.g. `2m ago` / `just now`. `None` reads `—`.
fn fmt_age(age_secs: Option<u64>) -> String {
    match age_secs {
        None => "—".into(),
        Some(s) if s < 5 => "just now".into(),
        Some(s) if s < 60 => format!("{s}s ago"),
        Some(s) if s < 3600 => format!("{}m ago", s / 60),
        Some(s) if s < 86_400 => format!("{}h ago", s / 3600),
        Some(s) => format!("{}d ago", s / 86_400),
    }
}

fn free_tone(pct: u8) -> Tok {
    if pct <= 5 {
        Tok::Hue(Hue::Red)
    } else if pct <= 15 {
        Tok::Hue(Hue::Amber)
    } else {
        Tok::Slot(S::Text)
    }
}

fn kind_str(k: thegn_metrics::DiskKind) -> &'static str {
    match k {
        thegn_metrics::DiskKind::Ssd => "ssd",
        thegn_metrics::DiskKind::Hdd => "hdd",
        thegn_metrics::DiskKind::Unknown => "",
    }
}

// --- GPU -----------------------------------------------------------------

fn gpu(cx: &Ctx) -> Vec<Section> {
    let s = &cx.model.stats;
    let mut out = vec![cx.graph(Metric::Gpu, MAIN_H, Tok::Hue(Hue::Teal))];
    out.push(spacer());
    out.push(heading("device", None));
    let mut cells = Vec::new();
    if let Some(p) = s.gpu_pct {
        cells.push(kv("utilization", format!("{p}%"), Tok::Hue(Hue::Teal)));
    }
    if let Some((u, t)) = s.gpu_mem_mib {
        cells.push(kv("vram", format!("{u} / {t} MiB"), Tok::Slot(S::Text)));
        if t > 0 {
            cells.push(kv(
                "vram used",
                format!("{:.0}%", u as f32 / t as f32 * 100.0),
                Tok::Slot(S::Dim),
            ));
        }
    }
    if let Some(c) = s.gpu_temp_c {
        cells.push(kv("temperature", Unit::Celsius.fmt(c), temp_tone(c)));
    }
    if let Some(w) = s.gpu_power_w {
        cells.push(kv("power", Unit::Watts.fmt(w), Tok::Slot(S::Dim)));
    }
    out.push(Section::Grid { cols: 2, cells });
    if s.gpu_mem_mib.is_some() {
        out.push(cx.sparkrow(Metric::GpuMem, "vram", Tok::Hue(Hue::Purple)));
    }
    if s.gpu_power_w.is_some() {
        out.push(cx.sparkrow(Metric::GpuPower, "power", Tok::Hue(Hue::Amber)));
    }
    out
}

// --- Power ---------------------------------------------------------------

fn power(cx: &Ctx) -> Vec<Section> {
    let s = &cx.model.stats;
    let Some((pct, on_ac)) = s.battery else {
        return vec![heading("no battery on this machine", None)];
    };
    let tone = if on_ac {
        Tok::Hue(Hue::Green)
    } else if pct <= 15 {
        Tok::Hue(Hue::Red)
    } else if pct <= 30 {
        Tok::Hue(Hue::Amber)
    } else {
        Tok::Hue(Hue::Blue)
    };
    let mut out = vec![cx.graph(Metric::Battery, MAIN_H, tone)];

    out.push(spacer());
    out.push(heading(
        "battery",
        Some(if on_ac {
            "on AC".into()
        } else {
            "discharging".into()
        }),
    ));
    let mut cells = vec![
        kv("charge", format!("{pct}%"), tone),
        kv(
            "source",
            if on_ac { "AC".into() } else { "battery".into() },
            Tok::Slot(S::Dim),
        ),
    ];
    if let Some(w) = s.battery_power_w {
        cells.push(kv("draw", Unit::Watts.fmt(w), Tok::Slot(S::Dim)));
    }
    if let Some(secs) = s.battery_eta_secs {
        cells.push(kv(
            if on_ac { "to full" } else { "to empty" },
            fmt_eta(secs),
            Tok::Slot(S::Text),
        ));
    }
    out.push(Section::Grid { cols: 2, cells });
    if s.battery_power_w.is_some() {
        out.push(cx.sparkrow(Metric::BatteryPower, "draw", Tok::Hue(Hue::Amber)));
    }
    out
}

fn fmt_eta(secs: u64) -> String {
    let (h, m) = (secs / 3600, (secs % 3600) / 60);
    if h > 0 {
        format!("{h}h {m:02}m")
    } else {
        format!("{m}m")
    }
}

// --- Processes -----------------------------------------------------------

#[allow(clippy::too_many_arguments)] // one call site; every argument is real view state
fn procs(
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

    let body: Vec<Vec<Cell>> = rows
        .iter()
        .map(|p| {
            // The owner tint is the ONLY foreground rule here: the cursor row is
            // the table's `sel`, painted as a background, so selecting a row no
            // longer erases whose process it is.
            let name_tone = owner_tone(p.owner);
            // Tree indent: two spaces per depth, with an elision marker on a row
            // whose real parent fell outside the kept top-N set.
            let mut name = String::new();
            if tree && p.depth > 0 {
                name.push_str(&"  ".repeat(p.depth));
            }
            if p.elided_parent && tree {
                name.push_str("… ");
            }
            name.push_str(&p.name);
            let name_budget = 24usize.saturating_sub(p.depth * 2);
            vec![
                Cell::Text(format!("{:>7}", p.pid), Tok::Slot(S::Ghost)),
                Cell::Text(trunc(&name, name_budget.max(6)), name_tone),
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
    out.push(Section::Table(TableSection {
        header: vec![
            "pid".into(),
            "name".into(),
            "owner".into(),
            "cpu".into(),
            "mem".into(),
        ],
        rows: body,
        sel: Some(sel),
    }));
    TabBuild {
        sections: out,
        row_y,
    }
}

// --- Containers ----------------------------------------------------------

fn containers(cx: &Ctx, sel: usize) -> TabBuild {
    use thegn_core::sandbox_manage::{Health, container_health, human_bytes};
    let list = &cx.model.containers;

    // Aggregate footprint header. Owned counts are precise (ownership-filtered
    // listings); the byte total is the engine-wide `df` total, marked partial
    // when a detected engine has no `df` op.
    let owned = list.iter().filter(|c| c.ours).count();
    // The list deliberately includes containers thegn does not own (each row is
    // marked "(foreign)"), so the HEADING may not claim them — it used to read
    // "thegn containers" over a mixed list. The ownership split lives in the
    // note instead, where both counts are visible.
    let foreign = list.iter().filter(|c| !c.ours).count();
    let running = list
        .iter()
        .filter(|c| c.ours && thegn_core::sandbox_manage::container_running(&c.status))
        .count();
    let split = format!("{owned} owned · {foreign} foreign");
    let note = match &cx.model.container_footprint {
        Some(fp) => {
            let bytes = human_bytes(fp.total_bytes());
            format!(
                "{split} · {} img · {} vol · {}{} engine disk",
                fp.images,
                fp.volumes,
                if fp.partial { "≥" } else { "" },
                bytes,
            )
        }
        None => format!("{split} · {running} running"),
    };
    let mut out = vec![heading("containers", Some(note))];

    if list.is_empty() {
        out.push(heading("no containers", None));
        return plain(out);
    }

    let rows: Vec<Vec<Cell>> = list
        .iter()
        .map(|c| {
            // Ownership is the SOLE foreground rule. The cursor used to
            // overwrite it, which destroyed the ours/foreign signal the whole
            // tab is built on for exactly the row the user was acting upon; the
            // cursor is now the table's `sel` background instead.
            let name_tone = if c.ours {
                Tok::Hue(Hue::Green)
            } else {
                Tok::Slot(S::Ghost)
            };
            let (health_txt, health_tone) = match container_health(&c.status) {
                Health::Healthy => ("healthy", Tok::Hue(Hue::Green)),
                Health::Unhealthy => ("unhealthy", Tok::Hue(Hue::Red)),
                Health::Starting => ("starting", Tok::Hue(Hue::Amber)),
                Health::None => ("up", Tok::Slot(S::Text)),
                Health::Stopped => ("stopped", Tok::Slot(S::Dim)),
            };
            // Foreign containers are visibly read-only.
            let owned_mark = if c.ours { "" } else { " (foreign)" };
            vec![
                Cell::Text(format!("{}{owned_mark}", trunc(&c.name, 26)), name_tone),
                Cell::Text(trunc(&c.backend, 14), Tok::Slot(S::Ghost)),
                Cell::Text(health_txt.into(), health_tone),
                Cell::Text(
                    if c.cpu.is_empty() {
                        "—".into()
                    } else {
                        c.cpu.clone()
                    },
                    Tok::Hue(Hue::Teal),
                ),
                Cell::Text(
                    if c.mem.is_empty() {
                        "—".into()
                    } else {
                        c.mem.clone()
                    },
                    Tok::Hue(Hue::Purple),
                ),
                Cell::Text(
                    if c.net.is_empty() {
                        "—".into()
                    } else {
                        c.net.clone()
                    },
                    Tok::Slot(S::Dim),
                ),
            ]
        })
        .collect();
    let row_y = row_ys(&out, rows.len(), true);
    out.push(Section::Table(TableSection {
        header: vec![
            "container".into(),
            "backend".into(),
            "health".into(),
            "cpu".into(),
            "mem".into(),
            "net".into(),
        ],
        rows,
        sel: Some(sel),
    }));
    TabBuild {
        sections: out,
        row_y,
    }
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

/// Truncate to `max` display cells with an ellipsis.
fn trunc(s: &str, max: usize) -> String {
    if crate::seg::cells(s) <= max {
        return s.to_string();
    }
    let mut out = crate::seg::take_cols(s, max.saturating_sub(1)).to_string();
    out.push('…');
    out
}
