use crate::{
    compositor::Rect,
    monitor::{
        board::TabView,
        kit::{self, emit::Emitter, grid::CellGrid, meter::Meter, panel::panel},
        layout::{Node, PanelId, PanelSpec, Placed, Size, Variant, VariantKind},
    },
    seg::{StyleTok, Tok},
    telemetry::{Metric, ScaleMode, SeriesReq, TelemetryHistory, Window},
};
use termwiz::surface::Surface;
use thegn_core::{
    mem_health::{self, MemHealth, MemHealthInput, Reason, Verdict},
    numfmt::{NumFmt, Reading, Unit},
    theme::{RampId, SeriesRole, StateRole},
    viz_raster::{self, CellBuf},
};
use thegn_metrics::collect::{
    CollectorId, CollectorSet, frame::MonitorFrame, parse::meminfo::MemField,
};

const COMPOSITION: PanelId = PanelId("memory.composition");
const VERDICT: PanelId = PanelId("memory.verdict");
const PRESSURE: PanelId = PanelId("memory.pressure");
const SWAP: PanelId = PanelId("memory.swap");
const PAGING: PanelId = PanelId("memory.paging");
const LIMIT: PanelId = PanelId("memory.limit");
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
const LIMIT_VARIANTS: [Variant; 1] = [Variant {
    kind: VariantKind::Full,
    min: (36, 4),
    pref: (72, 6),
}];
const HISTORY_CAPACITY: usize = 120;
const SPECS: [PanelSpec; 6] = [
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
    PanelSpec {
        id: LIMIT,
        priority: 5,
        variants: &LIMIT_VARIANTS,
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
    swap_present: bool,
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
    psi_history: MemoryHistory,
    mem_telemetry: MemoryHistory,
    swap_history: MemoryHistory,
    reclaim_history: MemoryHistory,
    pub health: MemHealth,
    pub slice_current: Option<u64>,
    pub slice_high: Option<thegn_metrics::collect::model::cgroup::Limit>,
    pub slice_max: Option<thegn_metrics::collect::model::cgroup::Limit>,
    pub effective_limit: Option<thegn_metrics::collect::model::cgroup::Limit>,
    pub own_current: Option<u64>,
    limit_history: MemoryHistory,
}

#[derive(Debug, Clone, PartialEq)]
struct MemoryHistory {
    values: [f32; HISTORY_CAPACITY],
    next: usize,
    len: usize,
}

impl Default for MemoryHistory {
    fn default() -> Self {
        Self {
            values: [0.0; HISTORY_CAPACITY],
            next: 0,
            len: 0,
        }
    }
}

impl MemoryHistory {
    fn clear(&mut self) {
        self.next = 0;
        self.len = 0;
    }

    fn push(&mut self, value: f32) {
        self.values[self.next] = value;
        self.next = (self.next + 1) % HISTORY_CAPACITY;
        self.len = self.len.saturating_add(1).min(HISTORY_CAPACITY);
    }

    fn copy_recent(&self, out: &mut [f32; HISTORY_CAPACITY]) -> (usize, f32) {
        let start = (self.next + HISTORY_CAPACITY - self.len) % HISTORY_CAPACITY;
        let mut peak = 0.0_f32;
        for (offset, slot) in out.iter_mut().take(self.len).enumerate() {
            let index = (start + offset) % HISTORY_CAPACITY;
            *slot = self.values[index];
            peak = peak.max(*slot);
        }
        (self.len, peak)
    }
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
            swap_present: false,
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
            psi_history: MemoryHistory::default(),
            mem_telemetry: MemoryHistory::default(),
            swap_history: MemoryHistory::default(),
            reclaim_history: MemoryHistory::default(),
            health: mem_health::evaluate(&MemHealthInput::default()),
            slice_current: None,
            slice_high: None,
            slice_max: None,
            effective_limit: None,
            own_current: None,
            limit_history: MemoryHistory::default(),
        }
    }
}

impl MemoryModel {
    #[cfg(test)]
    pub(in crate::monitor) fn history_state(&self) -> (usize, Option<f32>, usize, Option<f32>) {
        fn latest(history: &MemoryHistory) -> (usize, Option<f32>) {
            let mut recent = [0.0_f32; HISTORY_CAPACITY];
            let (len, _) = history.copy_recent(&mut recent);
            (len, len.checked_sub(1).map(|index| recent[index]))
        }
        let (mem_len, mem_latest) = latest(&self.mem_telemetry);
        let (swap_len, swap_latest) = latest(&self.swap_history);
        (mem_len, mem_latest, swap_len, swap_latest)
    }

