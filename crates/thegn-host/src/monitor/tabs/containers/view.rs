#![allow(dead_code)] // wiring waits for V-CTR-1b

use crate::{
    compositor::Rect,
    monitor::{
        board::TabView,
        graphs,
        kit::{emit::Emitter, grid::CellGrid, panel},
        layout::{Node, PanelId, PanelSpec, Placed, Size, Variant, VariantKind},
    },
    sections::Section,
};
use std::sync::Arc;
use termwiz::surface::Surface;
use thegn_core::{
    numfmt::{Marks, NumFmt, Reading, Unit, ZeroStyle},
    sandbox::ContainerInfo,
    theme::RampId,
};
use thegn_metrics::collect::{
    CollectorId, CollectorSet,
    frame::MonitorFrame,
    health::{Absent, Health},
    model::containers_stats::ContainerStatsRow,
};

const ESTATE: PanelId = PanelId("containers.estate");
const CPU: PanelId = PanelId("containers.cpu");
const MEMORY: PanelId = PanelId("containers.memory");
const IO: PanelId = PanelId("containers.io");
const TABLE: PanelId = PanelId("containers.table");
const HISTORY_CAP: usize = 120;
const SLOT_CAP: usize = 32;
const VARIANT: [Variant; 1] = [Variant {
    kind: VariantKind::Full,
    min: (24, 4),
    pref: (64, 10),
}];
const SPECS: [PanelSpec; 5] = [
    PanelSpec {
        id: ESTATE,
        priority: 0,
        variants: &VARIANT,
    },
    PanelSpec {
        id: CPU,
        priority: 1,
        variants: &VARIANT,
    },
    PanelSpec {
        id: MEMORY,
        priority: 2,
        variants: &VARIANT,
    },
    PanelSpec {
        id: IO,
        priority: 3,
        variants: &VARIANT,
    },
    PanelSpec {
        id: TABLE,
        priority: 4,
        variants: &VARIANT,
    },
];

#[derive(Debug, Clone)]
struct Ring {
    values: [f32; HISTORY_CAP],
    next: usize,
    len: usize,
}

impl Default for Ring {
    fn default() -> Self {
        Self {
            values: [f32::NAN; HISTORY_CAP],
            next: 0,
            len: 0,
        }
    }
}

impl Ring {
    fn clear(&mut self) {
        self.values.fill(f32::NAN);
        self.next = 0;
        self.len = 0;
    }

    fn push(&mut self, value: f32) {
        self.values[self.next] = value;
        self.next = (self.next + 1) % HISTORY_CAP;
        self.len = self.len.saturating_add(1).min(HISTORY_CAP);
    }

    fn ordered(&self, out: &mut [f32; HISTORY_CAP]) -> usize {
        let start = (self.next + HISTORY_CAP - self.len) % HISTORY_CAP;
        for (offset, slot) in out.iter_mut().take(self.len).enumerate() {
            *slot = self.values[(start + offset) % HISTORY_CAP];
        }
        self.len
    }
}

#[derive(Debug, Clone)]
struct Slot {
    id: Option<Arc<str>>,
    matched: bool,
    cpu: Option<f64>,
    memory: Option<u64>,
    peak: Option<u64>,
    pids: Option<u64>,
    read: Option<f64>,
    write: Option<f64>,
    cpu_history: Ring,
    memory_history: Ring,
    read_history: Ring,
    write_history: Ring,
}

impl Default for Slot {
    fn default() -> Self {
        Self {
            id: None,
            matched: false,
            cpu: None,
            memory: None,
            peak: None,
            pids: None,
            read: None,
            write: None,
            cpu_history: Ring::default(),
            memory_history: Ring::default(),
            read_history: Ring::default(),
            write_history: Ring::default(),
        }
    }
}

#[derive(Debug, Clone)]
pub(in crate::monitor) struct ContainersModel {
    slots: Vec<Slot>,
    order: Vec<usize>,
    cpu_total: Ring,
    memory_total: Ring,
    read_total: Ring,
    write_total: Ring,
    pub(super) health: Health,
    pub(super) scopes: u32,
    pub(super) truncated: bool,
    pub(super) rescans: u32,
    pub(super) list_errors: u32,
}

impl Default for ContainersModel {
    fn default() -> Self {
        Self {
            slots: Vec::new(),
            order: Vec::new(),
            cpu_total: Ring::default(),
            memory_total: Ring::default(),
            read_total: Ring::default(),
            write_total: Ring::default(),
            health: Health::Warming,
            scopes: 0,
            truncated: false,
            rescans: 0,
            list_errors: 0,
        }
    }
}

