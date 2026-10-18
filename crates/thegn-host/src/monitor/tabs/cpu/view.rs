use crate::telemetry::{Metric, ScaleMode, SeriesReq, TelemetryHistory, Window};
use crate::{
    compositor::Rect,
    monitor::{
        board::TabView,
        layout::{Node, PanelId, PanelSpec, Placed, Size, Variant, VariantKind},
    },
    sections::Section,
};
use termwiz::surface::Surface;
use thegn_metrics::collect::{
    CollectorId, CollectorSet,
    frame::MonitorFrame,
    health::Health,
    model::{
        cpu_freq::CpuFreqPart, cpu_irq::CpuIrqPart, cpu_sched::CpuSchedPart, cpu_stat::CpuStatPart,
        loadavg::LoadavgPart, psi::PsiPart,
    },
};

const USAGE: PanelId = PanelId("cpu.usage");
const CORES: PanelId = PanelId("cpu.cores");
const FREQ: PanelId = PanelId("cpu.freq");
const LOAD: PanelId = PanelId("cpu.load");
const IRQ: PanelId = PanelId("cpu.irq");
const SCHED: PanelId = PanelId("cpu.sched");
const IDLE: PanelId = PanelId("cpu.idle");
const CORE_HISTORY_SAMPLES: usize = 240;
const CORE_HISTORY_LIMIT: usize = 256;

const FULL_VARIANTS: [Variant; 3] = [
    Variant {
        kind: VariantKind::Full,
        min: (30, 7),
        pref: (72, 28),
    },
    Variant {
        kind: VariantKind::Compact,
        min: (22, 5),
        pref: (38, 9),
    },
    Variant {
        kind: VariantKind::Mini,
        min: (12, 3),
        pref: (20, 5),
    },
];
const MINI_VARIANTS: [Variant; 2] = [
    Variant {
        kind: VariantKind::Compact,
        min: (34, 6),
        pref: (44, 10),
    },
    Variant {
        kind: VariantKind::Mini,
        min: (34, 4),
        pref: (28, 6),
    },
];
const SPECS: [PanelSpec; 7] = [
    PanelSpec {
        id: USAGE,
        priority: 0,
        variants: &FULL_VARIANTS,
    },
    PanelSpec {
        id: CORES,
        priority: 1,
        variants: &FULL_VARIANTS,
    },
    PanelSpec {
        id: FREQ,
        priority: 2,
        variants: &FULL_VARIANTS,
    },
    PanelSpec {
        id: LOAD,
        priority: 3,
        variants: &FULL_VARIANTS,
    },
    PanelSpec {
        id: IRQ,
        priority: 4,
        variants: &MINI_VARIANTS,
    },
    PanelSpec {
        id: SCHED,
        priority: 5,
        variants: &MINI_VARIANTS,
    },
    PanelSpec {
        id: IDLE,
        priority: 6,
        variants: &FULL_VARIANTS,
    },
];

#[derive(Debug, Default, Clone)]
pub(in crate::monitor) struct CpuModel {
    cpu_stat: CpuStatPart,
    cpu_freq: CpuFreqPart,
    cpu_irq: CpuIrqPart,
    cpu_sched: CpuSchedPart,
    loadavg: LoadavgPart,
    psi: PsiPart,
    topology: thegn_metrics::collect::model::cpu_topology::CpuTopologyPart,
    idle: thegn_metrics::collect::model::cpu_idle::CpuIdlePart,
    cpu_history: Vec<f32>,
    load_history: Vec<f32>,
    core_busy_history: Vec<Vec<f32>>,
    cpu_stat_health: Health,
    cpu_freq_health: Health,
    cpu_irq_health: Health,
    loadavg_health: Health,
    psi_health: Health,
    sched_health: Health,
    topology_health: Health,
    idle_health: Health,
}

impl CpuModel {
    #[cfg(test)]
    pub(in crate::monitor) fn procs_running(&self) -> Option<u32> {
        self.cpu_stat.procs_running
    }

