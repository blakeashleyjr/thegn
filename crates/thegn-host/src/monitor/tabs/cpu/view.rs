#![allow(dead_code)] // wired into the overlay dispatch in batch 47 (see THE-871)

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
        min: (24, 6),
        pref: (44, 10),
    },
    Variant {
        kind: VariantKind::Mini,
        min: (16, 4),
        pref: (28, 6),
    },
];
const SPECS: [PanelSpec; 6] = [
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
];

#[derive(Debug, Default, Clone)]
pub(in crate::monitor) struct CpuModel {
    cpu_stat: CpuStatPart,
    cpu_freq: CpuFreqPart,
    cpu_irq: CpuIrqPart,
    cpu_sched: CpuSchedPart,
    loadavg: LoadavgPart,
    psi: PsiPart,
    cpu_stat_health: Health,
    cpu_freq_health: Health,
    cpu_irq_health: Health,
    loadavg_health: Health,
    psi_health: Health,
    sched_health: Health,
}

impl CpuModel {
    pub(in crate::monitor) fn absorb_frame(&mut self, frame: &MonitorFrame) {
        let source = &frame.parts.cpu_stat;
        self.cpu_stat.total = source.total;
        self.cpu_stat.cores.clone_from(&source.cores);
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
        self.cpu_stat_health = frame.health[CollectorId::CpuStat.index()];
        self.cpu_freq_health = frame.health[CollectorId::CpuFreq.index()];
        self.cpu_irq_health = frame.health[CollectorId::CpuIrq.index()];
        self.loadavg_health = frame.health[CollectorId::Loadavg.index()];
        self.psi_health = frame.health[CollectorId::Psi.index()];
        self.sched_health = frame.health[CollectorId::CpuSched.index()];
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
        let mut rows = vec![
            (
                Size::Min(12),
                Node::Row(vec![
                    (Size::Weight(1), Node::Panel(USAGE)),
                    (Size::Weight(1), Node::Panel(CORES)),
                ]),
            ),
            (
                Size::Min(12),
                Node::Row(vec![
                    (Size::Weight(1), Node::Panel(FREQ)),
                    (Size::Weight(1), Node::Panel(LOAD)),
                ]),
            ),
            (
                Size::Min(12),
                Node::Row(vec![
                    (Size::Weight(1), Node::Panel(IRQ)),
                    (Size::Weight(1), Node::Panel(SCHED)),
                ]),
            ),
        ];
        if matches!(self.0.sched_health, Health::Absent(_)) {
            rows[2].1 = Node::Panel(IRQ);
        }
        Node::Col(rows)
    }

    fn demand(&self, panel: PanelId) -> CollectorSet {
        let mut set = CollectorSet::empty();
        match panel {
            USAGE | CORES => set.insert(CollectorId::CpuStat),
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
