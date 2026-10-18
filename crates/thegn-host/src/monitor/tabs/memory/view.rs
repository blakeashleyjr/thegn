#![allow(dead_code)] // wired into the overlay dispatch in batch 47 (see THE-871)

use crate::{
    compositor::Rect,
    monitor::{
        board::TabView,
        kit::{self, emit::Emitter, grid::CellGrid, meter::Meter, panel::panel},
        layout::{Node, PanelId, PanelSpec, Placed, Size, Variant, VariantKind},
    },
    seg::{StyleTok, Tok},
};
use termwiz::surface::Surface;
use thegn_core::{
    mem_health::{self, MemHealth, MemHealthInput, Reason, Verdict},
    numfmt::{NumFmt, Reading, Unit},
    theme::{RampId, SeriesRole, StateRole},
};
use thegn_metrics::collect::{
    CollectorId, CollectorSet, frame::MonitorFrame, parse::meminfo::MemField,
};

const COMPOSITION: PanelId = PanelId("memory.composition");
const VERDICT: PanelId = PanelId("memory.verdict");
const PRESSURE: PanelId = PanelId("memory.pressure");
const SWAP: PanelId = PanelId("memory.swap");
const PAGING: PanelId = PanelId("memory.paging");
const COMPOSITION_VARIANTS: [Variant; 1] = [Variant {
    kind: VariantKind::Full,
    min: (36, 8),
    pref: (72, 19),
}];
const VERDICT_VARIANTS: [Variant; 1] = [Variant {
    kind: VariantKind::Full,
    min: (36, 5),
    pref: (72, 8),
}];
const PRESSURE_VARIANTS: [Variant; 1] = [Variant {
    kind: VariantKind::Full,
    min: (36, 7),
    pref: (72, 10),
}];
const SWAP_VARIANTS: [Variant; 1] = [Variant {
    kind: VariantKind::Full,
    min: (36, 4),
    pref: (72, 7),
}];
const PAGING_VARIANTS: [Variant; 1] = [Variant {
    kind: VariantKind::Full,
    min: (36, 8),
    pref: (72, 9),
}];
const SPECS: [PanelSpec; 5] = [
    PanelSpec {
        id: COMPOSITION,
        priority: 0,
        variants: &COMPOSITION_VARIANTS,
    },
    PanelSpec {
        id: VERDICT,
        priority: 1,
        variants: &VERDICT_VARIANTS,
    },
    PanelSpec {
        id: PRESSURE,
        priority: 2,
        variants: &PRESSURE_VARIANTS,
    },
    PanelSpec {
        id: SWAP,
        priority: 3,
        variants: &SWAP_VARIANTS,
    },
    PanelSpec {
        id: PAGING,
        priority: 4,
        variants: &PAGING_VARIANTS,
    },
];

#[derive(Debug, Clone, PartialEq)]
pub(in crate::monitor) struct MemoryModel {
    pub total: Option<u64>,
    pub available: Option<u64>,
    pub used: Option<u64>,
    pub free: Option<u64>,
    pub buffers: Option<u64>,
    pub cache: Option<u64>,
    pub shmem: Option<u64>,
    pub slab: Option<u64>,
    pub anon: Option<u64>,
    pub file: Option<u64>,
    pub dirty: Option<u64>,
    pub writeback: Option<u64>,
    pub swap_total: Option<u64>,
    pub swap_free: Option<u64>,
    pub zswap: Option<u64>,
    pub zram_used: Option<u64>,
    pub hugepages_total: Option<u64>,
    pub hugepages_free: Option<u64>,
    pub reclaim: Option<f64>,
    pub scan: Option<f64>,
    pub steal: Option<f64>,
    pub refaults: Option<f64>,
    pub allocstall: Option<f64>,
    pub major_faults: Option<f64>,
    pub oom_kill: Option<f64>,
    last_oom_at_ms: Option<u64>,
    pub psi_some_avg10: Option<f64>,
    pub psi_some_avg60: Option<f64>,
    pub psi_full_avg10: Option<f64>,
    pub psi_full_avg60: Option<f64>,
    pub health: MemHealth,
}