    pub(in crate::monitor) fn absorb_history(
        &mut self,
        history: &TelemetryHistory,
        window: Window,
        _scale: ScaleMode,
        now_ms: u64,
    ) {
        for (metric, slot) in [
            (Metric::Mem, &mut self.mem_telemetry),
            (Metric::Swap, &mut self.swap_history),
        ] {
            let series = history.series(&SeriesReq {
                metric,
                window,
                scale: TelemetryHistory::scale_for(metric, ScaleMode::Fixed),
                buckets: HISTORY_CAPACITY,
                agg: thegn_core::series::Agg::Max,
                now_ms: now_ms.saturating_add(1),
            });
            slot.clear();
            if !series.empty {
                for (raw, gap) in series.raw_hi.iter().zip(&series.gap) {
                    if !gap && raw.is_finite() {
                        slot.push((raw / 100.0).clamp(0.0, 1.0));
                    }
                }
            }
        }
    }

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
        if let Some(total) = self.swap_total
            && self.swap_present != (total > 0)
        {
            self.swap_present = total > 0;
        }
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
        if let Some(reclaim) = self.reclaim {
            self.reclaim_history.push(reclaim as f32);
        }
        if self.oom_kill.is_some_and(|rate| rate > 0.0) {
            self.last_oom_at_ms = Some(frame.at_ms);
        }

        let psi = &frame.parts.psi.memory;
        self.psi_some_avg10 = psi.file.some.map(|line| f64::from(line.avg10) / 100.0);
        self.psi_some_avg60 = psi.file.some.map(|line| f64::from(line.avg60) / 100.0);
        self.psi_full_avg10 = psi.file.full.map(|line| f64::from(line.avg10) / 100.0);
        self.psi_full_avg60 = psi.file.full.map(|line| f64::from(line.avg60) / 100.0);
        if let (Some(some), Some(full)) = (self.psi_some_avg10, self.psi_full_avg10) {
            self.psi_history.push((some + full) as f32);
        }
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
        let slice = &frame.parts.cgroup_slice;
        self.slice_current = slice.sample.mem_current;
        self.slice_high = slice.sample.mem_high;
        self.slice_max = slice.sample.mem_max;
        self.effective_limit = slice.effective_limit;
        self.own_current = slice.own_current;
        if let (Some(current), Some(limit)) = (
            slice.sample.mem_current,
            min_numeric_limit(slice.sample.mem_max, slice.sample.mem_high),
        ) && limit > 0
        {
            self.limit_history
                .push((current as f32 / limit as f32).clamp(0.0, 1.0));
        }
    }
}

fn sum_rates(left: Option<f64>, right: Option<f64>) -> Option<f64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left + right),
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    }
}

#[cfg(test)]
fn right_aligned_window(len: usize, width: usize) -> (usize, usize, usize) {
    let visible = len.min(width);
    (len - visible, visible, width - visible)
}

/// Return the sample slice and cell span needed to show the newest history
/// without stretching it. Braille cells carry two samples; ASCII columns carry
/// one.
fn right_aligned_raster_window(len: usize, width: usize) -> (usize, usize, usize, usize) {
    let samples_per_cell =
        if viz_raster::RasterTable::for_level(crate::caps::unicode_level()).braille {
            2
        } else {
            1
        };
    let visible_samples = len.min(width.saturating_mul(samples_per_cell));
    let visible_cells = visible_samples.div_ceil(samples_per_cell);
    (
        len - visible_samples,
        visible_samples,
        width - visible_cells,
        visible_cells,
    )
}

pub(in crate::monitor) struct MemoryView<'a>(pub &'a MemoryModel);

impl TabView for MemoryView<'_> {
    fn specs(&self) -> &'static [PanelSpec] {
        &SPECS
    }

    fn template(&self) -> Node {
        let mut panels = vec![COMPOSITION, VERDICT, PRESSURE, PAGING];
        // SwapTotal changes only on a swap-device layout change; swap usage does
        // not affect panel placement.
        if self.0.swap_present {
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
            (Size::Fixed(9), Node::Panel(LIMIT)),
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
            LIMIT => {
                demand.insert(CollectorId::CgroupSlice);
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
            LIMIT => "Container limit",
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
            LIMIT => self.paint_limit(&mut grid, body),
            _ => {}
        }
        Emitter::default().emit(&grid, area, surface, rect.x, rect.y);
    }
}

