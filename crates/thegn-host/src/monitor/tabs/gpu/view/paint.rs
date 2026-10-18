use super::{DEVICE_CAP, GpuView, HISTORY_CAP};
use crate::caps::{self, Glyph};
use crate::compositor::Rect;
use crate::monitor::kit::emit::Emitter;
use crate::monitor::kit::grid::CellGrid;
use termwiz::surface::Surface;
use thegn_core::numfmt::{NumFmt, Unit};
use thegn_metrics::collect::{
    health::{Absent, Health},
    model::{amd_smu::AmdSmuStatus, gpu::GpuKind, gpu::RuntimeStatus},
};

struct AuxRing<'a> {
    label: &'a str,
    values: &'a [f32; HISTORY_CAP],
    next: usize,
    len: usize,
    ramp: thegn_core::theme::RampId,
}

fn draw_aux_ring(
    surface: &mut Surface,
    rect: Rect,
    body: Rect,
    row: usize,
    ring: AuxRing<'_>,
) -> usize {
    if ring.len == 0 {
        return row;
    }
    let mut values = [0.0_f32; HISTORY_CAP];
    let len = super::BusyHistory::ordered_ring(ring.values, ring.next, ring.len, &mut values);
    let label = format!("{} 0-100%", ring.label);
    draw_labelled_spark(surface, rect, body, row, &label, &values[..len], ring.ramp);
    row + 1
}

fn draw_labelled_spark(
    surface: &mut Surface,
    rect: Rect,
    body: Rect,
    row: usize,
    label: &str,
    values: &[f32],
    ramp: thegn_core::theme::RampId,
) {
    const LABEL_COLS: usize = 28;
    if values.is_empty() || row >= body.rows || body.cols <= LABEL_COLS {
        return;
    }
    let mut label_grid = CellGrid::new(LABEL_COLS, 1);
    label_grid
        .view(Rect::full(LABEL_COLS, 1))
        .put_str(0, 0, label, Default::default());
    Emitter::default().emit(
        &label_grid,
        Rect::full(LABEL_COLS, 1),
        surface,
        rect.x + body.x,
        rect.y + body.y + row,
    );
    let area = Rect {
        x: rect.x + body.x + LABEL_COLS,
        y: rect.y + body.y + row,
        cols: body.cols - LABEL_COLS,
        rows: 1,
    };
    let graph = crate::monitor::kit::history_rect::graph_rect(
        area,
        values.len(),
        crate::monitor::graphs::table().braille,
    );
    crate::monitor::graphs::paint_sparkline(values, graph.cols, ramp, graph, surface);
}

fn activity_label(index: usize) -> &'static str {
    match index {
        0 => "gfx",
        1 => "mm",
        2 => "vcn",
        3 => "umc",
        4 => "ipu0",
        5 => "ipu1",
        6 => "ipu2",
        7 => "ipu3",
        8 => "ipu4",
        9 => "ipu5",
        10 => "ipu6",
        _ => "ipu7",
    }
}

fn mark(health: Health) -> String {
    match health {
        Health::Warming => caps::active_glyphs().ellipsis.to_owned(),
        Health::Absent(Absent::NoPermission) => {
            format!("{} root-only", caps::glyph(Glyph::Lock))
        }
        Health::Absent(_) | Health::Failing { .. } | Health::Ok => {
            caps::active_glyphs().emdash.to_owned()
        }
    }
}

fn display_health(primary: Health, secondary: Health) -> Health {
    match (primary, secondary) {
        (Health::Absent(Absent::NoPermission), _) => primary,
        (_, Health::Absent(Absent::NoPermission)) => secondary,
        (Health::Warming, _) => primary,
        (_, Health::Warming) => secondary,
        _ => primary,
    }
}

