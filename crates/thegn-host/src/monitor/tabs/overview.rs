//! First real board view: a compact summary of the latest system frame.
use crate::telemetry::{Agg, Metric, ScaleMode, SeriesReq, TelemetryHistory, Window};
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
use thegn_core::{
    mem_health::{self, MemHealthInput},
    numfmt::{NumFmt, Reading, Unit},
    theme::{RampId, StateRole},
};
use thegn_metrics::{
    StatsSnapshot,
    collect::{
        CollectorId, CollectorSet,
        frame::MonitorFrame,
        model::{
            cpu_stat::CpuStatPart, diskstats::DiskstatsPart, hwmon::HwmonPart,
            loadavg::LoadavgPart, meminfo::MeminfoPart, netdev::NetdevPart,
            power_supply::PowerSupplyPart, psi::PsiPart, thermal_zone::ThermalZonePart,
        },
    },
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
const VARIANTS: [Variant; 1] = [Variant {
    kind: VariantKind::Full,
    min: (12, 5),
    pref: (32, 12),
}];
const SPECS: [PanelSpec; 8] = [
    PanelSpec {
        id: IDS[0],
        priority: 8,
        variants: &VARIANTS,
    },
    PanelSpec {
        id: IDS[1],
        priority: 7,
        variants: &VARIANTS,
    },
    PanelSpec {
        id: IDS[2],
        priority: 8,
        variants: &VARIANTS,
    },
    PanelSpec {
        id: IDS[3],
        priority: 6,
        variants: &VARIANTS,
    },
    PanelSpec {
        id: IDS[4],
        priority: 5,
        variants: &VARIANTS,
    },
    PanelSpec {
        id: IDS[5],
        priority: 4,
        variants: &VARIANTS,
    },
    PanelSpec {
        id: IDS[6],
        priority: 3,
        variants: &VARIANTS,
    },
    PanelSpec {
        id: IDS[7],
        priority: 2,
        variants: &VARIANTS,
    },
];

#[derive(Debug, Default, Clone)]
pub(in crate::monitor) struct OverviewModel {
    stats: StatsSnapshot,
    sparks: Vec<Vec<f32>>,
    container_count: usize,
    cpu: CpuStatPart,
    load: LoadavgPart,
    psi: PsiPart,
    mem: MeminfoPart,
    net: NetdevPart,
    disk: DiskstatsPart,
    hwmon: HwmonPart,
    thermal: ThermalZonePart,
    power: PowerSupplyPart,
}
impl OverviewModel {
    pub(in crate::monitor) fn absorb_stats(&mut self, stats: &StatsSnapshot) {
        self.stats.clone_from(stats);
    }
    pub(in crate::monitor) fn absorb_container_count(&mut self, count: usize) {
        self.container_count = count;
    }
    pub(in crate::monitor) fn absorb_history(&mut self, history: &TelemetryHistory, now_ms: u64) {
        const METRICS: [Metric; 8] = [
            Metric::Cpu,
            Metric::Load,
            Metric::Mem,
            Metric::NetRx,
            Metric::DiskRead,
            Metric::Temp,
            Metric::Battery,
            Metric::DiskWrite,
        ];
        self.sparks.resize_with(METRICS.len(), Vec::new);
        for (slot, metric) in self.sparks.iter_mut().zip(METRICS) {
            let series = history.series(&SeriesReq {
                metric,
                window: Window::from_secs(60),
                scale: TelemetryHistory::scale_for(metric, ScaleMode::Window),
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
    pub(in crate::monitor) fn absorb_frame(&mut self, frame: &MonitorFrame) {
        self.cpu.clone_from(&frame.parts.cpu_stat);
        self.load.clone_from(&frame.parts.loadavg);
        self.psi.clone_from(&frame.parts.psi);
        self.mem.clone_from(&frame.parts.meminfo);
        self.net.clone_from(&frame.parts.netdev);
        self.disk.clone_from(&frame.parts.diskstats);
        self.hwmon.clone_from(&frame.parts.hwmon);
        self.thermal.clone_from(&frame.parts.thermal_zone);
        self.power.clone_from(&frame.parts.power_supply);
    }
    fn present(&self, index: usize) -> bool {
        match index {
            0 => {
                self.stats.cpu_pct.is_some()
                    || self.cpu.total.is_some()
                    || !self.cpu.cores.is_empty()
            }
            1 => {
                self.load.value.is_some()
                    || self.stats.load_avg.is_some()
                    || self.psi.cpu.present
                    || self.psi.memory.present
            }
            2 => self.mem.used_bytes().is_some() || self.stats.mem_gib.is_some(),
            3 => self.stats.net_bps.is_some() || !self.net.ifaces.is_empty(),
            4 => {
                self.disk.aggregate.read_bps.is_some()
                    || self.disk.aggregate.write_bps.is_some()
                    || self.stats.disk_free_pct.is_some()
            }
            5 => {
                self.hwmon.values.iter().any(|v| v.value.is_some())
                    || self.thermal.zones.iter().any(|z| z.temp_c.is_some())
                    || self.stats.cpu_temp_c.is_some()
            }
            6 => !self.power.supplies.is_empty() || self.stats.battery.is_some(),
            _ => true,
        }
    }
    fn health(&self) -> mem_health::MemHealth {
        let total = self
            .mem
            .raw
            .get(thegn_metrics::collect::parse::meminfo::MemField::MemTotal);
        let available = self
            .mem
            .raw
            .get(thegn_metrics::collect::parse::meminfo::MemField::MemAvailable);
        mem_health::evaluate(MemHealthInput {
            mem_total_bytes: total,
            mem_available_bytes: available,
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
        let Some(index) = IDS.iter().position(|id| *id == panel) else {
            return set;
        };
        if !self.0.present(index) {
            return set;
        }
        let m = self.0;
        let mut add = |id| {
            set.insert(id);
        };
        match index {
            0 if m.cpu.total.is_some() || !m.cpu.cores.is_empty() => add(CollectorId::CpuStat),
            1 => {
                if m.load.value.is_some() {
                    add(CollectorId::Loadavg);
                }
                if m.psi.cpu.present || m.psi.memory.present {
                    add(CollectorId::Psi);
                }
            }
            2 if m
                .mem
                .raw
                .get(thegn_metrics::collect::parse::meminfo::MemField::MemTotal)
                .is_some()
                || m.mem
                    .raw
                    .get(thegn_metrics::collect::parse::meminfo::MemField::MemAvailable)
                    .is_some()
                || m.mem.swap_used_bytes().is_some() =>
            {
                add(CollectorId::Meminfo)
            }
            3 if !m.net.ifaces.is_empty() => add(CollectorId::Netdev),
            4 if m.disk.aggregate.read_bps.is_some() || m.disk.aggregate.write_bps.is_some() => {
                add(CollectorId::Diskstats)
            }
            5 => {
                if m.hwmon.values.iter().any(|v| v.value.is_some()) {
                    add(CollectorId::Hwmon);
                }
                if m.thermal.zones.iter().any(|z| z.temp_c.is_some()) {
                    add(CollectorId::ThermalZone);
                }
            }
            6 if !m.power.supplies.is_empty() => add(CollectorId::PowerSupply),
            7 if m.cpu.procs_running.is_some() => add(CollectorId::CpuStat),
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
        let Some(index) = IDS.iter().position(|id| *id == placed.id) else {
            return;
        };
        let mut grid = CellGrid::new(rect.cols, rect.rows);
        let title = [
            "CPU",
            "Load / pressure",
            "Memory",
            "Network",
            "Disk",
            "Thermal",
            "Power",
            "Processes / containers",
        ][index];
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
        if let Some(values) = self.0.sparks.get(index).filter(|values| !values.is_empty()) {
            let ramps = [
                RampId::Util,
                RampId::Util,
                RampId::Mem,
                RampId::Rx,
                RampId::Read,
                RampId::Temp,
                RampId::Power,
                RampId::Write,
            ];
            let start = lines.len().saturating_add(1);
            let mut meter = kit::meter::Meter::default();
            for (row, value) in values
                .iter()
                .rev()
                .take(content_rect.rows.saturating_sub(start))
                .enumerate()
            {
                meter.paint(
                    &mut grid,
                    Rect {
                        x: content_rect.x,
                        y: content_rect.y + start + row,
                        cols: content_rect.cols,
                        rows: 1,
                    },
                    kit::meter::MeterSpec {
                        value: value.clamp(0.0, 1.0),
                        warn: 0.7,
                        crit: 0.9,
                        ramp: ramps[index],
                        label: false,
                        fmt: NumFmt::default(),
                    },
                );
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
