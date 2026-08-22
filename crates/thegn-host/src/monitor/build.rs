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

use super::{GraphStyle, MonitorTab, ProcSort, TabPrefs};
use crate::chrome::{FrameModel, S};
use crate::detail::StatusCtx;
use crate::sections::{Cell, GraphSection, Section, TableSection, spacer};
use crate::seg::Tok;
use crate::telemetry::{Metric, SeriesOut, SeriesReq, TelemetryHistory};
use thegn_core::series::Agg;
use thegn_core::theme::Hue;
use thegn_core::viz::{self, Unit};

/// Plot height for a tab's headline graph.
const MAIN_H: usize = 6;
/// Plot height for a secondary graph (a per-interface or per-disk trace).
const SUB_H: usize = 3;

/// Build the active tab's section stack.
#[allow(clippy::too_many_arguments)] // one call site; every argument is real state
pub(super) fn tab(
    tab: MonitorTab,
    model: &FrameModel,
    ctx: &StatusCtx,
    prefs: TabPrefs,
    cols: usize,
    now_ms: u64,
    proc_sort: ProcSort,
    proc_desc: bool,
    sel: usize,
) -> Vec<Section> {
    let cx = Ctx {
        model,
        hist: ctx.hist,
        prefs,
        cols,
        now_ms,
    };
    match tab {
        MonitorTab::Cpu => cpu(&cx),
        MonitorTab::Memory => memory(&cx),
        MonitorTab::Thermal => thermal(&cx),
        MonitorTab::Network => network(&cx),
        MonitorTab::Disk => disk(&cx),
        MonitorTab::Gpu => gpu(&cx),
        MonitorTab::Power => power(&cx),
        MonitorTab::Procs => procs(&cx, proc_sort, proc_desc, sel),
    }
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
        }));
    }
    out
}

// --- Disk ----------------------------------------------------------------

fn disk(cx: &Ctx) -> Vec<Section> {
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
    }));

    if let Some((total, avail)) = s.disk_bytes {
        out.push(spacer());
        out.push(heading("worktrees filesystem", None));
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
    out
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

fn procs(cx: &Ctx, sort: ProcSort, desc: bool, sel: usize) -> Vec<Section> {
    let snap = &cx.model.procs;
    if !snap.enabled {
        return vec![heading(
            "process sampling is off ([monitor] processes = false)",
            None,
        )];
    }
    if snap.procs.is_empty() {
        // The first scan after the tab opens has no CPU delta yet. Say so
        // rather than showing an empty table, which reads as broken.
        return vec![heading("sampling…", None)];
    }
    let mut rows: Vec<&thegn_metrics::ProcSample> = snap.procs.iter().collect();
    rows.sort_by(|a, b| {
        let ord = match sort {
            ProcSort::Cpu => a.cpu_pct.total_cmp(&b.cpu_pct),
            ProcSort::Rss => a.rss_bytes.cmp(&b.rss_bytes),
            ProcSort::Name => b.name.cmp(&a.name),
            ProcSort::Pid => b.pid.cmp(&a.pid),
        };
        if desc { ord.reverse() } else { ord }
    });

    let note = if snap.primed {
        format!("{} of {} processes", rows.len(), snap.total)
    } else {
        // CPU is a delta; the first sample has nothing to diff against.
        format!("{} of {} · cpu warming up", rows.len(), snap.total)
    };
    let mut out = vec![heading("processes", Some(note))];

    let body: Vec<Vec<Cell>> = rows
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let cur = i == sel;
            let name_tone = if cur {
                Tok::Slot(S::Accent)
            } else {
                owner_tone(p.owner)
            };
            vec![
                Cell::Text(format!("{:>7}", p.pid), Tok::Slot(S::Ghost)),
                Cell::Text(trunc(&p.name, 22), name_tone),
                Cell::Text(owner_label(p.owner), Tok::Slot(S::Ghost)),
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
    out.push(Section::Table(TableSection {
        header: vec![
            "pid".into(),
            "name".into(),
            "owner".into(),
            "cpu".into(),
            "mem".into(),
        ],
        rows: body,
    }));
    out
}

/// Tint a process by whose it is — thegn's own panes stand out from the rest.
fn owner_tone(o: thegn_metrics::ProcOwner) -> Tok {
    match o {
        thegn_metrics::ProcOwner::Other => Tok::Slot(S::Text),
        thegn_metrics::ProcOwner::ThegnSelf | thegn_metrics::ProcOwner::ThegnDaemon => {
            Tok::Hue(Hue::Green)
        }
        thegn_metrics::ProcOwner::Pane(_) => Tok::Hue(Hue::Blue),
    }
}

fn owner_label(o: thegn_metrics::ProcOwner) -> String {
    match o {
        thegn_metrics::ProcOwner::Other => String::new(),
        thegn_metrics::ProcOwner::ThegnSelf => "thegn".into(),
        thegn_metrics::ProcOwner::ThegnDaemon => "daemon".into(),
        thegn_metrics::ProcOwner::Pane(id) => format!("pane {id}"),
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
