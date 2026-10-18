//! First real board view: a compact summary of the latest system frame.
use crate::telemetry::{Metric, ScaleMode, SeriesReq, TelemetryHistory, Window};
use crate::{
    compositor::Rect,
    monitor::{
        board::TabView,
        kit::{self, grid::CellGrid},
        layout::{Node, PanelId, PanelSpec, Placed, Size, Variant, VariantKind},
    },
    sections::Section,
    seg::{StyleTok, Tok},
};
use termwiz::surface::Surface;
use thegn_core::series::Agg;
use thegn_core::{
    mem_health::{self, MemHealthInput},
    numfmt::{NumFmt, Reading, Unit},
    theme::{RampId, StateRole},
};
use thegn_metrics::{
    StatsSnapshot,
    collect::{CollectorId, CollectorSet, frame::MonitorFrame, model::psi::PsiPart},
};

#[path = "overview/panels.rs"]
mod panels;

const IDS: [PanelId; 8] = [
    PanelId("overview.cpu"),
    PanelId("overview.load"),
    PanelId("overview.memory"),
    PanelId("overview.network"),
    PanelId("overview.disk"),
    PanelId("overview.thermal"),
    PanelId("overview.power"),
    PanelId("overview.activity"),
];
#[derive(Clone, Copy)]
struct PanelMeta {
    id: PanelId,
    title: &'static str,
    priority: u8,
    ramp: RampId,
    metrics: [Option<Metric>; 2],
}
const PANELS: [PanelMeta; 8] = [
    PanelMeta {
        id: IDS[0],
        title: "CPU",
        priority: 0,
        ramp: RampId::Util,
        metrics: [Some(Metric::Cpu), None],
    },
    PanelMeta {
        id: IDS[1],
        title: "Load / pressure",
        priority: 2,
        ramp: RampId::Util,
        metrics: [Some(Metric::Load), None],
    },
    PanelMeta {
        id: IDS[2],
        title: "Memory",
        priority: 1,
        ramp: RampId::Mem,
        metrics: [Some(Metric::Mem), None],
    },
    PanelMeta {
        id: IDS[3],
        title: "Network",
        priority: 3,
        ramp: RampId::Rx,
        metrics: [Some(Metric::NetRx), None],
    },
    PanelMeta {
        id: IDS[4],
        title: "Disk",
        priority: 4,
        ramp: RampId::Read,
        metrics: [Some(Metric::DiskRead), Some(Metric::DiskWrite)],
    },
    PanelMeta {
        id: IDS[5],
        title: "Thermal",
        priority: 5,
        ramp: RampId::Temp,
        metrics: [Some(Metric::Temp), None],
    },
    PanelMeta {
        id: IDS[6],
        title: "Power",
        priority: 6,
        ramp: RampId::Power,
        metrics: [Some(Metric::Battery), None],
    },
    PanelMeta {
        id: IDS[7],
        title: "Processes / containers",
        priority: 7,
        ramp: RampId::Util,
        metrics: [None, None],
    },
];
const VARIANTS: [Variant; 1] = [Variant {
    kind: VariantKind::Full,
    min: (12, 5),
    pref: (32, 12),
}];
const SPECS: [PanelSpec; 8] = [
    PanelSpec {
        id: IDS[0],
        priority: PANELS[0].priority,
        variants: &VARIANTS,
    },
    PanelSpec {
        id: IDS[1],
        priority: PANELS[1].priority,
        variants: &VARIANTS,
    },
    PanelSpec {
        id: IDS[2],
        priority: PANELS[2].priority,
        variants: &VARIANTS,
    },
    PanelSpec {
        id: IDS[3],
        priority: PANELS[3].priority,
        variants: &VARIANTS,
    },
    PanelSpec {
        id: IDS[4],
        priority: PANELS[4].priority,
        variants: &VARIANTS,
    },
    PanelSpec {
        id: IDS[5],
        priority: PANELS[5].priority,
        variants: &VARIANTS,
    },
    PanelSpec {
        id: IDS[6],
        priority: PANELS[6].priority,
        variants: &VARIANTS,
    },
    PanelSpec {
        id: IDS[7],
        priority: PANELS[7].priority,
        variants: &VARIANTS,
    },
];