impl GpuView<'_> {
    pub(in crate::monitor::tabs::gpu) fn paint_aux_history(
        &self,
        surface: &mut Surface,
        rect: Rect,
        body: Rect,
        memory: bool,
    ) {
        if body.rows < 4 || body.cols < 28 {
            return;
        }
        let data_rows = self.0.devices.devices.len().min(body.rows);
        let mut output_row = data_rows;
        for (device_index, engine) in self.0.engines.devices.iter().take(DEVICE_CAP).enumerate() {
            if engine.runtime_status != RuntimeStatus::Active || output_row >= body.rows {
                continue;
            }
            let Some(history) = self
                .0
                .histories
                .iter()
                .find(|item| item.slot == engine.slot.as_str())
            else {
                continue;
            };
            let (first, first_next, first_len, second, second_next, second_len, labels) = if memory
            {
                (
                    &history.memory,
                    history.memory_next,
                    history.memory_len,
                    &history.gtt,
                    history.gtt_next,
                    history.gtt_len,
                    ["VRAM / total", "GTT / total"],
                )
            } else {
                (
                    &history.clocks,
                    history.clocks_next,
                    history.clocks_len,
                    &history.power,
                    history.power_next,
                    history.power_len,
                    ["gfx clock / max", "power / cap"],
                )
            };
            let first_label = format!("GPU{device_index} {}", labels[0]);
            let second_label = format!("GPU{device_index} {}", labels[1]);
            output_row = draw_aux_ring(
                surface,
                rect,
                body,
                output_row,
                AuxRing {
                    label: &first_label,
                    values: first,
                    next: first_next,
                    len: first_len,
                    ramp: thegn_core::theme::RampId::Mem,
                },
            );
            if output_row < body.rows {
                output_row = draw_aux_ring(
                    surface,
                    rect,
                    body,
                    output_row,
                    AuxRing {
                        label: &second_label,
                        values: second,
                        next: second_next,
                        len: second_len,
                        ramp: thegn_core::theme::RampId::Util,
                    },
                );
            }
        }
    }

    pub(in crate::monitor::tabs::gpu) fn paint_busy_history(
        &self,
        surface: &mut Surface,
        rect: Rect,
        body: Rect,
    ) {
        if matches!(self.0.engine_health, Health::Absent(_)) {
            return;
        }
        let Some(engine) = self.0.engines.devices.first() else {
            return;
        };
        if self.0.engines.devices.len() > 1 {
            for (index, engine) in self.0.engines.devices.iter().enumerate().take(body.rows) {
                if engine.runtime_status != RuntimeStatus::Active {
                    continue;
                }
                let Some(history) = self
                    .0
                    .histories
                    .iter()
                    .find(|item| item.slot == engine.slot.as_str())
                else {
                    continue;
                };
                if history.len == 0 || body.cols <= 36 {
                    continue;
                }
                let mut values = [0.0_f32; HISTORY_CAP];
                let len = history.ordered_into(&mut values);
                let graph_rect = crate::monitor::kit::history_rect::graph_rect(
                    Rect {
                        x: rect.x + body.x + 36,
                        y: rect.y + body.y + index,
                        cols: body.cols - 36,
                        rows: 1,
                    },
                    len,
                    crate::monitor::graphs::table().braille,
                );
                crate::monitor::graphs::paint_sparkline(
                    &values[..len],
                    graph_rect.cols,
                    thegn_core::theme::RampId::Util,
                    graph_rect,
                    surface,
                );
            }
        } else {
            if engine.runtime_status != RuntimeStatus::Active {
                return;
            }
            let Some(history) = self
                .0
                .histories
                .iter()
                .find(|item| item.slot == engine.slot.as_str())
            else {
                return;
            };
            if history.len > 0 && body.rows >= 3 && body.cols >= 12 {
                let mut values = [0.0_f32; HISTORY_CAP];
                let len = history.ordered_into(&mut values);
                let graph_rect = crate::monitor::kit::history_rect::graph_rect(
                    Rect {
                        x: rect.x + body.x,
                        y: rect.y + body.y + 1,
                        cols: body.cols,
                        rows: body.rows.saturating_sub(2),
                    },
                    len,
                    crate::monitor::graphs::table().braille,
                );
                crate::monitor::graphs::paint_history_graph(
                    &values[..len],
                    thegn_core::theme::RampId::Util,
                    graph_rect,
                    surface,
                );
                let mut vcn = [0.0_f32; HISTORY_CAP];
                let vcn_len = history.ordered_vcn_into(&mut vcn);
                if vcn_len > 0 {
                    let graph_y = rect.y + body.y + body.rows - 1;
                    let mut label = CellGrid::new(5, 1);
                    label
                        .view(Rect::full(5, 1))
                        .put_str(0, 0, "vcn %", Default::default());
                    Emitter::default().emit(
                        &label,
                        Rect::full(5, 1),
                        surface,
                        rect.x + body.x,
                        graph_y,
                    );
                    let graph_rect = crate::monitor::kit::history_rect::graph_rect(
                        Rect {
                            x: rect.x + body.x + 5,
                            y: graph_y,
                            cols: body.cols.saturating_sub(5),
                            rows: 1,
                        },
                        vcn_len,
                        crate::monitor::graphs::table().braille,
                    );
                    crate::monitor::graphs::paint_sparkline(
                        &vcn[..vcn_len],
                        graph_rect.cols,
                        thegn_core::theme::RampId::Util,
                        graph_rect,
                        surface,
                    );
                }
            }
        }
        let mut output_row = self.0.engines.devices.len().min(body.rows);
        for (device_index, engine) in self.0.engines.devices.iter().take(DEVICE_CAP).enumerate() {
            if output_row >= body.rows {
                break;
            }
            if engine.runtime_status != RuntimeStatus::Active {
                continue;
            }
            let Some(history) = self
                .0
                .histories
                .iter()
                .find(|item| item.slot == engine.slot.as_str())
            else {
                continue;
            };
            for (index, ring) in history.activity.iter().enumerate() {
                if output_row >= body.rows {
                    break;
                }
                if ring.len == 0 {
                    continue;
                }
                let label = activity_label(index);
                let mut values = [0.0_f32; HISTORY_CAP];
                let len = ring.ordered_into(&mut values);
                draw_labelled_spark(
                    surface,
                    rect,
                    body,
                    output_row,
                    &format!("GPU{device_index} {label} 0-100%"),
                    &values[..len],
                    thegn_core::theme::RampId::Util,
                );
                output_row += 1;
            }
        }
    }

    pub(super) fn paint_devices(&self, grid: &mut CellGrid, body: Rect) {
        let mut view = grid.view(body);
        if matches!(self.0.device_health, Health::Absent(_)) {
            view.put_str(
                0,
                0,
                &format!("{} no GPU device data", mark(self.0.device_health)),
                Default::default(),
            );
            return;
        }
        if self.0.devices.devices.is_empty() {
            let message = if self.0.device_health == Health::Warming {
                "waiting for GPU devices"
            } else {
                "no GPU detected"
            };
            view.put_str(
                0,
                0,
                &format!("{} {message}", mark(self.0.device_health)),
                Default::default(),
            );
            return;
        }
        for (index, device) in self.0.devices.devices.iter().take(body.rows).enumerate() {
            let kind = match device.kind {
                GpuKind::Dgpu => "dGPU",
                GpuKind::Igpu => "iGPU",
                GpuKind::Apu => "APU",
                GpuKind::Virtual => "virtual",
                GpuKind::Unknown => "GPU",
            };
            let driver = device.driver.as_deref().unwrap_or("unknown driver");
            let status = match device.runtime_status {
                RuntimeStatus::Active => "active",
                RuntimeStatus::Suspended => "suspended",
                RuntimeStatus::Suspending => "suspending",
                RuntimeStatus::Unsupported => "runtime PM unsupported",
                RuntimeStatus::Unknown => "status unknown",
            };
            let identity = device.family.as_deref().unwrap_or("GPU");
            view.put_str(
                0,
                index,
                &format!(
                    "{} {}  {}  {}  {}  {}",
                    device.slot,
                    identity,
                    kind,
                    driver,
                    status,
                    device.pci_id.as_deref().unwrap_or("")
                ),
                Default::default(),
            );
        }
    }

    pub(super) fn paint_engines(&self, grid: &mut CellGrid, body: Rect) {
        let mut view = grid.view(body);
        if matches!(self.0.engine_health, Health::Absent(_)) {
            view.put_str(
                0,
                0,
                &format!("{} engine counters unavailable", mark(self.0.engine_health)),
                Default::default(),
            );
            return;
        }
        if self.0.engines.devices.is_empty() {
            view.put_str(
                0,
                0,
                &format!("{} waiting for engine readings", mark(self.0.engine_health)),
                Default::default(),
            );
            return;
        }
        for (row, engine) in self.0.engines.devices.iter().take(DEVICE_CAP).enumerate() {
            if row >= body.rows {
                break;
            }
            let busy = if engine.runtime_status != RuntimeStatus::Active {
                format!(
                    "asleep (runtime-suspended) {} not polled to avoid waking it",
                    crate::caps::active_glyphs().middot
                )
            } else if let Some(value) = engine.gfx_busy_pct {
                format!("gfx {value}%")
            } else {
                format!("gfx {}", crate::caps::active_glyphs().emdash)
            };
            let vcn = if engine.runtime_status == RuntimeStatus::Active {
                engine.vcn_busy_pct.map_or_else(
                    || crate::caps::active_glyphs().emdash.to_owned(),
                    |value| format!("{value}%"),
                )
            } else {
                crate::caps::active_glyphs().emdash.to_owned()
            };
            view.put_str(
                0,
                row,
                &format!("{}  {}  vcn {}", engine.slot.as_str(), busy, vcn),
                Default::default(),
            );
        }
    }

    pub(super) fn paint_memory(&self, grid: &mut CellGrid, body: Rect) {
        let mut view = grid.view(body);
        if matches!(self.0.engine_health, Health::Absent(_))
            && matches!(self.0.device_health, Health::Absent(_))
        {
            let health = display_health(self.0.device_health, self.0.engine_health);
            view.put_str(
                0,
                0,
                &format!("{} memory readings unavailable", mark(health)),
                Default::default(),
            );
            return;
        }
        let mut row = 0;
        for device in self.0.devices.devices.iter().take(body.rows) {
            let engine = self
                .0
                .engines
                .devices
                .iter()
                .find(|item| item.slot.as_str() == device.slot);
            let (used_vram, total_vram, used_gtt, total_gtt) =
                engine.map_or((None, device.vram_total, None, device.gtt_total), |item| {
                    (
                        item.vram_used
                            .filter(|_| item.runtime_status == RuntimeStatus::Active),
                        item.vram_total.or(device.vram_total),
                        item.gtt_used
                            .filter(|_| item.runtime_status == RuntimeStatus::Active),
                        item.gtt_total.or(device.gtt_total),
                    )
                });
            let shared = device.kind == GpuKind::Apu;
            let name = "VRAM";
            let unknown = crate::caps::active_glyphs().emdash;
            let memory = match (used_vram, total_vram) {
                (Some(used), Some(total)) => format!("{name} {} / {}", bytes(used), bytes(total)),
                (Some(used), None) => format!("{name} {} / {unknown}", bytes(used)),
                (None, Some(total)) => format!("{name} {unknown} / {}", bytes(total)),
                (None, None) => format!("{name} {unknown}"),
            };
            let gtt = match (used_gtt, total_gtt) {
                (Some(used), Some(total)) => format!("gtt {} / {}", bytes(used), bytes(total)),
                (Some(used), None) => format!("gtt {} / {unknown}", bytes(used)),
                (None, Some(total)) => format!("gtt {unknown} / {}", bytes(total)),
                (None, None) => format!("gtt {unknown}"),
            };
            let memory = if shared {
                format!(
                    "shared  vram {}  {}  {gtt}",
                    memory.trim_start_matches("VRAM "),
                    caps::active_glyphs().middot
                )
            } else {
                format!(
                    "{}  {}  GTT {}",
                    memory,
                    caps::active_glyphs().middot,
                    gtt.trim_start_matches("gtt ")
                )
            };
            view.put_str(
                0,
                row,
                &format!("{}  {}", device.slot, memory),
                Default::default(),
            );
            row += 1;
        }
        if row == 0 {
            view.put_str(
                0,
                0,
                &format!(
                    "{} no memory readings",
                    mark(display_health(self.0.device_health, self.0.engine_health))
                ),
                Default::default(),
            );
        }
    }

    pub(super) fn paint_clocks(&self, grid: &mut CellGrid, body: Rect) {
        let mut view = grid.view(body);
        if matches!(self.0.engine_health, Health::Absent(_))
            && matches!(self.0.smu_health, Health::Absent(_))
        {
            let health = display_health(self.0.engine_health, self.0.smu_health);
            view.put_str(
                0,
                0,
                &format!("{} clock and SMU readings unavailable", mark(health)),
                Default::default(),
            );
            return;
        }
        let absent = crate::caps::active_glyphs().emdash;
        let mut row = 0;
        for engine in self.0.engines.devices.iter().take(body.rows) {
            let device = self.device(engine.slot.as_str());
            let active = engine.runtime_status == RuntimeStatus::Active;
            let sclk = clock_label(
                engine.sclk.current_mhz.filter(|_| active),
                &engine.sclk.levels_mhz
                    [..usize::from(engine.sclk.len).min(engine.sclk.levels_mhz.len())],
                absent,
            );
            let mclk = clock_label(
                engine.mclk.current_mhz.filter(|_| active),
                &engine.mclk.levels_mhz
                    [..usize::from(engine.mclk.len).min(engine.mclk.levels_mhz.len())],
                absent,
            );
            let smu = device.and_then(|device| {
                self.0
                    .smu
                    .cards
                    .iter()
                    .find(|card| u32::from(card.card) == device.card)
            });
            let (power, temp) = smu
                .and_then(|card| match card.decoded {
                    AmdSmuStatus::Decoded(_) if !card.suspended => Some((
                        card.metrics
                            .power_socket
                            .map(|mw| format!("{:.1}W", mw as f32 / 1000.0)),
                        card.metrics
                            .temperature_gfx
                            .map(|centi| format!("{} C", centi / 100)),
                    )),
                    _ => None,
                })
                .unwrap_or((None, None));
            view.put_str(
                0,
                row,
                &format!(
                    "{}  sclk {sclk}  mclk {mclk}  power {}  gfx {}",
                    engine.slot.as_str(),
                    power.unwrap_or_else(|| absent.to_owned()),
                    temp.unwrap_or_else(|| absent.to_owned())
                ),
                Default::default(),
            );
            row += 1;
        }
        if row == 0 {
            view.put_str(
                0,
                0,
                &format!(
                    "{} waiting for clock readings",
                    mark(display_health(self.0.engine_health, self.0.smu_health))
                ),
                Default::default(),
            );
        }
    }
}

fn clock_label(current: Option<u32>, levels: &[u32], absent: &str) -> String {
    let Some(current) = current else {
        return absent.to_owned();
    };
    if levels.is_empty() {
        return absent.to_owned();
    }
    let ladder = levels
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(&format!(" {} ", crate::caps::active_glyphs().middot));
    let Some(index) = levels.iter().position(|level| *level == current) else {
        return absent.to_owned();
    };
    let marker = format!("{current} MHz [{}/{}]", index + 1, levels.len());
    format!("{marker} ({ladder})")
}

fn bytes(value: u64) -> String {
    crate::monitor::kit::edge_fmt(NumFmt::default()).fmt(value as f64, Unit::Bytes, 8)
}