impl ContainersModel {
    pub(in crate::monitor) fn absorb_frame(&mut self, frame: &MonitorFrame) {
        let part = &frame.parts.containers_stats;
        self.health = frame.health[CollectorId::ContainersStats.index()];
        self.scopes = part.scopes;
        self.truncated = part.truncated;
        self.rescans = part.rescans;
        self.list_errors = part.list_errors;
        if !part.rows.is_empty() && self.slots.capacity() == 0 {
            self.slots.reserve_exact(SLOT_CAP);
            self.slots.resize_with(SLOT_CAP, Slot::default);
            self.order.reserve_exact(SLOT_CAP);
        }
        for slot in &mut self.slots {
            slot.matched = false;
        }
        // First preserve every id already resident, independent of row order.
        for row in part.rows.iter().take(SLOT_CAP) {
            if let Some(slot) = self
                .slots
                .iter_mut()
                .find(|slot| slot.id.as_deref() == Some(&*row.id))
            {
                fill_slot(slot, row);
                slot.matched = true;
            }
        }
        // New ids may use only slots left vacant by the matching pass.
        for row in part.rows.iter().take(SLOT_CAP) {
            if self
                .slots
                .iter()
                .any(|slot| slot.id.as_deref() == Some(&*row.id))
            {
                continue;
            }
            if let Some(slot) = self.slots.iter_mut().find(|slot| !slot.matched) {
                slot.cpu_history.clear();
                slot.memory_history.clear();
                slot.read_history.clear();
                slot.write_history.clear();
                slot.id = Some(Arc::clone(&row.id));
                fill_slot(slot, row);
                slot.matched = true;
            }
        }
        for slot in &mut self.slots {
            if !slot.matched {
                slot.id = None;
                slot.cpu = None;
                slot.memory = None;
                slot.peak = None;
                slot.pids = None;
                slot.read = None;
                slot.write = None;
                slot.cpu_history.clear();
                slot.memory_history.clear();
                slot.read_history.clear();
                slot.write_history.clear();
            }
        }
        self.order.clear();
        for (index, slot) in self.slots.iter().enumerate() {
            if slot.id.is_some() {
                self.order.push(index);
            }
        }
        self.order.sort_by(|left, right| {
            self.slots[*right]
                .cpu
                .partial_cmp(&self.slots[*left].cpu)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| {
                    self.slots[*left]
                        .id
                        .as_deref()
                        .cmp(&self.slots[*right].id.as_deref())
                })
        });
        let has_gap = part.rows.iter().any(|row| row.gap);
        let cpu = if has_gap {
            None
        } else {
            sum_values(part.rows.iter().map(|row| row.cpu_pct))
        };
        let memory = if has_gap {
            None
        } else {
            sum_values(
                part.rows
                    .iter()
                    .map(|row| row.mem_current.map(|v| v as f64)),
            )
        };
        let read = if has_gap {
            None
        } else {
            sum_values(part.rows.iter().map(|row| row.io_read_bps))
        };
        let write = if has_gap {
            None
        } else {
            sum_values(part.rows.iter().map(|row| row.io_write_bps))
        };
        self.cpu_total.push(cpu.map_or(f32::NAN, |v| v as f32));
        self.memory_total
            .push(memory.map_or(f32::NAN, |v| v as f32));
        self.read_total.push(read.map_or(f32::NAN, |v| v as f32));
        self.write_total.push(write.map_or(f32::NAN, |v| v as f32));
    }
}

fn sum_values(values: impl Iterator<Item = Option<f64>>) -> Option<f64> {
    let mut sum = 0.0;
    let mut found = false;
    for value in values {
        let value = value.filter(|value| value.is_finite() && *value >= 0.0)?;
        sum += value;
        found = true;
    }
    found.then_some(sum)
}

fn finite<T: Copy>(value: Option<T>, gap: bool, convert: impl FnOnce(T) -> f32) -> f32 {
    if gap {
        f32::NAN
    } else {
        value.map_or(f32::NAN, convert)
    }
}

