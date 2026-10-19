use crate::{
    compositor::Rect,
    monitor::{
        board::TabView,
        graphs,
        kit::{
            emit::Emitter,
            grid::{CellGrid, CellStyle},
            panel,
        },
        layout::{Node, PanelId, PanelSpec, Placed, Size, Variant, VariantKind},
    },
    sections::Section,
    seg::StyleTok,
};
use std::sync::Arc;
use termwiz::surface::Surface;
use thegn_core::{
    numfmt::{Marks, NumFmt, Reading, Unit, ZeroStyle},
    sandbox::ContainerInfo,
    sandbox_manage::{ContainerFootprint, Health as ContainerHealth, container_health},
    theme::{Hue, RampId, SeriesRole},
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
const INVENTORY: PanelId = PanelId("containers.inventory");
const HISTORY_CAP: usize = 120;
const SLOT_CAP: usize = 32;
const TABLE_ID: usize = 0;
const TABLE_CPU: usize = 13;
const TABLE_CPU_SPARK: usize = 20;
const TABLE_MEM: usize = 34;
const TABLE_MEM_SPARK: usize = 46;
const TABLE_PIDS: usize = 60;
const TABLE_IO: usize = 68;
const VARIANT: [Variant; 1] = [Variant {
    kind: VariantKind::Full,
    min: (24, 4),
    pref: (64, 10),
}];
const SPECS: [PanelSpec; 6] = [
    PanelSpec {
        id: ESTATE,
        priority: 0,
        variants: &VARIANT,
    },
    PanelSpec {
        id: INVENTORY,
        priority: 1,
        variants: &VARIANT,
    },
    PanelSpec {
        id: CPU,
        priority: 2,
        variants: &VARIANT,
    },
    PanelSpec {
        id: MEMORY,
        priority: 3,
        variants: &VARIANT,
    },
    PanelSpec {
        id: IO,
        priority: 4,
        variants: &VARIANT,
    },
    PanelSpec {
        id: TABLE,
        priority: 5,
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

    fn push_held(&mut self) {
        let mut ordered = [f32::NAN; HISTORY_CAP];
        let len = self.ordered(&mut ordered);
        if let Some(value) = ordered[..len].iter().rev().find(|value| value.is_finite()) {
            self.push(*value);
        }
    }

    fn ordered(&self, out: &mut [f32; HISTORY_CAP]) -> usize {
        let start = (self.next + HISTORY_CAP - self.len) % HISTORY_CAP;
        for (offset, slot) in out.iter_mut().take(self.len).enumerate() {
            *slot = self.values[(start + offset) % HISTORY_CAP];
        }
        self.len
    }
}

#[derive(Default, Debug, Clone)]
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

#[derive(Debug, Clone)]
pub(in crate::monitor) struct ContainersModel {
    slots: Vec<Slot>,
    order: Vec<usize>,
    cpu_total: Ring,
    memory_total: Ring,
    read_total: Ring,
    write_total: Ring,
    latest_cpu: Option<f64>,
    latest_memory: Option<f64>,
    pub(super) health: Health,
    pub(super) scopes: u32,
    pub(super) truncated: bool,
    pub(super) untracked: usize,
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
            latest_cpu: None,
            latest_memory: None,
            health: Health::Warming,
            scopes: 0,
            truncated: false,
            untracked: 0,
            rescans: 0,
            list_errors: 0,
        }
    }
}

impl ContainersModel {
    pub(in crate::monitor) fn absorb_frame(&mut self, frame: &MonitorFrame) {
        self.health = frame.health[CollectorId::ContainersStats.index()];
        if !frame.sampled.contains(CollectorId::ContainersStats) {
            return;
        }
        let part = &frame.parts.containers_stats;
        self.scopes = part.scopes;
        self.truncated = part.truncated;
        self.rescans = part.rescans;
        self.list_errors = part.list_errors;
        self.untracked = part.rows.len().saturating_sub(SLOT_CAP);
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
        let cpu = sum_values(
            part.rows
                .iter()
                .filter(|row| !row.gap)
                .map(|row| row.cpu_pct),
        );
        let memory = sum_values(
            part.rows
                .iter()
                .filter(|row| !row.gap)
                .map(|row| row.mem_current.map(|v| v as f64)),
        );
        let read = sum_values(
            part.rows
                .iter()
                .filter(|row| !row.gap)
                .map(|row| row.io_read_bps),
        );
        let write = sum_values(
            part.rows
                .iter()
                .filter(|row| !row.gap)
                .map(|row| row.io_write_bps),
        );
        if has_gap {
            self.cpu_total.push_held();
            self.memory_total.push_held();
            self.read_total.push_held();
            self.write_total.push_held();
        } else {
            self.latest_cpu = cpu.filter(|value| value.is_finite() && (*value as f32).is_finite());
            self.latest_memory = memory;
            self.cpu_total.push(history_value(cpu));
            self.memory_total.push(history_value(memory));
            self.read_total.push(history_value(read));
            self.write_total.push(history_value(write));
        }
    }
}