impl Default for MemoryModel {
    fn default() -> Self {
        Self {
            total: None,
            available: None,
            used: None,
            free: None,
            buffers: None,
            cache: None,
            shmem: None,
            slab: None,
            anon: None,
            file: None,
            dirty: None,
            writeback: None,
            swap_total: None,
            swap_free: None,
            zswap: None,
            zram_used: None,
            hugepages_total: None,
            hugepages_free: None,
            reclaim: None,
            scan: None,
            steal: None,
            refaults: None,
            allocstall: None,
            major_faults: None,
            oom_kill: None,
            last_oom_at_ms: None,
            psi_some_avg10: None,
            psi_some_avg60: None,
            psi_full_avg10: None,
            psi_full_avg60: None,
            health: mem_health::evaluate(&MemHealthInput::default()),
        }
    }
}

impl MemoryModel {
    pub(in crate::monitor) fn absorb_frame(&mut self, frame: &MonitorFrame) {
        let mem = &frame.parts.meminfo.raw;
        self.total = mem.get(MemField::MemTotal);
        self.available = mem.get(MemField::MemAvailable);
        self.used = self
            .total
            .zip(self.available)
            .map(|(total, available)| total.saturating_sub(available));
        self.free = mem.get(MemField::MemFree);
        self.buffers = mem.get(MemField::Buffers);
        self.cache = mem.get(MemField::Cached);
        self.shmem = mem.get(MemField::Shmem);
        self.slab = mem.get(MemField::Slab);
        self.anon = mem.get(MemField::AnonPages);
        self.file = mem
            .get(MemField::ActiveFile)
            .zip(mem.get(MemField::InactiveFile))
            .map(|(active, inactive)| active.saturating_add(inactive));
        self.dirty = mem.get(MemField::Dirty);
        self.writeback = mem.get(MemField::Writeback);
        self.swap_total = mem.get(MemField::SwapTotal);
        self.swap_free = mem.get(MemField::SwapFree);
        self.zswap = mem.get(MemField::Zswap);
        self.zram_used = (!frame.parts.meminfo.zram.is_empty()).then(|| {
            frame
                .parts
                .meminfo
                .zram
                .iter()
                .filter_map(|device| device.mm)
                .fold(0_u64, |sum, stat| sum.saturating_add(stat.mem_used_total))
        });
        self.hugepages_total = mem.get(MemField::HugePagesTotal);
        self.hugepages_free = mem.get(MemField::HugePagesFree);

        use thegn_metrics::collect::model::vmstat::VmRate;
        let vm = &frame.parts.vmstat;
        self.reclaim = vm.rates[VmRate::StealDirect as usize];
        self.scan = sum_rates(
            vm.rates[VmRate::ScanKswapd as usize],
            vm.rates[VmRate::ScanDirect as usize],
        );
        self.steal = vm.rates[VmRate::StealKswapd as usize];
        self.refaults = sum_rates(
            vm.rates[VmRate::RefaultAnon as usize],
            vm.rates[VmRate::RefaultFile as usize],
        );
        self.allocstall = vm.rates[VmRate::Allocstall as usize];
        self.major_faults = vm.rates[VmRate::Pgmajfault as usize];
        self.oom_kill = vm.rates[VmRate::OomKill as usize];
        if self.oom_kill.is_some_and(|rate| rate > 0.0) {
            self.last_oom_at_ms = Some(frame.at_ms);
        }

        let psi = &frame.parts.psi.memory;
        self.psi_some_avg10 = psi.file.some.map(|line| f64::from(line.avg10) / 100.0);
        self.psi_some_avg60 = psi.file.some.map(|line| f64::from(line.avg60) / 100.0);
        self.psi_full_avg10 = psi.file.full.map(|line| f64::from(line.avg10) / 100.0);
        self.psi_full_avg60 = psi.file.full.map(|line| f64::from(line.avg60) / 100.0);

        let input = MemHealthInput {
            mem_total_bytes: self.total,
            mem_available_bytes: self.available,
            swap_total_bytes: self.swap_total,
            swap_free_bytes: self.swap_free,
            swap_in_per_sec: frame.parts.vmstat.rates[VmRate::Pswpin as usize],
            psi_some_avg10: self.psi_some_avg10,
            psi_full_avg10: self.psi_full_avg10,
            refault_pages_per_sec: sum_rates(
                vm.rates[VmRate::RefaultAnon as usize],
                vm.rates[VmRate::RefaultFile as usize],
            ),
            majflt_per_sec: self.major_faults,
            direct_reclaim_pages_per_sec: self.reclaim,
            allocstall_per_sec: vm.rates[VmRate::Allocstall as usize],
            secs_since_oom_kill: self
                .last_oom_at_ms
                .map(|last| frame.at_ms.saturating_sub(last) as f64 / 1000.0),
            slice_used_frac_of_high: None,
            os_level: None,
        };
        self.health = mem_health::evaluate(&input);
    }
}