fn fill_slot(slot: &mut Slot, row: &ContainerStatsRow) {
    slot.id = Some(Arc::clone(&row.id));
    slot.cpu = if row.gap {
        None
    } else {
        row.cpu_pct.filter(|v| v.is_finite() && *v >= 0.0)
    };
    slot.memory = if row.gap { None } else { row.mem_current };
    slot.peak = if row.gap { None } else { row.mem_peak };
    slot.pids = row.pids;
    slot.read = if row.gap {
        None
    } else {
        row.io_read_bps.filter(|v| v.is_finite() && *v >= 0.0)
    };
    slot.write = if row.gap {
        None
    } else {
        row.io_write_bps.filter(|v| v.is_finite() && *v >= 0.0)
    };
    slot.cpu_history.push(finite(row.cpu_pct, row.gap, |v| {
        if v.is_finite() && v >= 0.0 {
            v as f32
        } else {
            f32::NAN
        }
    }));
    slot.memory_history
        .push(finite(row.mem_current, row.gap, |v| v as f32));
    slot.read_history
        .push(finite(row.io_read_bps, row.gap, |v| {
            if v.is_finite() && v >= 0.0 {
                v as f32
            } else {
                f32::NAN
            }
        }));
    slot.write_history
        .push(finite(row.io_write_bps, row.gap, |v| {
            if v.is_finite() && v >= 0.0 {
                v as f32
            } else {
                f32::NAN
            }
        }));
}

pub(in crate::monitor) struct ContainersView<'a> {
    model: &'a ContainersModel,
    inventory: &'a [ContainerInfo],
}

impl<'a> ContainersView<'a> {
    pub(in crate::monitor) fn new(
        model: &'a ContainersModel,
        inventory: &'a [ContainerInfo],
    ) -> Self {
        Self { model, inventory }
    }
}

impl TabView for ContainersView<'_> {
    fn specs(&self) -> &'static [PanelSpec] {
        &SPECS
    }
    fn template(&self) -> Node {
        Node::Col(vec![
            (Size::Weight(1), Node::Panel(ESTATE)),
            (
                Size::Weight(2),
                Node::Row(vec![
                    (Size::Weight(1), Node::Panel(CPU)),
                    (Size::Weight(1), Node::Panel(MEMORY)),
                ]),
            ),
            (
                Size::Weight(2),
                Node::Row(vec![
                    (Size::Weight(1), Node::Panel(IO)),
                    (Size::Weight(2), Node::Panel(TABLE)),
                ]),
            ),
        ])
    }
    fn demand(&self, _panel: PanelId) -> CollectorSet {
        let mut set = CollectorSet::empty();
        set.insert(CollectorId::ContainersStats);
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
        let mut grid = CellGrid::new(rect.cols, rect.rows);
        let title = match placed.id {
            ESTATE => "Estate",
            CPU => "CPU | % of one core",
            MEMORY => "Memory | peak",
            IO => "I/O | window peak",
            TABLE => "Containers",
            _ => return,
        };
        let body = panel::panel(&mut grid, Rect::full(rect.cols, rect.rows), title, None).rect();
        if matches!(placed.id, CPU | MEMORY | IO) {
            let label = match placed.id {
                CPU => format!(
                    "all-container CPU | window peak {:.1}% core",
                    max_ring(&self.model.cpu_total)
                ),
                MEMORY => format!(
                    "window peak {}",
                    fmt_option(ring_peak_reading(&self.model.memory_total), Unit::Bytes)
                ),
                _ => format!(
                    "read / write | window peak {:.0} B/s",
                    max_pair(&self.model.read_total, &self.model.write_total)
                ),
            };
            grid.view(body).put_str(0, 0, &label, Default::default());
        }
        if matches!(self.model.health, Health::Absent(_)) {
            grid.view(body)
                .put_str(0, 0, absent_mark(self.model.health), Default::default());
        } else if matches!(self.model.health, Health::Warming) && self.model.order.is_empty() {
            grid.view(body).put_str(
                0,
                0,
                crate::caps::active_glyphs().ellipsis,
                Default::default(),
            );
        } else {
            match placed.id {
                ESTATE => self.paint_estate(&mut grid, body),
                CPU | MEMORY | IO => {}
                TABLE => self.paint_table(&mut grid, body),
                _ => {}
            }
        }
        Emitter::default().emit(
            &grid,
            Rect::full(rect.cols, rect.rows),
            surface,
            rect.x,
            rect.y,
        );
        if matches!(placed.id, CPU | MEMORY | IO) {
            self.paint_history(
                surface,
                placed.id,
                Rect {
                    x: rect.x + body.x,
                    y: rect.y + body.y + 1,
                    rows: body.rows.saturating_sub(1),
                    ..body
                },
            );
        }
        if placed.id == TABLE {
            self.paint_table_sparks(surface, body, rect);
        }
    }
}

