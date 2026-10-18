use crate::telemetry::{Metric, ScaleMode, SeriesBuf, SeriesReq, TelemetryHistory, Window};
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
    health::Health,
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
    stapm_limit_w: Option<f32>,
    pub(super) state: Option<EffectiveState>,
    pub(super) eta_hours: Option<f64>,
    pub(super) session: SessionEnergy,
    ewma: Ewma,
    rail_history: [Vec<f32>; 5],
    rail_current: [Option<f32>; 5],
    telemetry_battery_history: Vec<f32>,
    telemetry_battery_power_history: Vec<f32>,
    telemetry_series: PowerSeriesBuf,
    telemetry_draw_peak_w: Option<f32>,
    pub(super) power_supply_health: Health,
    pub(super) powercap_health: Health,
    pub(super) amd_smu_health: Health,
    pub(super) cpu_freq_health: Health,
    pub(super) session_started: bool,
    session_previous: Option<(u64, f32)>,
    session_ac: Option<bool>,
}
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct BatteryRow {
    pub(super) name: String,
    pub(super) state: Option<EffectiveState>,
    pub(super) status: Option<SupplyStatus>,
    pub(super) capacity: Option<u8>,
    pub(super) now_wh: Option<f64>,
    pub(super) full_wh: Option<f64>,
    pub(super) design_wh: Option<f64>,
    pub(super) power_w: Option<f32>,
    pub(super) cycles: Option<u32>,
    charge_history: Vec<f32>,
    energy_history: Vec<f32>,
}
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct AdapterRow {
    pub(super) name: String,
    pub(super) online: Option<bool>,
}
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct ZoneRow {
    pub(super) name: String,
    pub(super) domain: Option<thegn_metrics::collect::model::powercap::PowerDomain>,
    pub(super) watts: Option<f32>,
    history: Vec<f32>,
    active: bool,
}

