#![allow(dead_code)] // wired into the overlay dispatch in batch 48 (see THE-888)

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
use termwiz::surface::Surface;
use thegn_core::theme::RampId;

mod paint;
use thegn_metrics::collect::{
    CollectorId, CollectorSet,
    frame::MonitorFrame,
    model::{
        amd_smu::AmdSmuStatus,
        cpu_policy::{CpuPolicy, PowerPolicy},
        power_supply::{SupplyKind, SupplyReading, SupplyStatus},
    },
    parse::amd_gpu_metrics::Decoded,
    policy::power::{self, EffectiveState, Ewma, SessionEnergy},
};

const BATTERY: PanelId = PanelId("power.battery");
const RAILS: PanelId = PanelId("power.rails");
const POLICY: PanelId = PanelId("power.policy");
const HISTORY_CAP: usize = 120;
const INSTANCE_CAP: usize = 32;
const VARIANT: [Variant; 1] = [Variant {
    kind: VariantKind::Full,
    min: (28, 5),
    pref: (72, 14),
}];
const SPECS: [PanelSpec; 3] = [
    PanelSpec {
        id: RAILS,
        priority: 0,
        variants: &VARIANT,
    },
    PanelSpec {
        id: BATTERY,
        priority: 1,
        variants: &VARIANT,
    },
    PanelSpec {
        id: POLICY,
        priority: 2,
        variants: &VARIANT,
    },
];

#[derive(Debug, Clone, Default, PartialEq)]
pub(in crate::monitor) struct PowerModel {
    pub(super) batteries: Vec<BatteryRow>,
    pub(super) adapters: Vec<AdapterRow>,
    pub(super) zones: Vec<ZoneRow>,
    pub(super) policy: PowerPolicy,
    pub(super) cpu_policy: Vec<CpuPolicy>,
    pub(super) socket_w: Option<f32>,
    pub(super) apu_w: Option<f32>,
    pub(super) gfx_w: Option<f32>,
    pub(super) core_w: Option<f32>,
    pub(super) system_w: Option<f32>,
    pub(super) battery_w: Option<f32>,
    pub(super) state: Option<EffectiveState>,
    pub(super) eta_hours: Option<f64>,
    pub(super) session: SessionEnergy,
    ewma: Ewma,
    history: Vec<f32>,
    history_scale_w: f32,
}
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct BatteryRow {
    pub(super) name: String,
    pub(super) status: Option<SupplyStatus>,
    pub(super) capacity: Option<u8>,
    pub(super) now_wh: Option<f64>,
    pub(super) full_wh: Option<f64>,
    pub(super) design_wh: Option<f64>,
    pub(super) power_w: Option<f32>,
    pub(super) cycles: Option<u32>,
}
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct AdapterRow {
    pub(super) name: String,
    pub(super) online: Option<bool>,
}
#[derive(Debug, Clone, PartialEq)]
pub(super) struct ZoneRow {
    pub(super) name: String,
    pub(super) watts: Option<f32>,
}
impl Default for ZoneRow {
    fn default() -> Self {
        Self {
            name: String::new(),
            watts: None,
        }
    }
}