impl MemoryView<'_> {
    fn paint_limit(&self, grid: &mut CellGrid, body: Rect) {
        use thegn_metrics::collect::model::cgroup::Limit;

        let used = self.0.own_current;
        let ceiling = match self.0.effective_limit {
            Some(Limit::Value(value)) => Some(value),
            Some(Limit::Max) | None => None,
        };
        let line = match self.0.effective_limit {
            Some(Limit::Max) => "own cgroup  no limit",
            Some(Limit::Value(_)) => "own cgroup",
            None => "own cgroup",
        };
        if body.rows > 0 {
            grid.view(Rect {
                x: body.x,
                y: body.y,
                cols: body.cols,
                rows: 1,
            })
            .put_str(0, 0, line, Default::default());
        }
        if let (Some(used), Some(limit)) = (used, ceiling)
            && limit > 0
            && body.rows > 1
        {
            Meter::default().paint(
                grid,
                Rect {
                    x: body.x,
                    y: body.y + 1,
                    cols: body.cols,
                    rows: 1,
                },
                kit::meter::MeterSpec {
                    value: (used as f32 / limit as f32).clamp(0.0, 1.0),
                    warn: 0.85,
                    crit: 0.95,
                    ramp: RampId::Mem,
                    label: true,
                    fmt: NumFmt::default(),
                },
            );
        } else if (self.0.effective_limit.is_none() || used.is_none()) && body.rows > 1 {
            grid.view(Rect {
                x: body.x,
                y: body.y + 1,
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
        let slice_limit = match (
            numeric_limit(self.0.slice_high),
            numeric_limit(self.0.slice_max),
        ) {
            (Some(high), Some(max)) => Some(high.min(max)),
            (Some(limit), None) | (None, Some(limit)) => Some(limit),
            (None, None) => None,
        };
        let slice_text = match (self.0.slice_current, slice_limit) {
            (Some(current), Some(limit)) => format!(
                "thegn.slice {} / {}",
                fmt_bytes(Some(current)),
                fmt_bytes(Some(limit))
            ),
            (Some(current), None)
                if self.0.slice_high == Some(Limit::Max)
                    && self.0.slice_max == Some(Limit::Max) =>
            {
                format!(
                    "thegn.slice {} {} no limit",
                    fmt_bytes(Some(current)).trim(),
                    crate::caps::active_glyphs().middot
                )
            }
            (Some(current), None) => format!(
                "thegn.slice {} / {}",
                fmt_bytes(Some(current)),
                crate::caps::active_glyphs().emdash
            ),
            (None, _) => format!("thegn.slice {}", crate::caps::active_glyphs().emdash),
        };
        if body.rows > 2 {
            grid.view(Rect {
                x: body.x,
                y: body.y + 2,
                cols: body.cols,
                rows: body.rows - 2,
            })
            .put_str(0, 0, &slice_text, Default::default());
        }
        if let (Some(current), Some(limit)) = (self.0.slice_current, slice_limit)
            && limit > 0
            && body.rows > 3
        {
            Meter::default().paint(
                grid,
                Rect {
                    x: body.x,
                    y: body.y + 3,
                    cols: body.cols,
                    rows: 1,
                },
                kit::meter::MeterSpec {
                    value: (current as f32 / limit as f32).clamp(0.0, 1.0),
                    warn: 0.85,
                    crit: 0.95,
                    ramp: RampId::Mem,
                    label: true,
                    fmt: NumFmt::default(),
                },
            );
        }
        if body.rows > 4 && self.0.limit_history.len > 1 {
            grid.view(Rect {
                x: body.x,
                y: body.y + 4,
                cols: body.cols,
                rows: 1,
            })
            .put_str(0, 0, "thegn.slice usage history", Default::default());
            if body.rows > 5 {
                let mut used = [0.0_f32; HISTORY_CAPACITY];
                let (len, _) = self.0.limit_history.copy_recent(&mut used);
                let available = used.map(|value| 1.0 - value.clamp(0.0, 1.0));
                let chart = Rect {
                    x: body.x,
                    y: body.y + 5,
                    cols: body.cols,
                    rows: body.rows - 5,
                };
                let mut raster = CellBuf::new();
                raster.reset(chart.cols, chart.rows);
                viz_raster::stacked(
                    &[&used[..len], &available[..len]],
                    viz_raster::RasterTable::for_level(crate::caps::unicode_level()),
                    &mut raster,
                );
                draw_raster(
                    grid,
                    chart,
                    &raster,
                    RasterStyle::Series(&[SeriesRole::MemAnon, SeriesRole::MemFree]),
                );
            }
        }
    }

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
        self.paint_used_history(grid, body);
    }

    fn paint_used_history(&self, grid: &mut CellGrid, body: Rect) {
        if self.0.mem_telemetry.len == 0 || body.rows < 4 || body.cols == 0 {
            return;
        }
        let label_y = body.y.saturating_add(16);
        if label_y >= body.y.saturating_add(body.rows) {
            return;
        }
        grid.view(Rect {
            x: body.x,
            y: label_y,
            cols: body.cols,
            rows: 1,
        })
        .put_str(
            0,
            0,
            "used / available history (memory total = 100%)",
            Default::default(),
        );
        let chart = Rect {
            x: body.x,
            y: label_y + 1,
            cols: body.cols,
            rows: body.y.saturating_add(body.rows).saturating_sub(label_y + 1),
        };
        if chart.rows == 0 {
            return;
        }
        let mut used = [0.0_f32; HISTORY_CAPACITY];
        let (len, _) = self.0.mem_telemetry.copy_recent(&mut used);
        if len == 0 {
            return;
        }
        let (start, visible_samples, offset, visible_cells) =
            right_aligned_raster_window(len, chart.cols);
        if visible_samples == 0 {
            return;
        }
        let chart = Rect {
            x: chart.x + offset,
            cols: visible_cells,
            ..chart
        };
        let available = used.map(|value| 1.0 - value.clamp(0.0, 1.0));
        let mut raster = CellBuf::new();
        raster.reset(chart.cols, chart.rows);
        viz_raster::stacked(
            &[
                &used[start..start + visible_samples],
                &available[start..start + visible_samples],
            ],
            viz_raster::RasterTable::for_level(crate::caps::unicode_level()),
            &mut raster,
        );
        draw_raster(
            grid,
            chart,
            &raster,
            RasterStyle::Series(&[SeriesRole::MemAnon, SeriesRole::MemFree]),
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
        self.paint_psi_history(grid, body);
    }

    fn paint_psi_history(&self, grid: &mut CellGrid, body: Rect) {
        if self.0.psi_history.len == 0 || body.rows < 8 || body.cols == 0 {
            return;
        }
        let label_y = body.y.saturating_add(5);
        if label_y >= body.y.saturating_add(body.rows) {
            return;
        }
        let mut values = [0.0_f32; HISTORY_CAPACITY];
        let (len, peak) = self.0.psi_history.copy_recent(&mut values);
        if len == 0 {
            return;
        }
        let (start, visible_samples, _, _) = right_aligned_raster_window(len, body.cols);
        if visible_samples == 0 {
            return;
        }
        let scale = if peak > 0.0 { peak * 1.1 } else { 1.0 };
        for value in values.iter_mut().take(len) {
            *value = (*value / scale).clamp(0.0, 1.0);
        }
        let label = format!("some + full PSI history (window peak {peak:.2}%)");
        grid.view(Rect {
            x: body.x,
            y: label_y,
            cols: body.cols,
            rows: 1,
        })
        .put_str(0, 0, &label, Default::default());
        let chart = Rect {
            x: body.x,
            y: label_y + 1,
            cols: body.cols,
            rows: body.y.saturating_add(body.rows).saturating_sub(label_y + 1),
        };
        if chart.rows == 0 {
            return;
        }
        let (_, _, offset, visible_cells) = right_aligned_raster_window(len, chart.cols);
        let chart = Rect {
            x: chart.x + offset,
            cols: visible_cells,
            ..chart
        };
        let mut raster = CellBuf::new();
        raster.reset(chart.cols, chart.rows);
        viz_raster::area(
            &values[start..start + visible_samples],
            viz_raster::RasterTable::for_level(crate::caps::unicode_level()),
            &mut raster,
        );
        draw_raster(grid, chart, &raster, RasterStyle::Ramp(RampId::Mem));
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
        if self.0.swap_history.len > 0 && body.rows >= 5 {
            let mut values = [0.0_f32; HISTORY_CAPACITY];
            let (len, peak) = self.0.swap_history.copy_recent(&mut values);
            grid.view(Rect {
                x: body.x,
                y: body.y + 3,
                cols: body.cols,
                rows: 1,
            })
            .put_str(
                0,
                0,
                &format!(
                    "swap history (total = 100%, window peak {:.0}%)",
                    peak * 100.0
                ),
                Default::default(),
            );
            let (start, visible_samples, offset, visible_cells) =
                right_aligned_raster_window(len, body.cols);
            if visible_samples > 0 && body.rows > 4 {
                let chart = Rect {
                    x: body.x + offset,
                    y: body.y + 4,
                    cols: visible_cells,
                    rows: body.rows - 4,
                };
                let mut raster = CellBuf::new();
                raster.reset(chart.cols, chart.rows);
                viz_raster::area(
                    &values[start..start + visible_samples],
                    viz_raster::RasterTable::for_level(crate::caps::unicode_level()),
                    &mut raster,
                );
                draw_raster(grid, chart, &raster, RasterStyle::Ramp(RampId::Mem));
            }
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
        self.paint_reclaim_history(grid, body);
    }

    fn paint_reclaim_history(&self, grid: &mut CellGrid, body: Rect) {
        if self.0.reclaim_history.len == 0 || body.rows < 10 || body.cols == 0 {
            return;
        }
        let label_y = body.y.saturating_add(8);
        if label_y >= body.y.saturating_add(body.rows) {
            return;
        }
        let mut values = [0.0_f32; HISTORY_CAPACITY];
        let (len, peak) = self.0.reclaim_history.copy_recent(&mut values);
        if len == 0 || peak <= 0.0 {
            return;
        }
        let (start, visible_samples, _, _) = right_aligned_raster_window(len, body.cols);
        if visible_samples == 0 {
            return;
        }
        for value in values.iter_mut().take(len) {
            *value = (*value / peak).clamp(0.0, 1.0);
        }
        grid.view(Rect {
            x: body.x,
            y: label_y,
            cols: body.cols,
            rows: 1,
        })
        .put_str(
            0,
            0,
            "direct reclaim history (window peak)",
            Default::default(),
        );
        let chart = Rect {
            x: body.x,
            y: label_y + 1,
            cols: body.cols,
            rows: body.y.saturating_add(body.rows).saturating_sub(label_y + 1),
        };
        if chart.rows == 0 {
            return;
        }
        let (_, _, offset, visible_cells) = right_aligned_raster_window(len, chart.cols);
        let chart = Rect {
            x: chart.x + offset,
            cols: visible_cells,
            ..chart
        };
        let mut raster = CellBuf::new();
        raster.reset(chart.cols, chart.rows);
        viz_raster::area(
            &values[start..start + visible_samples],
            viz_raster::RasterTable::for_level(crate::caps::unicode_level()),
            &mut raster,
        );
        draw_raster(grid, chart, &raster, RasterStyle::Ramp(RampId::Mem));
    }
}

fn numeric_limit(limit: Option<thegn_metrics::collect::model::cgroup::Limit>) -> Option<u64> {
    match limit {
        Some(thegn_metrics::collect::model::cgroup::Limit::Value(value)) => Some(value),
        Some(thegn_metrics::collect::model::cgroup::Limit::Max) | None => None,
    }
}

fn min_numeric_limit(
    left: Option<thegn_metrics::collect::model::cgroup::Limit>,
    right: Option<thegn_metrics::collect::model::cgroup::Limit>,
) -> Option<u64> {
    match (numeric_limit(left), numeric_limit(right)) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (Some(limit), None) | (None, Some(limit)) => Some(limit),
        (None, None) => None,
    }
}

#[derive(Clone, Copy)]
enum RasterStyle<'a> {
    Series(&'a [SeriesRole]),
    Ramp(RampId),
}

fn draw_raster(grid: &mut CellGrid, rect: Rect, raster: &CellBuf, style: RasterStyle<'_>) {
    for y in 0..rect.rows.min(raster.height()) {
        for x in 0..rect.cols.min(raster.width()) {
            let cell = raster.row(y)[x];
            let (fg, bg) = match style {
                RasterStyle::Series(roles) => (
                    roles
                        .get(usize::from(cell.fg))
                        .copied()
                        .map(Tok::Series)
                        .and_then(StyleTok::from_tok)
                        .unwrap_or(StyleTok::INHERIT),
                    cell.bg
                        .and_then(|index| roles.get(usize::from(index)).copied())
                        .map(Tok::Series)
                        .and_then(StyleTok::from_tok)
                        .unwrap_or(StyleTok::INHERIT),
                ),
                RasterStyle::Ramp(ramp) => (
                    StyleTok::from_tok(Tok::Ramp(ramp, cell.fg)).unwrap_or(StyleTok::INHERIT),
                    StyleTok::INHERIT,
                ),
            };
            grid.view(Rect {
                x: rect.x + x,
                y: rect.y + y,
                cols: 1,
                rows: 1,
            })
            .put(
                0,
                0,
                crate::monitor::kit::grid::CellStyle {
                    ch: cell.ch,
                    fg,
                    bg,
                    ..Default::default()
                },
            );
        }
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