#[derive(Debug, Default, Clone)]
pub(in crate::monitor) struct OverviewModel {
    stats: StatsSnapshot,
    sparks: Vec<[Vec<f32>; 2]>,
    container_count: usize,
    cpu_cores: Vec<u16>,
    procs_running: Option<u32>,
    load: Option<thegn_metrics::collect::parse::loadavg::Loadavg>,
    psi: PsiPart,
    mem_total: Option<u64>,
    mem_available: Option<u64>,
    swap_used: Option<u64>,
    net_ifaces: usize,
    disk_read_bps: Option<f64>,
    disk_write_bps: Option<f64>,
    hottest_c: Option<f32>,
    thermal_zones: usize,
    battery: Option<(
        Option<u8>,
        Option<thegn_metrics::collect::model::power_supply::SupplyStatus>,
    )>,
    health: [thegn_metrics::collect::health::Health; thegn_metrics::collect::COLLECTORS],
}
impl OverviewModel {
    #[cfg(test)]
    pub(in crate::monitor) fn procs_running(&self) -> Option<u32> {
        self.procs_running
    }
    pub(in crate::monitor) fn absorb_stats(&mut self, stats: &StatsSnapshot) {
        self.stats.clone_from(stats);
    }
    pub(in crate::monitor) fn absorb_container_count(&mut self, count: usize) {
        self.container_count = count;
    }
    pub(in crate::monitor) fn absorb_history(
        &mut self,
        history: &TelemetryHistory,
        window: Window,
        scale: ScaleMode,
        now_ms: u64,
    ) {
        self.sparks
            .resize_with(PANELS.len(), || std::array::from_fn(|_| Vec::new()));
        for (index, slots) in self.sparks.iter_mut().enumerate() {
            for (series_index, slot) in slots.iter_mut().enumerate() {
                let Some(metric) = PANELS[index].metrics[series_index] else {
                    slot.clear();
                    continue;
                };
                let series = history.series(&SeriesReq {
                    metric,
                    window,
                    scale: TelemetryHistory::scale_for(metric, scale),
                    buckets: 64,
                    agg: Agg::Max,
                    now_ms,
                });
                if series.empty {
                    slot.clear();
                } else {
                    slot.clone_from(&series.hi);
                }
            }
        }
    }
    pub(in crate::monitor) fn absorb_frame(&mut self, frame: &MonitorFrame) {
        let cpu = &frame.parts.cpu_stat;
        self.cpu_cores.clear();
        self.cpu_cores.extend(
            cpu.cores
                .iter()
                .filter_map(|core| core.share.map(|share| share.busy)),
        );
        self.procs_running = cpu.procs_running;
        self.load = frame.parts.loadavg.value;
        self.psi.clone_from(&frame.parts.psi);
        let mem = &frame.parts.meminfo;
        self.mem_total = mem
            .raw
            .get(thegn_metrics::collect::parse::meminfo::MemField::MemTotal);
        self.mem_available = mem
            .raw
            .get(thegn_metrics::collect::parse::meminfo::MemField::MemAvailable);
        self.swap_used = mem.swap_used_bytes();
        self.net_ifaces = frame.parts.netdev.ifaces.len();
        self.disk_read_bps = frame.parts.diskstats.aggregate.read_bps;
        self.disk_write_bps = frame.parts.diskstats.aggregate.write_bps;
        self.hottest_c = frame
            .parts
            .thermal_zone
            .zones
            .iter()
            .filter_map(|z| z.temp_c)
            .chain(frame.parts.hwmon.values.iter().filter_map(|v| v.value))
            .reduce(f32::max);
        self.thermal_zones = frame.parts.thermal_zone.zones.len();
        self.battery = frame
            .parts
            .power_supply
            .supplies
            .iter()
            .find(|s| s.kind == thegn_metrics::collect::model::power_supply::SupplyKind::Battery)
            .map(|s| (s.capacity_pct, s.status));
        self.health.copy_from_slice(&frame.health);
    }
    fn source_present(&self, ids: &[CollectorId], fallback: bool) -> bool {
        fallback
            || ids.iter().any(|id| {
                !matches!(
                    self.health[id.index()],
                    thegn_metrics::collect::health::Health::Absent(_)
                )
            })
    }
    fn present(&self, index: usize) -> bool {
        match index {
            0 => self.source_present(&[CollectorId::CpuStat], self.stats.cpu_pct.is_some()),
            1 => self.source_present(
                &[CollectorId::Loadavg, CollectorId::Psi],
                self.stats.load_avg.is_some(),
            ),
            2 => self.source_present(&[CollectorId::Meminfo], self.stats.mem_gib.is_some()),
            3 => self.source_present(&[CollectorId::Netdev], self.stats.net_bps.is_some()),
            4 => self.source_present(
                &[CollectorId::Diskstats],
                self.stats.disk_free_pct.is_some(),
            ),
            5 => self.source_present(
                &[CollectorId::Hwmon, CollectorId::ThermalZone],
                self.stats.cpu_temp_c.is_some(),
            ),
            6 => self.source_present(&[CollectorId::PowerSupply], self.stats.battery.is_some()),
            _ => true,
        }
    }
    fn health(&self) -> mem_health::MemHealth {
        mem_health::evaluate(&MemHealthInput {
            mem_total_bytes: self.mem_total,
            mem_available_bytes: self.mem_available,
            psi_some_avg10: self
                .psi
                .memory
                .file
                .some
                .map(|line| f64::from(line.avg10) / 100.0),
            psi_full_avg10: self
                .psi
                .memory
                .file
                .full
                .map(|line| f64::from(line.avg10) / 100.0),
            ..Default::default()
        })
    }
}