fn sum_rates(left: Option<f64>, right: Option<f64>) -> Option<f64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left + right),
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    }
}

pub(in crate::monitor) struct MemoryView<'a>(pub &'a MemoryModel);

impl TabView for MemoryView<'_> {
    fn specs(&self) -> &'static [PanelSpec] {
        &SPECS
    }

    fn template(&self) -> Node {
        let mut panels = vec![COMPOSITION, VERDICT, PRESSURE, PAGING];
        if self.0.swap_total.is_some_and(|total| total > 0) {
            panels.push(SWAP);
        }
        Node::Col(vec![
            (
                Size::Weight(1),
                Node::Row(
                    panels[..panels.len().min(3)]
                        .iter()
                        .map(|id| (Size::Weight(1), Node::Panel(*id)))
                        .collect(),
                ),
            ),
            (
                Size::Weight(1),
                Node::Row(
                    panels[panels.len().min(3)..]
                        .iter()
                        .map(|id| (Size::Weight(1), Node::Panel(*id)))
                        .collect(),
                ),
            ),
        ])
    }

    fn demand(&self, panel: PanelId) -> CollectorSet {
        let mut demand = CollectorSet::empty();
        match panel {
            COMPOSITION => {
                demand.insert(CollectorId::Meminfo);
            }
            VERDICT => {
                demand.insert(CollectorId::Meminfo);
                demand.insert(CollectorId::Vmstat);
                demand.insert(CollectorId::Psi);
            }
            PRESSURE => {
                demand.insert(CollectorId::Psi);
            }
            SWAP => {
                demand.insert(CollectorId::Meminfo);
                demand.insert(CollectorId::Vmstat);
            }
            PAGING => {
                demand.insert(CollectorId::Vmstat);
            }
            _ => {}
        }
        demand
    }

    fn paint(
        &self,
        surface: &mut Surface,
        placed: &Placed,
        rect: Rect,
        _scroll: usize,
        _sections: &[crate::sections::Section],
    ) {
        let mut grid = CellGrid::new(rect.cols, rect.rows);
        let title = match placed.id {
            COMPOSITION => "Memory composition",
            VERDICT => "Memory health",
            PRESSURE => "Memory pressure",
            SWAP => "Swap and compression",
            PAGING => "Paging activity",
            _ => return,
        };
        let area = Rect::full(rect.cols, rect.rows);
        let body = panel(&mut grid, area, title, None).rect();
        match placed.id {
            COMPOSITION => self.paint_composition(&mut grid, body),
            VERDICT => self.paint_verdict(&mut grid, body),
            PRESSURE => self.paint_pressure(&mut grid, body),
            SWAP => self.paint_swap(&mut grid, body),
            PAGING => self.paint_paging(&mut grid, body),
            _ => {}
        }
        Emitter::default().emit(&grid, area, surface, rect.x, rect.y);
    }
}