impl PowerModel {
    pub(in crate::monitor) fn absorb_frame(&mut self, frame: &MonitorFrame) {
        let mut battery_count = 0;
        let mut adapter_count = 0;
        for supply in &frame.parts.power_supply.supplies {
            match supply.kind {
                SupplyKind::Battery if !supply.device_scope && battery_count < INSTANCE_CAP => {
                    if self.batteries.capacity() == 0 {
                        self.batteries.reserve_exact(INSTANCE_CAP);
                    }
                    if battery_count == self.batteries.len() {
                        self.batteries.push(BatteryRow::default());
                    }
                    fill_battery(&mut self.batteries[battery_count], supply);
                    battery_count += 1;
                }
                SupplyKind::Mains | SupplyKind::Usb if adapter_count < INSTANCE_CAP => {
                    if self.adapters.capacity() == 0 {
                        self.adapters.reserve_exact(INSTANCE_CAP);
                    }
                    if adapter_count == self.adapters.len() {
                        self.adapters.push(AdapterRow::default());
                    }
                    let row = &mut self.adapters[adapter_count];
                    row.name.clear();
                    row.name.push_str(&supply.name);
                    row.online = supply.online;
                    adapter_count += 1;
                }
                _ => {}
            }
        }
        self.batteries.truncate(battery_count);
        self.adapters.truncate(adapter_count);
        let ac_online = self
            .adapters
            .iter()
            .any(|adapter| adapter.online == Some(true));
        let ac_state = (!self.adapters.is_empty()).then_some(ac_online);
        self.state = self.batteries.first().and_then(|battery| {
            power::effective_state(battery.status, ac_state, battery.capacity, None)
        });
        self.battery_w = self
            .batteries
            .first()
            .and_then(|battery| battery.power_w)
            .zip(self.state)
            .map(|(watts, state)| power::signed_battery_watts(watts, state));
        if let Some(watts) = self.battery_w {
            let _smoothed = self.ewma.update(watts, frame.at_ms);
        }
        self.eta_hours = self.batteries.first().and_then(|battery| {
            power::eta(
                battery.now_wh?,
                battery.full_wh?,
                self.ewma.watts()?,
                self.state?,
            )
        });
        self.policy = frame.parts.cpu_freq.power;
        self.cpu_policy.clone_from(&frame.parts.cpu_freq.policies);
        for (index, zone) in frame
            .parts
            .powercap
            .zones
            .iter()
            .take(INSTANCE_CAP)
            .enumerate()
        {
            if self.zones.capacity() == 0 {
                self.zones.reserve_exact(INSTANCE_CAP);
            }
            if index == self.zones.len() {
                self.zones.push(ZoneRow::default());
            }
            let row = &mut self.zones[index];
            row.name.clear();
            row.name.push_str(&zone.name);
            row.watts = zone.watts.filter(|value| value.is_finite());
        }
        self.zones
            .truncate(frame.parts.powercap.zones.len().min(INSTANCE_CAP));
        self.socket_w = None;
        self.apu_w = None;
        self.gfx_w = None;
        self.core_w = None;
        self.system_w = None;
        for card in &frame.parts.amd_smu.cards {
            if !matches!(
                card.decoded,
                AmdSmuStatus::Decoded(Decoded::Full | Decoded::Truncated)
            ) {
                continue;
            }
            let metrics = &card.metrics;
            self.socket_w = self.socket_w.or_else(|| metric_watts(metrics.power_socket));
            self.apu_w = self.apu_w.or_else(|| metric_watts(metrics.power_apu));
            self.gfx_w = self.gfx_w.or_else(|| metric_watts(metrics.power_gfx));
            self.core_w = self.core_w.or_else(|| metric_watts(metrics.power_all_core));
            self.system_w = self.system_w.or_else(|| metric_watts(metrics.power_sys));
        }
        self.socket_w = self.socket_w.or_else(|| {
            self.zones
                .iter()
                .find(|zone| zone.name.contains("package"))
                .and_then(|zone| zone.watts)
        });
        self.session.update(
            frame.at_ms,
            ac_state,
            self.socket_w.or_else(|| self.battery_w.map(f32::abs)),
        );
        if let Some(watts) = self.socket_w.or(self.battery_w.map(f32::abs)) {
            if self.history.capacity() == 0 {
                self.history.reserve_exact(HISTORY_CAP);
            }
            if watts > self.history_scale_w {
                let previous_scale = self.history_scale_w;
                if previous_scale > 0.0 {
                    for sample in &mut self.history {
                        *sample *= previous_scale / watts;
                    }
                }
                self.history_scale_w = watts;
            }
            if self.history.len() == HISTORY_CAP {
                self.history.remove(0);
            }
            self.history
                .push(watts / self.history_scale_w.max(f32::EPSILON));
        }
    }
    #[cfg(test)]
    pub(in crate::monitor) fn history_capacity(&self) -> usize {
        self.history.capacity()
    }
}
fn metric_watts(value: Option<u32>) -> Option<f32> {
    value
        .filter(|value| {
            *value != u32::MAX
                && *value != u32::from(u16::MAX)
                && *value != u32::from(u16::MAX) * 1000
        })
        .map(|value| value as f32 / 1000.0)
}
fn fill_battery(row: &mut BatteryRow, supply: &SupplyReading) {
    row.name.clear();
    row.name.push_str(&supply.name);
    row.status = supply.status;
    row.capacity = supply.capacity_pct;
    row.now_wh = supply.energy_now_uwh.map(|v| v as f64 / 1_000_000.0);
    row.full_wh = supply.energy_full_uwh.map(|v| v as f64 / 1_000_000.0);
    row.design_wh = supply
        .energy_full_design_uwh
        .map(|v| v as f64 / 1_000_000.0);
    row.power_w = supply
        .power_now_uw
        .map(|v| v as f32 / 1_000_000.0)
        .or_else(|| {
            supply
                .current_now_ua
                .zip(supply.voltage_now_uv)
                .map(|(a, v)| (a.unsigned_abs() as f64 * v as f64 / 1_000_000_000_000.0) as f32)
        });
    row.cycles = supply.cycle_count;
}

pub(in crate::monitor) struct PowerView<'a>(pub &'a PowerModel);
impl TabView for PowerView<'_> {
    fn specs(&self) -> &'static [PanelSpec] {
        &SPECS
    }
    fn template(&self) -> Node {
        Node::Col(vec![
            (
                Size::Weight(3),
                Node::Row(vec![
                    (Size::Weight(2), Node::Panel(RAILS)),
                    (Size::Weight(1), Node::Panel(BATTERY)),
                ]),
            ),
            (Size::Weight(1), Node::Panel(POLICY)),
        ])
    }
    fn demand(&self, panel: PanelId) -> CollectorSet {
        let mut set = CollectorSet::empty();
        match panel {
            BATTERY => {
                set.insert(CollectorId::PowerSupply);
            }
            RAILS => {
                set.insert(CollectorId::Powercap);
                set.insert(CollectorId::AmdSmu);
                set.insert(CollectorId::PowerSupply);
            }
            POLICY => {
                set.insert(CollectorId::CpuFreq);
            }
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
        let mut grid = CellGrid::new(rect.cols, rect.rows);
        let title = match placed.id {
            BATTERY => "Battery",
            RAILS => "Power rails",
            POLICY => "Power policy",
            _ => return,
        };
        let inner = panel::panel(&mut grid, Rect::full(rect.cols, rect.rows), title, None).rect();
        match placed.id {
            BATTERY => paint::paint_battery(&mut grid, inner, &self.0),
            RAILS => paint::paint_rails(&mut grid, inner, &self.0),
            POLICY => paint::paint_policy(&mut grid, inner, &self.0),
            _ => {}
        }
        Emitter::default().emit(
            &grid,
            Rect::full(rect.cols, rect.rows),
            surface,
            rect.x,
            rect.y,
        );
        if placed.id == RAILS && !self.0.history.is_empty() && inner.rows > 2 {
            graphs::paint_history_graph(
                &self.0.history,
                RampId::Power,
                Rect {
                    x: rect.x + inner.x,
                    y: rect.y + inner.y,
                    cols: inner.cols,
                    rows: inner.rows.saturating_sub(11),
                },
                surface,
            );
        }
    }
}
#[cfg(test)]
#[path = "power_view_tests.rs"]
mod tests;