pub(in crate::monitor) struct OverviewView<'a>(pub &'a OverviewModel);
impl TabView for OverviewView<'_> {
    fn specs(&self) -> &'static [PanelSpec] {
        &SPECS
    }
    fn template(&self) -> Node {
        Node::Col(
            (0..2)
                .filter_map(|row| {
                    let children = (row * 4..row * 4 + 4)
                        .filter(|index| self.0.present(*index))
                        .map(|index| (Size::Weight(1), Node::Panel(IDS[index])))
                        .collect::<Vec<_>>();
                    (!children.is_empty()).then_some((Size::Weight(1), Node::Row(children)))
                })
                .collect(),
        )
    }
    fn demand(&self, panel: PanelId) -> CollectorSet {
        let mut set = CollectorSet::empty();
        let Some(index) = PANELS.iter().position(|meta| meta.id == panel) else {
            return set;
        };
        match index {
            0 | 7 => set.insert(CollectorId::CpuStat),
            1 => {
                set.insert(CollectorId::Loadavg);
                set.insert(CollectorId::Psi);
            }
            2 => set.insert(CollectorId::Meminfo),
            3 => set.insert(CollectorId::Netdev),
            4 => set.insert(CollectorId::Diskstats),
            5 => {
                set.insert(CollectorId::Hwmon);
                set.insert(CollectorId::ThermalZone);
            }
            6 => set.insert(CollectorId::PowerSupply),
            _ => {}
        }
        set
    }
    fn paint(
        &self,
        surface: &mut Surface,
        placed: &Placed,
        rect: Rect,
        _scroll: usize,
        _sections: &[Section],
    ) {
        let Some(index) = PANELS.iter().position(|meta| meta.id == placed.id) else {
            return;
        };
        let mut grid = CellGrid::new(rect.cols, rect.rows);
        let title = PANELS[index].title;
        let (badge, lines, tone) = self.panel_content(index);
        let content_rect;
        {
            let mut body = kit::panel::panel(
                &mut grid,
                Rect::full(rect.cols, rect.rows),
                title,
                badge.as_deref(),
            );
            content_rect = body.rect();
            for (row, line) in lines.iter().take(body.rect().rows).enumerate() {
                body.put_str(0, row, line, kit_style(tone));
            }
        }
        let mut sparks = Vec::new();
        if let Some(values) = self.0.sparks.get(index) {
            let start = lines.len().saturating_add(1);
            for (series_index, series) in values.iter().enumerate() {
                let y = start + series_index;
                if !series.is_empty() && y < content_rect.rows {
                    sparks.push((
                        series.as_slice(),
                        content_rect.x,
                        content_rect.y + y,
                        content_rect.cols,
                    ));
                }
            }
        }
        let mut emitter = kit::emit::Emitter::default();
        emitter.emit(
            &grid,
            Rect::full(rect.cols, rect.rows),
            surface,
            rect.x,
            rect.y,
        );
        for (values, x, y, cols) in sparks {
            crate::monitor::graphs::paint_sparkline(
                values,
                cols,
                PANELS[index].ramp,
                Rect {
                    x: rect.x + x,
                    y: rect.y + y,
                    cols,
                    rows: 1,
                },
                surface,
            );
        }
    }
}

fn kit_style(role: StateRole) -> kit::grid::CellStyle {
    kit::grid::CellStyle {
        fg: StyleTok::from_tok(Tok::State(role)).unwrap_or(StyleTok::INHERIT),
        ..Default::default()
    }
}

#[cfg(test)]
#[path = "overview_view_tests.rs"]
mod tests;