impl MemoryView<'_> {
    fn paint_composition(&self, grid: &mut CellGrid, body: Rect) {
        let m = self.0;
        let total = m.total;
        let values = [
            (
                "used",
                m.total
                    .zip(m.free)
                    .zip(m.buffers)
                    .zip(m.cache)
                    .map(|(((total, free), buffers), cache)| {
                        total.saturating_sub(free.saturating_add(buffers).saturating_add(cache))
                    })
                    .or(m.used),
                SeriesRole::MemAnon,
            ),
            ("cache", m.cache, SeriesRole::MemCache),
            ("buffers", m.buffers, SeriesRole::MemBuffers),
            ("free", m.free, SeriesRole::MemFree),
        ];
        let whole = total.filter(|total| *total > 0);
        if let Some(total) = whole
            && let [Some(used), Some(cache), Some(buffers), Some(free)] =
                values.map(|(_, value, _)| value)
        {
            let proportions =
                [used, cache, buffers, free].map(|value| value.min(total) as f32 / total as f32);
            // The graph stacked painter still emits GraphOut rows; adapting it
            // to CellGrid is deferred until the kit exposes that adapter.
            let mut x: usize = 0;
            for (proportion, (_, _, role)) in proportions.into_iter().zip(values) {
                let width = (proportion * body.cols as f32).round() as usize;
                let end = x.saturating_add(width).min(body.cols);
                for col in x..end {
                    let style = crate::monitor::kit::grid::CellStyle {
                        ch: crate::caps::active_glyphs()
                            .bar_fill
                            .chars()
                            .next()
                            .unwrap_or_default(),
                        fg: StyleTok::from_tok(Tok::Series(role)).unwrap_or(StyleTok::INHERIT),
                        ..Default::default()
                    };
                    if body.rows > 0 {
                        grid.view(Rect {
                            x: body.x + col,
                            y: body.y,
                            cols: 1,
                            rows: 1,
                        })
                        .put(0, 0, style);
                    }
                }
                x = end;
            }
        } else if body.cols > 0 && body.rows > 0 {
            grid.view(Rect {
                x: body.x,
                y: body.y,
                cols: body.cols,
                rows: 1,
            })
            .put_str(
                0,
                0,
                crate::caps::active_glyphs().emdash,
                Default::default(),
            );
        }
        let entries = [
            ("total", fmt_bytes(m.total)),
            ("available", fmt_bytes(m.available)),
            ("used", fmt_bytes(m.used)),
            ("free", fmt_bytes(m.free)),
            ("cache", fmt_bytes(m.cache)),
            ("buffers", fmt_bytes(m.buffers)),
            ("anon", fmt_bytes(m.anon)),
            ("file", fmt_bytes(m.file)),
            ("shmem", fmt_bytes(m.shmem)),
            ("slab", fmt_bytes(m.slab)),
            ("dirty", fmt_bytes(m.dirty)),
            ("writeback", fmt_bytes(m.writeback)),
            (
                "hugepages total",
                reading_text(m.hugepages_total, Unit::Count),
            ),
            (
                "hugepages free",
                reading_text(m.hugepages_free, Unit::Count),
            ),
        ];
        let rows_data: Vec<[kit::table::TableCell<'_>; 2]> = entries
            .iter()
            .map(|(name, size)| {
                [
                    kit::table::TableCell::Text(name),
                    kit::table::TableCell::Text(size),
                ]
            })
            .collect();
        let rows: Vec<&[kit::table::TableCell<'_>]> =
            rows_data.iter().map(|row| row.as_slice()).collect();
        kit::table::draw(
            grid,
            Rect {
                x: body.x,
                y: body.y.saturating_add(1),
                cols: body.cols,
                rows: body.rows.saturating_sub(1),
            },
            &kit::table::TableSpec {
                cols: &[
                    kit::table::Col {
                        title: "component",
                        width: body.cols / 2,
                        align: kit::table::Align::Left,
                        unit: None,
                    },
                    kit::table::Col {
                        title: "size",
                        width: body.cols.saturating_sub(body.cols / 2 + 1),
                        align: kit::table::Align::Right,
                        unit: None,
                    },
                ],
                rows: &rows,
                offset: 0,
                visible: body.rows.saturating_sub(1),
                sort: None,
                selected: None,
                fmt: NumFmt::default(),
            },
        );
    }

    fn paint_verdict(&self, grid: &mut CellGrid, body: Rect) {
        let state = match self.0.health.verdict {
            Verdict::Unknown => StateRole::Unknown,
            Verdict::Ok => StateRole::Ok,
            Verdict::Tight | Verdict::Pressure => StateRole::Warn,
            Verdict::Thrashing | Verdict::Oom => StateRole::Crit,
        };
        kit::chip::draw(
            grid,
            Rect {
                x: body.x,
                y: body.y,
                cols: body.cols,
                rows: 1,
            },
            self.0.health.verdict.label(),
            Reading::NotApplicable,
            Unit::Count,
            NumFmt::default(),
            state,
        );
        let mut line = String::new();
        for reason in self.0.health.reasons.iter() {
            line.push_str("  ");
            line.push_str(reason_label(reason));
        }
        if body.rows > 1 {
            grid.view(Rect {
                x: body.x,
                y: body.y + 1,
                cols: body.cols,
                rows: body.rows - 1,
            })
            .put_str(0, 0, &line, Default::default());
        }
    }

    fn paint_pressure(&self, grid: &mut CellGrid, body: Rect) {
        for (row, title, value, ramp) in [
            (0, "some", self.0.psi_some_avg10, RampId::Mem),
            (2, "full", self.0.psi_full_avg10, RampId::Util),
        ] {
            if body.rows <= row {
                continue;
            }
            let value_text = percent_text(value);
            grid.view(Rect {
                x: body.x,
                y: body.y + row,
                cols: body.cols,
                rows: 1,
            })
            .put_str(
                0,
                0,
                &format!("{title} avg10 {value_text}"),
                Default::default(),
            );
            if let Some(value) = value
                && body.rows > row + 1
            {
                let mut meter = Meter::default();
                meter.paint(
                    grid,
                    Rect {
                        x: body.x,
                        y: body.y + row + 1,
                        cols: body.cols,
                        rows: 1,
                    },
                    kit::meter::MeterSpec {
                        value: (value as f32 / 100.0).clamp(0.0, 1.0),
                        warn: 0.05,
                        crit: 0.10,
                        ramp,
                        label: true,
                        fmt: NumFmt::default(),
                    },
                );
            }
        }
        let avg60 = format!(
            "avg60 some {}  full {}",
            percent_text(self.0.psi_some_avg60),
            percent_text(self.0.psi_full_avg60)
        );
        if body.rows > 4 {
            grid.view(Rect {
                x: body.x,
                y: body.y + 4,
                cols: body.cols,
                rows: 1,
            })
            .put_str(0, 0, &avg60, Default::default());
        }
    }

    fn paint_swap(&self, grid: &mut CellGrid, body: Rect) {
        let (Some(total), Some(free)) = (self.0.swap_total, self.0.swap_free) else {
            draw_absent(grid, body);
            return;
        };
        let used = total.saturating_sub(free);
        let label = format!(
            "used {} / {}  zram {}  zswap {}",
            fmt_bytes(Some(used)),
            fmt_bytes(Some(total)),
            fmt_bytes(self.0.zram_used),
            fmt_bytes(self.0.zswap)
        );
        grid.view(Rect {
            x: body.x,
            y: body.y,
            cols: body.cols,
            rows: 1,
        })
        .put_str(0, 0, &label, Default::default());
        let ratio = if total > 0 {
            used as f32 / total as f32
        } else {
            0.0
        };
        let mut meter = Meter::default();
        if body.rows > 1 {
            meter.paint(
                grid,
                Rect {
                    x: body.x,
                    y: body.y + 1,
                    cols: body.cols,
                    rows: 1,
                },
                kit::meter::MeterSpec {
                    value: ratio,
                    warn: 0.5,
                    crit: 0.8,
                    ramp: RampId::Mem,
                    label: true,
                    fmt: NumFmt::default(),
                },
            );
        }
        let zram_devs = format!(
            "zram {}  zswap {}",
            fmt_bytes(self.0.zram_used),
            fmt_bytes(self.0.zswap)
        );
        if body.rows > 2 {
            grid.view(Rect {
                x: body.x,
                y: body.y + 2,
                cols: body.cols,
                rows: body.rows - 2,
            })
            .put_str(0, 0, &zram_devs, Default::default());
        }
    }

    fn paint_paging(&self, grid: &mut CellGrid, body: Rect) {
        let cols = [
            kit::table::Col {
                title: "rate",
                width: body.cols.saturating_sub(14),
                align: kit::table::Align::Left,
                unit: None,
            },
            kit::table::Col {
                title: "per sec",
                width: 12,
                align: kit::table::Align::Right,
                unit: Some(Unit::Count),
            },
        ];
        let items = [
            ("direct reclaim", self.0.reclaim),
            ("kswapd steal", self.0.steal),
            ("scan", self.0.scan),
            ("refaults", self.0.refaults),
            ("allocstall", self.0.allocstall),
            ("major faults", self.0.major_faults),
            ("OOM kills", self.0.oom_kill),
        ];
        let rows_data: Vec<[kit::table::TableCell<'_>; 2]> = items
            .iter()
            .map(|(name, value)| {
                [
                    kit::table::TableCell::Text(name),
                    kit::table::TableCell::Number(value.map_or(Reading::Unknown, Reading::Value)),
                ]
            })
            .collect();
        let rows: Vec<&[kit::table::TableCell<'_>]> =
            rows_data.iter().map(|row| row.as_slice()).collect();
        kit::table::draw(
            grid,
            body,
            &kit::table::TableSpec {
                cols: &cols,
                rows: &rows,
                offset: 0,
                visible: body.rows,
                sort: None,
                selected: None,
                fmt: NumFmt::default(),
            },
        );
    }
}

fn absent_mark() -> &'static str {
    crate::caps::active_glyphs().emdash
}
fn draw_absent(grid: &mut CellGrid, body: Rect) {
    if body.rows > 0 && body.cols > 0 {
        grid.view(body)
            .put_str(0, 0, absent_mark(), Default::default());
    }
}
fn fmt_bytes(value: Option<u64>) -> String {
    value.map_or_else(
        || absent_mark().to_owned(),
        |value| kit::edge_fmt(NumFmt::default()).fmt(value as f64, Unit::Bytes, 12),
    )
}
fn reading_text(value: Option<u64>, unit: Unit) -> String {
    value.map_or_else(
        || absent_mark().to_owned(),
        |value| kit::edge_fmt(NumFmt::default()).fmt(value as f64, unit, 12),
    )
}
fn percent_text(value: Option<f64>) -> String {
    value.map_or_else(|| absent_mark().to_owned(), |value| format!("{value:.2}%"))
}
fn reason_label(reason: Reason) -> &'static str {
    match reason {
        Reason::OomRecent => "recent OOM",
        Reason::PsiFull => "full PSI",
        Reason::RefaultMajflt => "refault + major faults",
        Reason::PsiSome => "some PSI",
        Reason::DirectReclaim => "direct reclaim",
        Reason::AllocStall => "allocation stalls",
        Reason::AvailableLow => "low available",
        Reason::AvailableTight => "tight available",
        Reason::SwapActive => "active swap",
        Reason::SliceHigh => "slice high",
        Reason::OsLevel => "OS pressure",
    }
}

#[cfg(test)]
#[path = "memory_view_tests.rs"]
mod tests;