impl ContainersView<'_> {
    fn paint_estate(&self, grid: &mut CellGrid, body: Rect) {
        let running = self
            .inventory
            .iter()
            .filter(|row| thegn_core::sandbox_manage::container_running(&row.status))
            .count();
        let owned = self.inventory.iter().filter(|row| row.ours).count();
        let foreign = self.inventory.len().saturating_sub(owned);
        let text = format!(
            "{} total | {running} running | {owned} owned | {foreign} foreign",
            self.inventory.len()
        );
        grid.view(body).put_str(0, 0, &text, Default::default());
        if body.rows > 1 {
            let totals = format!(
                "CPU {} | memory {}",
                fmt_option(
                    sum_slots(self.model, |slot| slot.cpu).map(Reading::Value),
                    Unit::Percent
                ),
                fmt_option(
                    sum_slots(self.model, |slot| slot.memory.map(|v| v as f64)).map(Reading::Value),
                    Unit::Bytes
                )
            );
            grid.view(Rect {
                y: body.y + 1,
                rows: body.rows - 1,
                ..body
            })
            .put_str(0, 0, &totals, Default::default());
        }
        let flags = format!(
            "scopes {}{}{}",
            self.model.scopes,
            if self.model.truncated {
                " | truncated"
            } else {
                ""
            },
            if self.model.list_errors > 0 {
                " | list errors"
            } else {
                ""
            }
        );
        if body.rows > 2 {
            grid.view(Rect {
                y: body.y + 2,
                rows: body.rows - 2,
                ..body
            })
            .put_str(0, 0, &flags, Default::default());
        }
    }

    fn paint_history(&self, surface: &mut Surface, panel_id: PanelId, graph_area: Rect) {
        let (ring, peak, ramp) = match panel_id {
            CPU => (
                &self.model.cpu_total,
                max_ring(&self.model.cpu_total).max(1.0),
                RampId::Util,
            ),
            MEMORY => (
                &self.model.memory_total,
                max_ring(&self.model.memory_total),
                RampId::Mem,
            ),
            _ => (
                &self.model.read_total,
                max_pair(&self.model.read_total, &self.model.write_total),
                RampId::Read,
            ),
        };
        let mut values = [f32::NAN; HISTORY_CAP];
        let len = ring.ordered(&mut values);
        if len > 0 && peak > 0.0 {
            for value in &mut values[..len] {
                if value.is_finite() {
                    *value = (*value / peak).clamp(0.0, 1.0);
                }
            }
        }
        if panel_id == IO {
            let mut down = [f32::NAN; HISTORY_CAP];
            let write_peak = max_pair(&self.model.read_total, &self.model.write_total);
            let down_len = self.model.write_total.ordered(&mut down);
            if write_peak > 0.0 {
                for val in &mut down[..down_len] {
                    if val.is_finite() {
                        *val = (*val / write_peak).clamp(0.0, 1.0);
                    }
                }
            }
            if len > 0 && down_len > 0 {
                let half = graph_area.rows / 2;
                let upper = Rect {
                    rows: half,
                    ..graph_area
                };
                let lower = Rect {
                    y: graph_area.y + half,
                    rows: graph_area.rows - half,
                    ..graph_area
                };
                graphs::paint_history_graph(
                    &values[..len],
                    ramp,
                    crate::monitor::kit::history_rect::graph_rect(
                        upper,
                        len,
                        graphs::table().braille,
                    ),
                    surface,
                );
                graphs::paint_history_graph(
                    &down[..down_len],
                    RampId::Write,
                    crate::monitor::kit::history_rect::graph_rect(
                        lower,
                        down_len,
                        graphs::table().braille,
                    ),
                    surface,
                );
            }
        } else if len > 0 {
            graphs::paint_history_graph(
                &values[..len],
                ramp,
                crate::monitor::kit::history_rect::graph_rect(
                    graph_area,
                    len,
                    graphs::table().braille,
                ),
                surface,
            );
        }
    }

    fn paint_table(&self, grid: &mut CellGrid, body: Rect) {
        let headers = "id           cpu% cpu history       memory mem/peak spark pids io B/s";
        grid.view(body).put_str(0, 0, headers, Default::default());
        for (visible, index) in self
            .model
            .order
            .iter()
            .take(body.rows.saturating_sub(1))
            .enumerate()
        {
            let slot = &self.model.slots[*index];
            let id = slot.id.as_deref().unwrap_or("");
            let short = &id[..id.char_indices().nth(12).map_or(id.len(), |(pos, _)| pos)];
            let cpu = fmt_option(slot.cpu.map(Reading::Value), Unit::Percent);
            let memory = fmt_option(
                slot.memory.map(|value| Reading::Value(value as f64)),
                Unit::Bytes,
            );
            let pids = fmt_option(
                slot.pids.map(|value| Reading::Value(value as f64)),
                Unit::Count,
            );
            let io = match (slot.read, slot.write) {
                (Some(r), Some(w)) => format!("{r:.0}/{w:.0}"),
                _ => fmt_option(None, Unit::BitsPerSec),
            };
            let row =
                format!("{short:<12} {cpu:>5}             {memory:>10}             {pids:>4} {io}");
            grid.view(Rect {
                y: body.y + visible + 1,
                rows: body.rows.saturating_sub(visible + 1),
                ..body
            })
            .put_str(0, 0, &row, Default::default());
        }
    }

    fn paint_table_sparks(&self, surface: &mut Surface, body: Rect, rect: Rect) {
        for (visible, index) in self
            .model
            .order
            .iter()
            .take(body.rows.saturating_sub(1))
            .enumerate()
        {
            let mut history = [f32::NAN; HISTORY_CAP];
            let len = self.model.slots[*index].cpu_history.ordered(&mut history);
            for value in &mut history[..len] {
                if value.is_finite() {
                    *value = (*value / 100.0).clamp(0.0, 1.0);
                }
            }
            let area = Rect {
                x: rect.x + body.x + body.cols.min(20),
                y: rect.y + body.y + visible + 1,
                cols: body.cols.saturating_sub(20).min(14),
                rows: 1,
            };
            paint_finite_spark_runs(&history[..len], RampId::Util, area, surface);
            let mut memory = [f32::NAN; HISTORY_CAP];
            let memory_len = self.model.slots[*index].memory_history.ordered(&mut memory);
            let memory_area = Rect {
                x: rect.x + body.x + body.cols.min(46),
                y: rect.y + body.y + visible + 1,
                cols: body.cols.saturating_sub(46).min(14),
                rows: 1,
            };
            if let Some(peak) = self.model.slots[*index].peak.filter(|peak| *peak > 0) {
                for value in &mut memory[..memory_len] {
                    if value.is_finite() {
                        *value = (*value / peak as f32).clamp(0.0, 1.0);
                    }
                }
                paint_finite_spark_runs(&memory[..memory_len], RampId::Mem, memory_area, surface);
            }
        }
    }
}

