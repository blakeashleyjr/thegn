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

fn history_graph_rect(rect: Rect, body: Rect, len: usize, braille: bool) -> Rect {
    let used = if braille { len.div_ceil(2) } else { len };
    let cols = body.cols.min(used);
    Rect {
        x: rect.x + body.x + body.cols.saturating_sub(cols),
        y: rect.y + body.y + 1,
        cols,
        rows: body.rows.saturating_sub(2),
    }
}

impl GpuView<'_> {
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
                crate::monitor::graphs::paint_sparkline(
                    &values[..len],
                    body.cols - 36,
                    thegn_core::theme::RampId::Util,
                    Rect {
                        x: rect.x + body.x + 36,
                        y: rect.y + body.y + index,
                        cols: body.cols - 36,
                        rows: 1,
                    },
                    surface,
                );
            }
            return;
        }
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
        if history.len == 0 || body.rows < 3 || body.cols < 12 {
            return;
        }
        let mut values = [0.0_f32; HISTORY_CAP];
        let len = history.ordered_into(&mut values);
        let graph_rect =
            history_graph_rect(rect, body, len, crate::monitor::graphs::table().braille);
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
            Emitter::default().emit(&label, Rect::full(5, 1), surface, rect.x + body.x, graph_y);
            crate::monitor::graphs::paint_sparkline(
                &vcn[..vcn_len],
                body.cols.saturating_sub(5),
                thegn_core::theme::RampId::Util,
                Rect {
                    x: rect.x + body.x + 5,
                    y: graph_y,
                    cols: body.cols.saturating_sub(5),
                    rows: 1,
                },
                surface,
            );
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
        let mut row = 0;
        for engine in self.0.engines.devices.iter().take(DEVICE_CAP) {
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
            row += 1;
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
                        item.vram_used,
                        item.vram_total.or(device.vram_total),
                        item.gtt_used,
                        item.gtt_total.or(device.gtt_total),
                    )
                });
            let shared = device.kind == GpuKind::Apu;
            let name = "VRAM";
            let memory = match (used_vram, total_vram) {
                (Some(used), Some(total)) => format!("{name} {} / {}", bytes(used), bytes(total)),
                (_, Some(total)) if shared => format!(
                    "VRAM {} / {}",
                    crate::caps::active_glyphs().emdash,
                    bytes(total)
                ),
                _ => format!("{name} {}", crate::caps::active_glyphs().emdash),
            };
            let gtt = match (used_gtt, total_gtt) {
                (Some(used), Some(total)) => format!("gtt {} / {}", bytes(used), bytes(total)),
                _ => format!("gtt {}", crate::caps::active_glyphs().emdash),
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