    pub(in crate::monitor) fn absorb_frame(&mut self, frame: &MonitorFrame) {
        let source = &frame.parts.cpu_stat;
        self.cpu_stat.total = source.total;
        self.cpu_stat.cores.clone_from(&source.cores);
        let core_count = source.cores.len().min(CORE_HISTORY_LIMIT);
        self.core_busy_history
            .resize_with(core_count, || Vec::with_capacity(CORE_HISTORY_SAMPLES));
        self.core_busy_history.truncate(core_count);
        for (history, core) in self.core_busy_history.iter_mut().zip(&source.cores) {
            let value = core
                .share
                .map(|share| f32::from(share.busy) / 1000.0)
                .unwrap_or(f32::NAN);
            if history.len() == CORE_HISTORY_SAMPLES {
                history.rotate_left(1);
                history[CORE_HISTORY_SAMPLES - 1] = value;
            } else {
                history.push(value);
            }
        }
        self.cpu_stat.ctxt_per_s = source.ctxt_per_s;
        self.cpu_stat.intr_per_s = source.intr_per_s;
        self.cpu_stat.softirq_per_s = source.softirq_per_s;
        self.cpu_stat.forks_per_s = source.forks_per_s;
        self.cpu_stat.procs_running = source.procs_running;
        self.cpu_stat.procs_blocked = source.procs_blocked;
        self.cpu_stat.btime = source.btime;
        self.cpu_stat.steal_seen = source.steal_seen;
        self.cpu_stat.truncated = source.truncated;

        let source = &frame.parts.cpu_freq;
        self.cpu_freq.policies.clone_from(&source.policies);
        self.cpu_freq.power = source.power;

        let source = &frame.parts.cpu_irq;
        self.cpu_irq.cores.clone_from(&source.cores);
        self.cpu_irq.softirq_top = source.softirq_top;
        self.cpu_irq.thr_per_s = source.thr_per_s;
        self.cpu_irq.mce_per_s = source.mce_per_s;
        self.cpu_irq.dfr_per_s = source.dfr_per_s;
        self.cpu_irq.numeric_top.clone_from(&source.numeric_top);

        let source = &frame.parts.cpu_sched;
        self.cpu_sched.cores.clone_from(&source.cores);
        self.cpu_sched.version = source.version;
        self.loadavg.clone_from(&frame.parts.loadavg);
        self.psi.clone_from(&frame.parts.psi);
        self.topology.clone_from(&frame.parts.cpu_topology);
        self.idle.clone_from(&frame.parts.cpu_idle);
        self.cpu_stat_health = frame.health[CollectorId::CpuStat.index()];
        self.cpu_freq_health = frame.health[CollectorId::CpuFreq.index()];
        self.cpu_irq_health = frame.health[CollectorId::CpuIrq.index()];
        self.loadavg_health = frame.health[CollectorId::Loadavg.index()];
        self.psi_health = frame.health[CollectorId::Psi.index()];
        self.sched_health = frame.health[CollectorId::CpuSched.index()];
        self.topology_health = frame.health[CollectorId::CpuTopology.index()];
        self.idle_health = frame.health[CollectorId::CpuIdle.index()];
    }

    pub(in crate::monitor) fn absorb_history(
        &mut self,
        history: &TelemetryHistory,
        window: Window,
        scale: ScaleMode,
        now_ms: u64,
    ) {
        for (metric, slot) in [
            (Metric::Cpu, &mut self.cpu_history),
            (Metric::Load, &mut self.load_history),
        ] {
            let series = history.series(&SeriesReq {
                metric,
                window,
                scale: TelemetryHistory::scale_for(metric, scale),
                buckets: 240,
                agg: thegn_core::series::Agg::Max,
                now_ms,
            });
            if series.empty {
                slot.clear();
            } else {
                slot.clone_from(&series.hi);
            }
        }
    }

    fn sched_supported(&self) -> bool {
        !matches!(self.sched_health, Health::Absent(_))
    }
}

pub(in crate::monitor) struct CpuView<'a>(pub &'a CpuModel);

impl TabView for CpuView<'_> {
    fn specs(&self) -> &'static [PanelSpec] {
        &SPECS
    }

    fn template(&self) -> Node {
        Node::Col(vec![
            (
                Size::Weight(2),
                Node::Row(vec![
                    (Size::Weight(2), Node::Panel(USAGE)),
                    (Size::Weight(1), Node::Panel(LOAD)),
                ]),
            ),
            (
                Size::Weight(2),
                Node::Row(vec![
                    (Size::Weight(2), Node::Panel(CORES)),
                    (Size::Weight(1), Node::Panel(IDLE)),
                ]),
            ),
            (
                Size::Weight(1),
                Node::Row(vec![
                    (Size::Weight(1), Node::Panel(FREQ)),
                    (Size::Weight(1), Node::Panel(IRQ)),
                    (Size::Weight(1), Node::Panel(SCHED)),
                ]),
            ),
        ])
    }

    fn demand(&self, panel: PanelId) -> CollectorSet {
        let mut set = CollectorSet::empty();
        match panel {
            USAGE => set.insert(CollectorId::CpuStat),
            CORES => {
                set.insert(CollectorId::CpuStat);
                set.insert(CollectorId::CpuTopology);
            }
            IDLE => set.insert(CollectorId::CpuIdle),
            FREQ => set.insert(CollectorId::CpuFreq),
            LOAD => {
                set.insert(CollectorId::CpuStat);
                set.insert(CollectorId::Loadavg);
                set.insert(CollectorId::Psi);
            }
            IRQ => set.insert(CollectorId::CpuIrq),
            SCHED if self.0.sched_supported() => set.insert(CollectorId::CpuSched),
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
        paint::paint(self.0, surface, placed.id, rect);
    }
}

mod paint;

#[cfg(test)]
#[path = "cpu_view_tests.rs"]
mod tests;