fn sum_slots(model: &ContainersModel, get: impl Fn(&Slot) -> Option<f64>) -> Option<f64> {
    let mut total = 0.0;
    let mut found = false;
    for slot in &model.slots {
        if slot.id.is_some()
            && let Some(value) = get(slot)
        {
            total += value;
            found = true;
        }
    }
    found.then_some(total)
}

fn absent_mark(health: Health) -> &'static str {
    match health {
        Health::Absent(Absent::NoPermission) => crate::caps::glyph(crate::caps::Glyph::Lock),
        _ => crate::caps::active_glyphs().emdash,
    }
}

fn paint_finite_spark_runs(values: &[f32], ramp: RampId, area: Rect, surface: &mut Surface) {
    let braille = graphs::table().braille;
    let aligned = crate::monitor::kit::history_rect::graph_rect(area, values.len(), braille);
    if values.is_empty() || aligned.cols == 0 {
        return;
    }
    let mut start = 0;
    while start < values.len() {
        while start < values.len() && !values[start].is_finite() {
            start += 1;
        }
        let mut end = start;
        while end < values.len() && values[end].is_finite() {
            end += 1;
        }
        if end > start {
            let left = start * aligned.cols / values.len();
            let right = (end * aligned.cols)
                .div_ceil(values.len())
                .min(aligned.cols);
            let cols = right.saturating_sub(left);
            if cols > 0 {
                graphs::paint_sparkline(
                    &values[start..end],
                    cols,
                    ramp,
                    Rect {
                        x: aligned.x + left,
                        cols,
                        ..area
                    },
                    surface,
                );
            }
        }
        start = end.max(start + 1);
    }
}

fn max_ring(ring: &Ring) -> f32 {
    ring.values
        .iter()
        .copied()
        .filter(|v| v.is_finite())
        .fold(0.0, f32::max)
}
fn ring_peak_reading(ring: &Ring) -> Option<Reading> {
    ring.values
        .iter()
        .any(|value| value.is_finite())
        .then(|| Reading::Value(max_ring(ring) as f64))
}
fn max_pair(left: &Ring, right: &Ring) -> f32 {
    max_ring(left).max(max_ring(right))
}
fn fmt_option(value: Option<Reading>, unit: Unit) -> String {
    let glyphs = crate::caps::active_glyphs();
    let fmt = NumFmt {
        zero: ZeroStyle::Dot,
        marks: Marks::from_glyphs(glyphs, glyphs.lock),
        ..NumFmt::default()
    };
    fmt.reading(
        value.unwrap_or(Reading::Unknown),
        unit,
        unit.default_width(),
    )
}

#[cfg(test)]
#[path = "containers_view_tests.rs"]
mod tests;