#[derive(Debug, Default, Clone)]
struct PowerSeriesBuf(SeriesBuf);
impl PartialEq for PowerSeriesBuf {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

impl PowerModel {
    pub(in crate::monitor) fn absorb_frame(&mut self, frame: &MonitorFrame) {
        self.power_supply_health = frame.health[CollectorId::PowerSupply.index()];
        self.powercap_health = frame.health[CollectorId::Powercap.index()];
        self.amd_smu_health = frame.health[CollectorId::AmdSmu.index()];
        self.cpu_freq_health = frame.health[CollectorId::CpuFreq.index()];
        let mut adapter_count = 0;
        for supply in &frame.parts.power_supply.supplies {
            match supply.kind {
                SupplyKind::Battery if !supply.device_scope => {
                    if self.batteries.capacity() == 0 {
                        self.batteries.reserve_exact(INSTANCE_CAP);
                    }
                    let index = self
                        .batteries
                        .iter()
                        .position(|battery| battery.name == supply.name.as_ref());
                    let index = match index {
                        Some(index) => index,
                        None if self.batteries.len() < INSTANCE_CAP => {
                            self.batteries.push(BatteryRow::default());
                            self.batteries.len() - 1
                        }
                        None => continue,
                    };
                    fill_battery(&mut self.batteries[index], supply);
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
        self.batteries.retain(|battery| {
            frame.parts.power_supply.supplies.iter().any(|supply| {
                supply.kind == SupplyKind::Battery
                    && !supply.device_scope
                    && battery.name == supply.name.as_ref()
            })
        });
        self.adapters.truncate(adapter_count);
        let ac_online = self
            .adapters
            .iter()
            .any(|adapter| adapter.online == Some(true));
        let ac_state = (!self.adapters.is_empty()).then_some(ac_online);
        for battery in &mut self.batteries {
            battery.state =
                power::effective_state(battery.status, ac_state, battery.capacity, None);
        }
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
        for row in &mut self.zones {
            row.active = false;
        }
        for zone in frame.parts.powercap.zones.iter().take(INSTANCE_CAP) {
            if let Some(index) = self
                .zones
                .iter()
                .position(|row| row.name == zone.name.as_ref() && row.domain == Some(zone.domain))
            {
                let row = &mut self.zones[index];
                row.active = true;
                row.watts = zone.watts.filter(|value| value.is_finite());
                if let Some(watts) = row.watts {
                    push_history(&mut row.history, watts);
                }
            }
        }
        for zone in frame.parts.powercap.zones.iter().take(INSTANCE_CAP) {
            if self.zones.iter().any(|row| {
                row.active && row.name == zone.name.as_ref() && row.domain == Some(zone.domain)
            }) {
                continue;
            }
            if self.zones.capacity() == 0 {
                self.zones.reserve_exact(INSTANCE_CAP);
            }
            let index = if self.zones.len() < INSTANCE_CAP {
                self.zones.push(ZoneRow::default());
                self.zones.len() - 1
            } else if let Some(index) = self.zones.iter().position(|row| !row.active) {
                index
            } else {
                continue;
            };
            let row = &mut self.zones[index];
            row.history.clear();
            row.name.clear();
            row.name.push_str(&zone.name);
            row.domain = Some(zone.domain);
            row.active = true;
            row.watts = zone.watts.filter(|value| value.is_finite());
            if row.history.capacity() == 0 {
                row.history.reserve_exact(HISTORY_CAP);
            }
            if let Some(watts) = row.watts {
                push_history(&mut row.history, watts);
            }
        }
        self.socket_w = None;
        self.apu_w = None;
        self.gfx_w = None;
        self.core_w = None;
        self.system_w = None;
        self.stapm_limit_w = None;
        let mut decoded_smu = false;
        for card in &frame.parts.amd_smu.cards {
            if !matches!(
                card.decoded,
                AmdSmuStatus::Decoded(Decoded::Full | Decoded::Truncated)
            ) {
                continue;
            }
            decoded_smu = true;
            let metrics = &card.metrics;
            self.socket_w = self.socket_w.or_else(|| metric_watts(metrics.power_socket));
            self.apu_w = self.apu_w.or_else(|| metric_watts(metrics.power_apu));
            self.gfx_w = self.gfx_w.or_else(|| metric_watts(metrics.power_gfx));
            self.core_w = self.core_w.or_else(|| metric_watts(metrics.power_all_core));
            self.system_w = self.system_w.or_else(|| metric_watts(metrics.power_sys));
            self.stapm_limit_w = self.stapm_limit_w.or_else(|| {
                metrics
                    .power_current_stapm_limit
                    .filter(|watts| *watts > 0 && *watts != u32::MAX)
                    .map(|watts| watts as f32)
            });
        }
        self.rail_current = [
            self.socket_w,
            self.apu_w,
            self.core_w,
            self.gfx_w,
            self.system_w,
        ];
        if !decoded_smu {
            for history in &mut self.rail_history {
                history.clear();
            }
        }
        for (slot, value) in self.rail_history.iter_mut().zip(self.rail_current) {
            if let Some(watts) = value {
                push_history(slot, watts);
            }
        }
        self.socket_w = self.socket_w.or_else(|| {
            self.zones
                .iter()
                .find(|zone| {
                    zone.active
                        && zone.domain
                            == Some(thegn_metrics::collect::model::powercap::PowerDomain::Package)
                })
                .and_then(|zone| zone.watts)
        });
        let session_watts = self.socket_w.or_else(|| self.battery_w.map(f32::abs));
        if self.session_previous.is_some_and(|(previous_ms, _)| {
            let elapsed = frame.at_ms.saturating_sub(previous_ms);
            session_watts.is_some() && elapsed > 0 && elapsed <= 30_000
        }) {
            self.session_started = true;
        }
        if self.session_ac == Some(true) && ac_state == Some(false) {
            self.session_started = false;
        }
        self.session.update(frame.at_ms, ac_state, session_watts);
        self.session_previous = session_watts.map(|watts| (frame.at_ms, watts));
        self.session_ac = ac_state;
    }
    pub(in crate::monitor) fn absorb_history(
        &mut self,
        history: &TelemetryHistory,
        window: Window,
        scale: ScaleMode,
        now_ms: u64,
    ) {
        for (metric, slot) in [
            (Metric::Battery, &mut self.telemetry_battery_history),
            (
                Metric::BatteryPower,
                &mut self.telemetry_battery_power_history,
            ),
        ] {
            history.series_into(
                &SeriesReq {
                    metric,
                    window,
                    scale: TelemetryHistory::scale_for(metric, scale),
                    buckets: 240,
                    agg: thegn_core::series::Agg::Max,
                    now_ms,
                },
                &mut self.telemetry_series.0,
            );
            let series = &self.telemetry_series.0;
            if series.empty() {
                slot.clear();
                if metric == Metric::BatteryPower {
                    self.telemetry_draw_peak_w = None;
                }
            } else {
                let first_sample = series.gap().iter().position(|gap| !gap);
                let draw_peak = if metric == Metric::BatteryPower {
                    let peak = series
                        .raw_hi()
                        .iter()
                        .zip(series.gap())
                        .filter(|(_, gap)| !**gap)
                        .map(|(value, _)| value.abs())
                        .filter(|value| value.is_finite())
                        .fold(0.0_f32, f32::max);
                    self.telemetry_draw_peak_w = (peak > 0.0).then_some(peak);
                    peak
                } else {
                    0.0
                };
                let mut held = f32::NAN;
                slot.clear();
                for (index, (raw, gap)) in series.raw_hi().iter().zip(series.gap()).enumerate() {
                    if first_sample.is_none_or(|first| index < first) {
                        slot.push(f32::NAN);
                        continue;
                    }
                    if !gap {
                        held = if raw.is_finite() { *raw } else { f32::NAN };
                    }
                    let value = if !held.is_finite() {
                        f32::NAN
                    } else if metric == Metric::Battery {
                        if (0.0..=100.0).contains(&held) {
                            held / 100.0
                        } else {
                            f32::NAN
                        }
                    } else if metric == Metric::BatteryPower {
                        if draw_peak > 0.0 {
                            held.abs() / draw_peak
                        } else {
                            0.0
                        }
                    } else {
                        series.hi()[index]
                    };
                    slot.push(value);
                }
            }
        }
    }
    #[cfg(test)]
    pub(in crate::monitor) fn history_capacity(&self) -> usize {
        self.rail_history[0].capacity()
    }
    #[cfg(test)]
    fn series_storage(&self) -> (usize, usize, usize) {
        (
            self.telemetry_series.0.hi().as_ptr() as usize,
            self.telemetry_series.0.raw_hi().as_ptr() as usize,
            self.telemetry_series.0.gap().as_ptr() as usize,
        )
    }
}
fn push_history(history: &mut Vec<f32>, value: f32) {
    if history.capacity() == 0 {
        history.reserve_exact(HISTORY_CAP);
    }
    if history.len() < HISTORY_CAP {
        history.push(value);
    } else {
        history.rotate_left(1);
        history[HISTORY_CAP - 1] = value;
    }
}
fn metric_watts(value: Option<u32>) -> Option<f32> {
    value
        .filter(|value| *value != u32::MAX)
        .map(|value| value as f32 / 1000.0)
}
fn rail_ceiling(model: &PowerModel) -> f32 {
    if let Some(limit) = model.stapm_limit_w {
        return limit;
    }
    let peak = rail_drawn_peak(model);
    (peak.max(30.0) / 10.0).ceil() * 10.0
}
fn rail_drawn_peak(model: &PowerModel) -> f32 {
    model
        .rail_history
        .iter()
        .zip(model.rail_current)
        .filter(|(_, reading)| reading.is_some())
        .flat_map(|(history, _)| history.iter().copied())
        .chain(
            model
                .zones
                .iter()
                .filter(|zone| zone.active)
                .flat_map(|zone| zone.history.iter().copied()),
        )
        .filter(|value| value.is_finite())
        .fold(0.0_f32, f32::max)
}
fn draw_graph_label(surface: &mut Surface, x: usize, y: usize, width: usize, label: &str) {
    let line = crate::seg::Line::segs(vec![crate::seg::seg(
        crate::seg::Tok::Slot(crate::chrome::S::Dim),
        label,
    )]);
    crate::seg::draw_line(
        surface,
        x,
        y,
        width,
        &line,
        crate::seg::Tok::Slot(crate::chrome::S::Dim),
    );
}
fn watts_label(name: &str, watts: Option<f32>) -> String {
    let value = watts
        .map(|watts| format!("{watts:.1} W"))
        .unwrap_or_else(|| crate::caps::active_glyphs().emdash.to_owned());
    format!("{name} {value}")
}
fn draw_history_label(model: &PowerModel) -> String {
    let peak = model
        .telemetry_draw_peak_w
        .map(|watts| format!("{watts:.1} W"))
        .unwrap_or_else(|| paint::mark(model.power_supply_health).to_owned());
    format!("battery draw peak {peak}")
}
fn rail_title(model: &PowerModel) -> String {
    let ceiling = rail_ceiling(model);
    let peak = rail_drawn_peak(model);
    match model.stapm_limit_w {
        Some(_) if peak > ceiling => {
            format!("Power rails ({ceiling:.0} W STAPM; peak {peak:.0} W)")
        }
        Some(_) => format!("Power rails ({ceiling:.0} W STAPM)"),
        None => format!("Power rails ({ceiling:.0} W max)"),
    }
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
    if let Some(charge) = row.capacity {
        push_history(&mut row.charge_history, charge as f32 / 100.0);
    }
    if let (Some(energy), Some(full)) = (row.now_wh, row.full_wh.or(row.design_wh))
        && full > 0.0
    {
        push_history(&mut row.energy_history, (energy / full) as f32);
    }
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
            BATTERY => "Battery".to_owned(),
            RAILS => rail_title(self.0),
            POLICY => "Power policy".to_owned(),
            _ => return,
        };
        let inner = panel::panel(&mut grid, Rect::full(rect.cols, rect.rows), &title, None).rect();
        match placed.id {
            BATTERY => paint::paint_battery(&mut grid, inner, self.0),
            RAILS => paint::paint_rails(&mut grid, inner, self.0),
            POLICY => paint::paint_policy(&mut grid, inner, self.0),
            _ => {}
        }
        Emitter::default().emit(
            &grid,
            Rect::full(rect.cols, rect.rows),
            surface,
            rect.x,
            rect.y,
        );
        if placed.id == BATTERY && inner.rows >= 14 {
            let graph_rows = (inner.rows.saturating_sub(5) / 2).min(6);
            let graph_y = rect.y + inner.y + inner.rows - graph_rows * 2;
            let graph_x = rect.x + inner.x;
            let charge_label = if self.0.telemetry_battery_history.is_empty() {
                format!("charge history {}", paint::mark(self.0.power_supply_health))
            } else {
                "telemetry charge / 100%".to_owned()
            };
            if self.0.batteries.is_empty() {
                let charge = Rect {
                    x: graph_x,
                    y: graph_y,
                    cols: inner.cols,
                    rows: graph_rows,
                };
                draw_graph_label(surface, graph_x, graph_y, inner.cols, &charge_label);
                let power = Rect {
                    y: graph_y + graph_rows,
                    ..charge
                };
                draw_graph_label(
                    surface,
                    graph_x,
                    graph_y + graph_rows,
                    inner.cols,
                    &draw_history_label(self.0),
                );
                let charge = Rect {
                    y: charge.y + 1,
                    rows: charge.rows.saturating_sub(1),
                    ..charge
                };
                let power = Rect {
                    y: power.y + 1,
                    rows: power.rows.saturating_sub(1),
                    ..power
                };
                let charge = crate::monitor::kit::history_rect::graph_rect(
                    charge,
                    self.0.telemetry_battery_history.len(),
                    graphs::table().braille,
                );
                let power = crate::monitor::kit::history_rect::graph_rect(
                    power,
                    self.0.telemetry_battery_power_history.len(),
                    graphs::table().braille,
                );
                graphs::paint_history_graph(
                    &self.0.telemetry_battery_history,
                    RampId::Power,
                    charge,
                    surface,
                );
                graphs::paint_history_graph(
                    &self.0.telemetry_battery_power_history,
                    RampId::Power,
                    power,
                    surface,
                );
            } else {
                let telemetry_cols = inner.cols * 2 / 3;
                let battery_x = graph_x + telemetry_cols;
                let battery_cols = inner.cols.saturating_sub(telemetry_cols);
                let charge = Rect {
                    x: graph_x,
                    y: graph_y,
                    cols: telemetry_cols,
                    rows: graph_rows,
                };
                let power = Rect {
                    y: graph_y + graph_rows,
                    ..charge
                };
                draw_graph_label(surface, graph_x, graph_y, telemetry_cols, &charge_label);
                draw_graph_label(
                    surface,
                    graph_x,
                    graph_y + graph_rows,
                    telemetry_cols,
                    &draw_history_label(self.0),
                );
                let charge = Rect {
                    y: charge.y + 1,
                    rows: charge.rows.saturating_sub(1),
                    ..charge
                };
                let power = Rect {
                    y: power.y + 1,
                    rows: power.rows.saturating_sub(1),
                    ..power
                };
                let charge = crate::monitor::kit::history_rect::graph_rect(
                    charge,
                    self.0.telemetry_battery_history.len(),
                    graphs::table().braille,
                );
                let power = crate::monitor::kit::history_rect::graph_rect(
                    power,
                    self.0.telemetry_battery_power_history.len(),
                    graphs::table().braille,
                );
                graphs::paint_history_graph(
                    &self.0.telemetry_battery_history,
                    RampId::Power,
                    charge,
                    surface,
                );
                graphs::paint_history_graph(
                    &self.0.telemetry_battery_power_history,
                    RampId::Power,
                    power,
                    surface,
                );
                let row_height = (graph_rows * 2) / self.0.batteries.len();
                for (index, battery) in self.0.batteries.iter().enumerate() {
                    if row_height < 2 {
                        break;
                    }
                    let battery_y = graph_y + index * row_height;
                    let charge = Rect {
                        x: battery_x,
                        y: battery_y,
                        cols: battery_cols,
                        rows: row_height / 2,
                    };
                    let energy = Rect {
                        y: battery_y + row_height / 2,
                        rows: row_height - row_height / 2,
                        ..charge
                    };
                    draw_graph_label(
                        surface,
                        battery_x,
                        battery_y,
                        battery_cols,
                        &format!("{} charge / 100%", battery.name),
                    );
                    draw_graph_label(
                        surface,
                        battery_x,
                        battery_y + row_height / 2,
                        battery_cols,
                        &format!("{} energy / capacity", battery.name),
                    );
                    let charge = Rect {
                        y: charge.y + 1,
                        rows: charge.rows.saturating_sub(1),
                        ..charge
                    };
                    let energy = Rect {
                        y: energy.y + 1,
                        rows: energy.rows.saturating_sub(1),
                        ..energy
                    };
                    let charge = crate::monitor::kit::history_rect::graph_rect(
                        charge,
                        battery.charge_history.len(),
                        graphs::table().braille,
                    );
                    let energy = crate::monitor::kit::history_rect::graph_rect(
                        energy,
                        battery.energy_history.len(),
                        graphs::table().braille,
                    );
                    graphs::paint_history_graph(
                        &battery.charge_history,
                        RampId::Power,
                        charge,
                        surface,
                    );
                    graphs::paint_history_graph(
                        &battery.energy_history,
                        RampId::Power,
                        energy,
                        surface,
                    );
                }
            }
        }
        if placed.id == RAILS && inner.rows > 2 {
            let rail_samples = self
                .0
                .rail_history
                .iter()
                .map(Vec::len)
                .chain(
                    self.0
                        .zones
                        .iter()
                        .filter(|zone| zone.active)
                        .map(|zone| zone.history.len()),
                )
                .max()
                .unwrap_or(0);
            let graph_rect = crate::monitor::kit::history_rect::graph_rect(
                Rect {
                    x: rect.x + inner.x,
                    y: rect.y + inner.y,
                    cols: inner.cols,
                    rows: inner.rows.saturating_sub(11),
                },
                rail_samples,
                graphs::table().braille,
            );
            let band_count = self
                .0
                .rail_history
                .iter()
                .enumerate()
                .filter(|(index, history)| {
                    self.0.rail_current[*index].is_some() && !history.is_empty()
                })
                .count()
                + self
                    .0
                    .zones
                    .iter()
                    .filter(|zone| zone.active && !zone.history.is_empty())
                    .count();
            if band_count > 0 {
                let drawn_bands = band_count.min(graph_rect.rows / 2);
                let plot_rows = graph_rect.rows / drawn_bands;
                let ceiling = rail_ceiling(self.0);
                let mut band = 0;
                for (index, history) in self.0.rail_history.iter().enumerate() {
                    if history.is_empty()
                        || self.0.rail_current[index].is_none()
                        || band >= drawn_bands
                    {
                        continue;
                    }
                    let labels = ["socket", "APU", "core", "gfx", "system"];
                    let values = [
                        self.0.rail_current[0],
                        self.0.rail_current[1],
                        self.0.rail_current[2],
                        self.0.rail_current[3],
                        self.0.rail_current[4],
                    ];
                    let band_y = graph_rect.y + band * plot_rows;
                    let mut normalized = [0.0_f32; HISTORY_CAP];
                    normalized[..history.len()].copy_from_slice(history);
                    for value in normalized.iter_mut().take(history.len()) {
                        *value /= ceiling;
                    }
                    draw_graph_label(
                        surface,
                        graph_rect.x,
                        band_y,
                        graph_rect.cols,
                        &watts_label(labels[index], values[index]),
                    );
                    let plot = crate::monitor::kit::history_rect::graph_rect(
                        Rect {
                            y: band_y + 1,
                            rows: plot_rows.saturating_sub(1),
                            ..graph_rect
                        },
                        history.len(),
                        graphs::table().braille,
                    );
                    graphs::paint_history_graph(
                        &normalized[..history.len()],
                        RampId::Power,
                        plot,
                        surface,
                    );
                    band += 1;
                }
                for zone in self
                    .0
                    .zones
                    .iter()
                    .filter(|zone| zone.active && !zone.history.is_empty())
                {
                    if band >= drawn_bands {
                        break;
                    }
                    let band_y = graph_rect.y + band * plot_rows;
                    let mut normalized = [0.0_f32; HISTORY_CAP];
                    normalized[..zone.history.len()].copy_from_slice(&zone.history);
                    for value in normalized.iter_mut().take(zone.history.len()) {
                        *value /= ceiling;
                    }
                    draw_graph_label(
                        surface,
                        graph_rect.x,
                        band_y,
                        graph_rect.cols,
                        &watts_label(&zone.name, zone.watts),
                    );
                    let plot = crate::monitor::kit::history_rect::graph_rect(
                        Rect {
                            y: band_y + 1,
                            rows: plot_rows.saturating_sub(1),
                            ..graph_rect
                        },
                        zone.history.len(),
                        graphs::table().braille,
                    );
                    graphs::paint_history_graph(
                        &normalized[..zone.history.len()],
                        RampId::Power,
                        plot,
                        surface,
                    );
                    band += 1;
                }
            }
        }
    }
}
#[cfg(test)]
#[path = "power_view_tests.rs"]
mod tests;