fn sum_values(values: impl Iterator<Item = Option<f64>>) -> Option<f64> {
    let mut sum = 0.0;
    let mut found = false;
    for value in values {
        if let Some(value) = value.and_then(valid_metric) {
            sum += value;
            found = true;
        }
    }
    found.then_some(sum)
}

fn valid_metric(value: f64) -> Option<f64> {
    (value.is_finite() && value >= 0.0 && (value as f32).is_finite()).then_some(value)
}

fn history_value(value: Option<f64>) -> f32 {
    value
        .filter(|value| value.is_finite() && *value >= 0.0)
        .map(|value| value as f32)
        .filter(|value| value.is_finite())
        .unwrap_or(f32::NAN)
}

fn fill_slot(slot: &mut Slot, row: &ContainerStatsRow) {
    slot.id = Some(Arc::clone(&row.id));
    if row.gap {
        slot.cpu_history.push_held();
        slot.memory_history.push_held();
        slot.read_history.push_held();
        slot.write_history.push_held();
        return;
    }

    slot.cpu = row.cpu_pct.and_then(valid_metric);
    slot.memory = row.mem_current;
    slot.peak = row.mem_peak;
    slot.pids = row.pids;
    slot.read = row.io_read_bps.and_then(valid_metric);
    slot.write = row.io_write_bps.and_then(valid_metric);
    slot.cpu_history.push(history_value(slot.cpu));
    slot.memory_history
        .push(slot.memory.map_or(f32::NAN, |v| v as f32));
    slot.read_history.push(history_value(slot.read));
    slot.write_history.push(history_value(slot.write));
}

pub(in crate::monitor) struct ContainersView<'a> {
    model: &'a ContainersModel,
    inventory: &'a [ContainerInfo],
    footprint: Option<ContainerFootprint>,
    sel: usize,
}

impl<'a> ContainersView<'a> {
    pub(in crate::monitor) fn new(
        model: &'a ContainersModel,
        inventory: &'a [ContainerInfo],
        footprint: Option<ContainerFootprint>,
        sel: usize,
    ) -> Self {
        Self {
            model,
            inventory,
            footprint,
            sel,
        }
    }
}

impl TabView for ContainersView<'_> {
    fn specs(&self) -> &'static [PanelSpec] {
        &SPECS
    }
    fn template(&self) -> Node {
        Node::Col(vec![
            (Size::Weight(1), Node::Panel(ESTATE)),
            (Size::Weight(1), Node::Panel(INVENTORY)),
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
    fn demand(&self, panel: PanelId) -> CollectorSet {
        let mut set = CollectorSet::empty();
        if panel != INVENTORY {
            set.insert(CollectorId::ContainersStats);
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
        let mut grid = CellGrid::new(rect.cols, rect.rows);
        let title = match placed.id {
            ESTATE => "Estate",
            INVENTORY => "containers",
            CPU => "CPU | % of one core",
            MEMORY => "Memory | peak",
            IO => "I/O | window peak",
            TABLE => "Containers",
            _ => return,
        };
        let absent = matches!(self.model.health, Health::Absent(_));
        let title = if placed.id == INVENTORY {
            let glyphs = crate::caps::active_glyphs();
            let sep = glyphs.middot;
            let ge = if crate::caps::unicode_level() == thegn_core::termcaps::UnicodeLevel::Ascii {
                ">="
            } else {
                "≥"
            };
            format!(
                "containers {sep} {}",
                super::ownership_note(self.inventory, self.footprint, sep, ge)
            )
        } else {
            title.to_owned()
        };
        let body = panel::panel(&mut grid, Rect::full(rect.cols, rect.rows), &title, None).rect();
        let has_history = match placed.id {
            CPU => ring_has_finite(&self.model.cpu_total),
            MEMORY => ring_has_finite(&self.model.memory_total),
            IO => {
                ring_has_finite(&self.model.read_total) || ring_has_finite(&self.model.write_total)
            }
            _ => false,
        };
        if matches!(placed.id, CPU | MEMORY | IO) && !absent && has_history {
            let label = match placed.id {
                CPU => format!(
                    "all-container CPU | scale {}",
                    fmt_option(
                        Some(Reading::Value(
                            max_ring(&self.model.cpu_total).max(1.0) as f64
                        )),
                        Unit::Percent
                    )
                ),
                MEMORY => format!(
                    "window peak {}",
                    fmt_option(ring_peak_reading(&self.model.memory_total), Unit::Bytes)
                ),
                _ => format!(
                    "read | scale {} B/s",
                    fmt_option(
                        Some(Reading::Value(max_pair(
                            &self.model.read_total,
                            &self.model.write_total
                        ) as f64)),
                        Unit::Bytes
                    )
                ),
            };
            grid.view(body).put_str(0, 0, &label, Default::default());
        }
        if placed.id == INVENTORY {
            self.paint_inventory(&mut grid, body);
        } else if absent {
            if placed.id == ESTATE {
                self.paint_estate(&mut grid, body);
            } else {
                grid.view(body)
                    .put_str(0, 0, absent_mark(self.model.health), Default::default());
            }
        } else if matches!(self.model.health, Health::Warming)
            && ((matches!(placed.id, CPU | MEMORY | IO) && !has_history)
                || (self.model.order.is_empty() && placed.id != ESTATE))
        {
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
        if placed.id == IO && body.rows > 1 && !absent && has_history {
            grid.view(body)
                .put_str(0, body.rows - 1, "write", Default::default());
        }
        Emitter::default().emit(
            &grid,
            Rect::full(rect.cols, rect.rows),
            surface,
            rect.x,
            rect.y,
        );
        if matches!(placed.id, CPU | MEMORY | IO) && !absent {
            self.paint_history(
                surface,
                placed.id,
                Rect {
                    x: rect.x + body.x,
                    y: rect.y + body.y + 1,
                    rows: body
                        .rows
                        .saturating_sub(if placed.id == IO { 2 } else { 1 }),
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
    fn paint_inventory(&self, grid: &mut CellGrid, body: Rect) {
        if self.inventory.is_empty() {
            grid.view(body)
                .put_str(0, 0, "no containers", Default::default());
            return;
        }
        if body.rows == 0 {
            return;
        }
        let mut header = grid.view(body);
        header.put_str(
            0,
            0,
            &format!(
                "{:<26}  {:<14}  {:<9}  {:<6}  {:<6}  net",
                "container", "backend", "health", "cpu", "mem"
            ),
            Default::default(),
        );
        let visible = body.rows.saturating_sub(1);
        if visible == 0 {
            return;
        }
        let start = self.sel.saturating_sub(visible.saturating_sub(1));
        let end = self.inventory.len().min(start.saturating_add(visible));
        for (line, (index, container)) in self
            .inventory
            .iter()
            .enumerate()
            .skip(start)
            .take(end - start)
            .enumerate()
        {
            let ownership = if container.ours {
                crate::seg::Tok::Hue(Hue::Green)
            } else {
                crate::seg::Tok::Slot(crate::chrome::S::Ghost)
            };
            let style = CellStyle {
                fg: StyleTok::from_tok(ownership).unwrap_or(StyleTok::INHERIT),
                bg: if index == self.sel {
                    StyleTok::from_tok(crate::seg::Tok::Series(SeriesRole::Selected))
                        .unwrap_or(StyleTok::INHERIT)
                } else {
                    StyleTok::INHERIT
                },
                attr: if index == self.sel {
                    crate::monitor::kit::grid::BOLD
                } else {
                    0
                },
                ..CellStyle::default()
            };
            let mark = if container.ours { "" } else { " (foreign)" };
            let health = match container_health(&container.status) {
                ContainerHealth::Healthy => "healthy",
                ContainerHealth::Unhealthy => "unhealthy",
                ContainerHealth::Starting => "starting",
                ContainerHealth::None => "up",
                ContainerHealth::Stopped => "stopped",
            };
            let net = if container.net.is_empty() {
                crate::caps::active_glyphs().emdash
            } else {
                &container.net
            };
            let name = format!(
                "{}{mark}",
                inventory_truncate(
                    &container.name,
                    26_usize.saturating_sub(mark.chars().count())
                )
            );
            let backend = inventory_truncate(&container.backend, 14);
            let cpu = if container.cpu.is_empty() {
                crate::caps::active_glyphs().emdash.to_owned()
            } else {
                inventory_truncate(&container.cpu, 6)
            };
            let mem = if container.mem.is_empty() {
                crate::caps::active_glyphs().emdash.to_owned()
            } else {
                inventory_truncate(&container.mem, 6)
            };
            let row = format!(
                "{}  {}  {}  {}  {}  {}",
                inventory_pad(&name, 26),
                inventory_pad(&backend, 14),
                inventory_pad(health, 9),
                inventory_pad(&cpu, 6),
                inventory_pad(&mem, 6),
                net
            );
            let y = body.y + line + 1;
            let mut row_view = grid.view(Rect { y, rows: 1, ..body });
            if index == self.sel {
                row_view.fill(CellStyle {
                    bg: style.bg,
                    attr: style.attr,
                    ..style
                });
            }
            row_view.put_str(0, 0, &row, style);
        }
    }

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
                fmt_option(self.model.latest_cpu.map(Reading::Value), Unit::Percent),
                fmt_option(self.model.latest_memory.map(Reading::Value), Unit::Bytes)
            );
            grid.view(Rect {
                y: body.y + 1,
                rows: body.rows - 1,
                ..body
            })
            .put_str(
                0,
                0,
                if matches!(self.model.health, Health::Absent(_)) {
                    absent_mark(self.model.health)
                } else {
                    &totals
                },
                Default::default(),
            );
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
            let flags = if self.model.untracked > 0 {
                format!("{flags} | +{} untracked", self.model.untracked)
            } else {
                flags
            };
            let flags = if self.model.rescans > 0 {
                format!("{flags} | rescans {}", self.model.rescans)
            } else {
                flags
            };
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
            let mut read = [f32::NAN; HISTORY_CAP];
            let mut write = [f32::NAN; HISTORY_CAP];
            let read_len = self.model.read_total.ordered(&mut read);
            let write_len = self.model.write_total.ordered(&mut write);
            let peak = max_pair(&self.model.read_total, &self.model.write_total);
            if peak > 0.0 {
                for val in &mut read[..read_len] {
                    if val.is_finite() {
                        *val = (*val / peak).clamp(0.0, 1.0);
                    }
                }
                for val in &mut write[..write_len] {
                    if val.is_finite() {
                        *val = (*val / peak).clamp(0.0, 1.0);
                    }
                }
            }
            let scale = if graphs::table().braille { 2 } else { 1 };
            let visible = read_len
                .max(write_len)
                .min(graph_area.cols.saturating_mul(scale));
            let read_skip = read_len.saturating_sub(visible);
            let write_skip = write_len.saturating_sub(visible);
            let read = &read[read_skip..read_len];
            let write = &write[write_skip..write_len];
            paint_mirrored_history_runs(read, write, graph_area, graphs::table().braille, surface);
        } else if len > 0 {
            paint_finite_history_runs(
                &values[..len],
                ramp,
                graph_area,
                graphs::table().braille,
                surface,
            );
        }
    }

    fn paint_table(&self, grid: &mut CellGrid, body: Rect) {
        let mut header = grid.view(body);
        header.put_str(TABLE_ID, 0, "id", Default::default());
        header.put_str(TABLE_CPU, 0, "cpu%", Default::default());
        let cpu_scale = max_slot_cpu(self.model).max(1.0);
        header.put_str(
            TABLE_CPU_SPARK,
            0,
            &format!(
                "cpu/core {}",
                fmt_option(Some(Reading::Value(cpu_scale as f64)), Unit::Percent)
            ),
            Default::default(),
        );
        header.put_str(TABLE_MEM, 0, "memory", Default::default());
        header.put_str(TABLE_MEM_SPARK, 0, "mem/peak", Default::default());
        header.put_str(TABLE_PIDS, 0, "pids", Default::default());
        header.put_str(TABLE_IO, 0, "rd/wr B/s", Default::default());
        if self.model.untracked > 0 {
            header.put_str(
                TABLE_IO + 8,
                0,
                &format!("+{} untracked", self.model.untracked),
                Default::default(),
            );
        }
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
            let memory_peak = fmt_option(
                slot.peak.map(|value| Reading::Value(value as f64)),
                Unit::Bytes,
            );
            let pids = fmt_option(
                slot.pids.map(|value| Reading::Value(value as f64)),
                Unit::Count,
            );
            let read = fmt_option(slot.read.map(Reading::Value), Unit::Bytes);
            let write = fmt_option(slot.write.map(Reading::Value), Unit::Bytes);
            let mut row = grid.view(Rect {
                y: body.y + visible + 1,
                rows: body.rows.saturating_sub(visible + 1),
                ..body
            });
            row.put_str(TABLE_ID, 0, &format!("{short:<12}"), Default::default());
            row.put_str(TABLE_CPU, 0, &format!("{cpu:>5}"), Default::default());
            row.put_str(
                TABLE_MEM,
                0,
                &format!("{memory:>5}/{memory_peak:>5}"),
                Default::default(),
            );
            row.put_str(TABLE_PIDS, 0, &format!("{pids:>5}"), Default::default());
            row.put_str(TABLE_IO, 0, &format!("{read}/{write}"), Default::default());
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
            let cpu_scale = max_slot_cpu(self.model).max(1.0);
            for value in &mut history[..len] {
                if value.is_finite() {
                    *value = (*value / cpu_scale).clamp(0.0, 1.0);
                }
            }
            let area = Rect {
                x: rect.x + body.x + TABLE_CPU_SPARK,
                y: rect.y + body.y + visible + 1,
                cols: body
                    .cols
                    .saturating_sub(TABLE_CPU_SPARK)
                    .min(TABLE_MEM - TABLE_CPU_SPARK),
                rows: 1,
            };
            paint_finite_spark_runs(&history[..len], RampId::Util, area, surface);
            let mut memory = [f32::NAN; HISTORY_CAP];
            let memory_len = self.model.slots[*index].memory_history.ordered(&mut memory);
            let memory_area = Rect {
                x: rect.x + body.x + TABLE_MEM_SPARK,
                y: rect.y + body.y + visible + 1,
                cols: body
                    .cols
                    .saturating_sub(TABLE_MEM_SPARK)
                    .min(TABLE_PIDS - TABLE_MEM_SPARK),
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

fn paint_finite_history_runs(
    values: &[f32],
    ramp: RampId,
    area: Rect,
    braille: bool,
    surface: &mut Surface,
) {
    let samples_per_cell = if braille { 2 } else { 1 };
    let max_samples = area.cols.saturating_mul(samples_per_cell);
    let start_index = values.len().saturating_sub(max_samples);
    let visible = &values[start_index..];
    let aligned = crate::monitor::kit::history_rect::graph_rect(area, visible.len(), braille);
    let slots = aligned.cols.saturating_mul(samples_per_cell);
    let shown = visible.len().min(slots);
    let skip = visible.len().saturating_sub(shown);
    let pad = slots.saturating_sub(shown);
    let visible = &visible[skip..];
    let mut start = 0;
    while start < visible.len() {
        while start < visible.len() && !visible[start].is_finite() {
            start += 1;
        }
        let mut end = start;
        while end < visible.len() && visible[end].is_finite() {
            end += 1;
        }
        if end > start {
            let slot_start = pad + start;
            let offset = if braille { slot_start % 2 } else { 0 };
            let first_col = slot_start / samples_per_cell;
            let run_cols = (offset + end - start).div_ceil(samples_per_cell);
            let mut aligned_run = [f32::NAN; HISTORY_CAP + 2];
            let used_len = offset + end - start;
            let run_len = if braille {
                used_len + used_len % 2
            } else {
                used_len
            };
            aligned_run[offset..offset + end - start].copy_from_slice(&visible[start..end]);
            graphs::paint_history_graph(
                &aligned_run[..run_len],
                ramp,
                Rect {
                    x: aligned.x + first_col,
                    cols: run_cols,
                    ..area
                },
                surface,
            );
        }
        start = end.max(start + 1);
    }
}

fn paint_mirrored_history_runs(
    read: &[f32],
    write: &[f32],
    area: Rect,
    braille: bool,
    surface: &mut Surface,
) {
    if area.cols == 0 || area.rows == 0 || read.is_empty() || write.is_empty() {
        return;
    }
    let columns = area.cols;
    let mut raster = thegn_core::viz_raster::CellBuf::new();
    raster.reset(columns, area.rows);
    thegn_core::viz_raster::mirrored(read, write, graphs::table(), &mut raster);
    let mut grid = CellGrid::new(columns, area.rows);
    let per_cell = if braille { 2 } else { 1 };
    let mut view = grid.view(Rect::full(columns, area.rows));
    let upper_rows = area.rows.div_ceil(2);
    let slots = columns.saturating_mul(per_cell);
    let offset = slots.saturating_sub(read.len().max(write.len()));
    for x in 0..columns {
        let first_slot = x * per_cell;
        let last_slot = first_slot + per_cell;
        let has_data = (first_slot..last_slot).any(|slot| {
            if slot < offset {
                return false;
            }
            let index = slot - offset;
            read.get(index).is_some_and(|value| value.is_finite())
                && write.get(index).is_some_and(|value| value.is_finite())
        });
        if !has_data {
            continue;
        }
        for y in 0..area.rows {
            if let Some(cell) = raster.get(x, y) {
                let ramp = if y < upper_rows {
                    RampId::Read
                } else {
                    RampId::Write
                };
                view.put(
                    x,
                    y,
                    CellStyle {
                        ch: cell.ch,
                        fg: StyleTok::from_tok(crate::seg::Tok::Ramp(ramp, cell.fg))
                            .unwrap_or(StyleTok::INHERIT),
                        ..CellStyle::default()
                    },
                );
            }
        }
    }
    Emitter::default().emit(
        &grid,
        Rect::full(columns, area.rows),
        surface,
        area.x,
        area.y,
    );
}

fn inventory_truncate(value: &str, max_chars: usize) -> String {
    if crate::seg::cells(value) <= max_chars {
        return value.to_owned();
    }
    let ellipsis = crate::caps::active_glyphs().ellipsis;
    let text = crate::seg::take_cols(value, max_chars.saturating_sub(crate::seg::cells(ellipsis)));
    format!("{text}{ellipsis}")
}

fn inventory_pad(value: &str, width: usize) -> String {
    format!(
        "{value}{}",
        " ".repeat(width.saturating_sub(crate::seg::cells(value)))
    )
}

fn absent_mark(health: Health) -> &'static str {
    match health {
        Health::Absent(Absent::NoPermission) => crate::caps::glyph(crate::caps::Glyph::Lock),
        _ => crate::caps::active_glyphs().emdash,
    }
}

fn paint_finite_spark_runs(values: &[f32], ramp: RampId, area: Rect, surface: &mut Surface) {
    let count = values.len().min(area.cols);
    let tail_start = values.len().saturating_sub(count);
    let aligned = crate::monitor::kit::history_rect::graph_rect(area, count, false);
    if values.is_empty() || aligned.cols == 0 {
        return;
    }
    let values = &values[tail_start..];
    let mut start = 0;
    while start < count {
        while start < count && !values[start].is_finite() {
            start += 1;
        }
        let mut end = start;
        while end < count && values[end].is_finite() {
            end += 1;
        }
        if end > start {
            let cols = end - start;
            graphs::paint_sparkline(
                &values[start..end],
                cols,
                ramp,
                Rect {
                    x: aligned.x + start,
                    cols,
                    ..area
                },
                surface,
            );
        }
        start = end.max(start + 1);
    }
}

fn ring_has_finite(ring: &Ring) -> bool {
    ring.values.iter().any(|value| value.is_finite())
}

fn max_slot_cpu(model: &ContainersModel) -> f32 {
    model
        .slots
        .iter()
        .flat_map(|slot| slot.cpu_history.values.iter().copied())
        .filter(|value| value.is_finite())
        .fold(0.0, f32::max)
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
