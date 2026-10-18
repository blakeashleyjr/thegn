use crate::{
    compositor::Rect,
    monitor::{
        board::TabView,
        kit::{emit::Emitter, grid::CellGrid, panel},
        layout::{Node, PanelId, PanelSpec, Placed, Size, Variant, VariantKind},
    },
    telemetry::{Agg, Metric, ScaleMode, SeriesReq, TelemetryHistory, Window},
};
use termwiz::surface::Surface;
use thegn_metrics::collect::{
    CollectorId, CollectorSet,
    frame::MonitorFrame,
    model::{
        amd_smu::AmdSmuStatus,
        hwmon::{ChipGroup, SensorKind, SensorRole},
        thermal_zone::ThermalTrip,
    },
    parse::amd_gpu_metrics::Decoded,
    policy::{hwmon_group, thermal},
};

const SENSOR_CAP: usize = 256;
const HISTORY_CAP: usize = 120;
const PACKAGE: PanelId = PanelId("thermal.package");
const CORES: PanelId = PanelId("thermal.cores");
const SENSORS: PanelId = PanelId("thermal.sensors");
const ZONES: PanelId = PanelId("thermal.zones");
const GPU: PanelId = PanelId("thermal.gpu");
const HEATMAP: PanelId = PanelId("thermal.heatmap");

const FULL: Variant = Variant {
    kind: VariantKind::Full,
    min: (24, 12),
    pref: (44, 16),
};
const COMPACT: Variant = Variant {
    kind: VariantKind::Compact,
    min: (16, 11),
    pref: (24, 12),
};
const MINI: Variant = Variant {
    kind: VariantKind::Mini,
    min: (16, 11),
    pref: (16, 11),
};
const VARIANTS: &[Variant] = &[FULL, COMPACT, MINI];
const SPECS: [PanelSpec; 6] = [
    PanelSpec {
        id: PACKAGE,
        priority: 0,
        variants: VARIANTS,
    },
    PanelSpec {
        id: CORES,
        priority: 2,
        variants: VARIANTS,
    },
    PanelSpec {
        id: SENSORS,
        priority: 1,
        variants: VARIANTS,
    },
    PanelSpec {
        id: ZONES,
        priority: 4,
        variants: VARIANTS,
    },
    PanelSpec {
        id: GPU,
        priority: 3,
        variants: VARIANTS,
    },
    PanelSpec {
        id: HEATMAP,
        priority: 3,
        variants: VARIANTS,
    },
];

#[derive(Debug, Clone, PartialEq)]
pub(in crate::monitor) struct ThermalModel {
    sensors: Vec<SensorRow>,
    zones: Vec<ZoneRow>,
    cores: [Option<f32>; 16],
    gfx_c: Option<f32>,
    edge_c: Option<f32>,
    hotspot_c: Option<f32>,
    soc_c: Option<f32>,
    socket_w: Option<f32>,
    package_history: [f32; HISTORY_CAP],
    history_len: usize,
    history_next: usize,
}

impl Default for ThermalModel {
    fn default() -> Self {
        Self {
            sensors: Vec::with_capacity(SENSOR_CAP),
            zones: Vec::with_capacity(16),
            cores: [None; 16],
            gfx_c: None,
            edge_c: None,
            hotspot_c: None,
            soc_c: None,
            socket_w: None,
            package_history: [0.0; HISTORY_CAP],
            history_len: 0,
            history_next: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct SensorRow {
    key: String,
    chip: String,
    label: String,
    group: ChipGroup,
    temp_c: Option<f32>,
    max_c: Option<f32>,
    crit_c: Option<f32>,
    history: [f32; HISTORY_CAP],
    history_len: usize,
    history_next: usize,
    seen: bool,
}

impl Default for SensorRow {
    fn default() -> Self {
        Self {
            key: String::new(),
            chip: String::new(),
            label: String::new(),
            group: ChipGroup::Other,
            temp_c: None,
            max_c: None,
            crit_c: None,
            history: [0.0; HISTORY_CAP],
            history_len: 0,
            history_next: 0,
            seen: false,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
struct ZoneRow {
    label: String,
    temp_c: Option<f32>,
    trips: Vec<ThermalTrip>,
    alarm_events: u64,
}

impl ThermalModel {
    pub(in crate::monitor) fn absorb_history(
        &mut self,
        history: &TelemetryHistory,
        window: Window,
        scale: ScaleMode,
        now_ms: u64,
    ) {
        let series = history.series(&SeriesReq {
            metric: Metric::Temp,
            window,
            scale: TelemetryHistory::scale_for(Metric::Temp, scale),
            buckets: HISTORY_CAP,
            agg: Agg::Max,
            now_ms,
        });
        if series.empty {
            return;
        }
        self.package_history.fill(0.0);
        self.history_len = series.hi.len().min(HISTORY_CAP);
        self.history_next = 0;
        for index in 0..self.history_len {
            self.package_history[index] = if series.gap[index] {
                f32::NAN
            } else {
                series.raw_hi[index]
            };
        }
    }

    pub(in crate::monitor) fn absorb_frame(&mut self, frame: &MonitorFrame) {
        let mut zone_count = 0;
        for sensor in &mut self.sensors {
            sensor.seen = false;
        }
        self.cores.fill(None);
        self.gfx_c = None;
        self.edge_c = None;
        self.hotspot_c = None;
        self.soc_c = None;
        self.socket_w = None;

        let layout = frame.parts.hwmon.layout.as_deref();
        if let Some(layout) = layout {
            for (index, sensor) in layout.sensors.iter().enumerate() {
                if sensor.kind != SensorKind::Temp || sensor.role != SensorRole::Input {
                    continue;
                }
                let Some(chip) = layout.chips.get(usize::from(sensor.chip)) else {
                    continue;
                };
                let value = frame
                    .parts
                    .hwmon
                    .values
                    .get(index)
                    .map(|reading| reading.value);
                if value.is_some_and(|reading| {
                    reading.is_some_and(|value| {
                        hwmon_group::plausible_input(SensorKind::Temp, (value * 1000.0) as i64)
                            .is_none()
                    })
                }) {
                    continue;
                }
                let temp_c = value.flatten();
                let row_index = self.sensors.iter().position(|row| row.key == sensor.key);
                if row_index.is_none() && self.sensors.len() == SENSOR_CAP {
                    continue;
                }
                let row = if let Some(index) = row_index {
                    &mut self.sensors[index]
                } else {
                    self.sensors.push(SensorRow::default());
                    self.sensors
                        .last_mut()
                        .expect("sensor row was just inserted")
                };
                row.key.clone_from(&sensor.key);
                row.chip.clone_from(&chip.name);
                row.group = chip.group;
                row.label.clear();
                row.label.push_str(
                    sensor
                        .label
                        .as_deref()
                        .filter(|label| !label.is_empty())
                        .unwrap_or("temperature"),
                );
                row.temp_c = temp_c;
                row.max_c = sensor.thresholds.max;
                row.crit_c = sensor.thresholds.crit;
                row.seen = true;
                if let Some(temp) = temp_c {
                    row.history[row.history_next] = temp;
                    row.history_next = (row.history_next + 1) % HISTORY_CAP;
                    row.history_len = row.history_len.saturating_add(1).min(HISTORY_CAP);
                }
            }
        }
        self.sensors.retain(|sensor| sensor.seen);
        self.sensors.sort_unstable_by(|left, right| {
            left.chip
                .cmp(&right.chip)
                .then_with(|| {
                    right
                        .temp_c
                        .partial_cmp(&left.temp_c)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .then_with(|| left.label.cmp(&right.label))
        });

        for zone in &frame.parts.thermal_zone.zones {
            if zone.alias
                || zone
                    .temp_c
                    .is_some_and(|temp| !thermal::plausible_range(temp))
            {
                continue;
            }
            let row = self.zone_slot(zone_count);
            zone_count += 1;
            row.label.clone_from(&zone.zone_type);
            row.temp_c = zone.temp_c;
            row.trips.clone_from(&zone.trips);
            row.alarm_events = zone.alarm_events;
        }
        self.zones.truncate(zone_count);

        if let Some(temp) = self.package().and_then(|sensor| sensor.temp_c) {
            self.package_history[self.history_next] = temp;
            self.history_next = (self.history_next + 1) % HISTORY_CAP;
            self.history_len = self.history_len.saturating_add(1).min(HISTORY_CAP);
        }

        for card in &frame.parts.amd_smu.cards {
            if !matches!(
                card.decoded,
                AmdSmuStatus::Decoded(Decoded::Full | Decoded::Truncated)
            ) {
                continue;
            }
            let metrics = &card.metrics;
            let card_cores = metrics.temperature_core.map(|source| {
                source
                    .map(|value| f32::from(value) / 100.0)
                    .filter(|value| thermal::plausible_range(*value))
            });
            if card_cores.iter().any(Option::is_some) && !self.cores.iter().any(Option::is_some) {
                self.cores = card_cores;
            }
            let gfx_c = metrics
                .temperature_gfx
                .map(|value| f32::from(value) / 100.0)
                .filter(|value| thermal::plausible_range(*value));
            let edge_c = metrics
                .temperature_edge
                .map(|value| f32::from(value) / 100.0)
                .filter(|value| thermal::plausible_range(*value));
            let hotspot_c = metrics
                .temperature_hotspot
                .map(|value| f32::from(value) / 100.0)
                .filter(|value| thermal::plausible_range(*value));
            let soc_c = metrics
                .temperature_soc
                .map(|value| f32::from(value) / 100.0)
                .filter(|value| thermal::plausible_range(*value));
            if self.gfx_c.is_none() {
                self.gfx_c = gfx_c;
            }
            if self.edge_c.is_none() {
                self.edge_c = edge_c;
            }
            if self.hotspot_c.is_none() {
                self.hotspot_c = hotspot_c;
            }
            if self.soc_c.is_none() {
                self.soc_c = soc_c;
            }
            if self.socket_w.is_none() {
                self.socket_w = metrics.power_socket.map(|value| value as f32 / 1000.0);
            }
        }
    }

    fn zone_slot(&mut self, index: usize) -> &mut ZoneRow {
        if index == self.zones.len() {
            self.zones.reserve(16);
            self.zones.push(ZoneRow::default());
        }
        &mut self.zones[index]
    }

    fn package(&self) -> Option<&SensorRow> {
        self.sensors
            .iter()
            .filter(|row| row.group == ChipGroup::Cpu)
            .max_by(|left, right| {
                left.temp_c
                    .partial_cmp(&right.temp_c)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
    }
}

mod paint;

pub(in crate::monitor) struct ThermalView<'a>(pub &'a ThermalModel);

impl TabView for ThermalView<'_> {
    fn specs(&self) -> &'static [PanelSpec] {
        &SPECS
    }

    fn template(&self) -> Node {
        Node::Col(vec![
            (
                Size::Weight(1),
                Node::Row(vec![
                    (Size::Weight(2), Node::Panel(PACKAGE)),
                    (Size::Weight(1), Node::Panel(CORES)),
                    (Size::Weight(1), Node::Panel(GPU)),
                ]),
            ),
            (Size::Weight(3), Node::Panel(SENSORS)),
            (Size::Fixed(11), Node::Panel(ZONES)),
            (Size::Weight(2), Node::Panel(HEATMAP)),
        ])
    }

    fn demand(&self, panel: PanelId) -> CollectorSet {
        let mut set = CollectorSet::empty();
        match panel {
            PACKAGE | SENSORS => {
                set.insert(CollectorId::Hwmon);
            }
            CORES => {
                set.insert(CollectorId::AmdSmu);
            }
            ZONES => {
                set.insert(CollectorId::ThermalZone);
            }
            HEATMAP => {
                set.insert(CollectorId::Hwmon);
            }
            GPU => {
                set.insert(CollectorId::Hwmon);
                set.insert(CollectorId::AmdSmu);
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
        _sections: &[crate::sections::Section],
    ) {
        let mut grid = CellGrid::new(rect.cols, rect.rows);
        let title = match placed.id {
            PACKAGE => "CPU package",
            CORES => "CPU cores",
            SENSORS => "Sensors",
            ZONES => "Thermal zones",
            GPU => "GPU thermals",
            HEATMAP => "Sensor history",
            _ => return,
        };
        let inner = {
            let body = panel::panel(&mut grid, Rect::full(rect.cols, rect.rows), title, None);
            body.rect()
        };
        match placed.id {
            PACKAGE => paint::package(
                &mut grid,
                inner,
                self.0.package(),
                &self.0.package_history,
                self.0.history_len,
                self.0.history_next,
            ),
            CORES => paint::cores(&mut grid, inner, &self.0.cores),
            SENSORS => paint::sensors(&mut grid, inner, &self.0.sensors),
            ZONES => paint::zones(&mut grid, inner, &self.0.zones),
            GPU => paint::gpu(&mut grid, inner, self.0),
            HEATMAP => paint::heatmap(&mut grid, inner, &self.0.sensors),
            _ => {}
        }
        Emitter::default().emit(
            &grid,
            Rect::full(rect.cols, rect.rows),
            surface,
            rect.x,
            rect.y,
        );
        if placed.id == SENSORS {
            let spark_x = rect.x.saturating_add(inner.x).saturating_add(71);
            let spark_width = inner.cols.saturating_sub(71);
            for (index, sensor) in self
                .0
                .sensors
                .iter()
                .enumerate()
                .take(inner.rows.saturating_sub(1))
            {
                let mut ordered = paint::history_width(sensor, spark_width);
                let limit = sensor.crit_c.or(sensor.max_c).unwrap_or(100.0);
                for sample in &mut ordered {
                    *sample = (*sample / limit).clamp(0.0, 1.0);
                }
                crate::monitor::graphs::paint_sparkline(
                    &ordered,
                    spark_width,
                    thegn_core::theme::RampId::Temp,
                    Rect {
                        x: spark_x,
                        y: rect.y.saturating_add(inner.y).saturating_add(index + 1),
                        cols: spark_width,
                        rows: 1,
                    },
                    surface,
                );
            }
        }
    }
}

#[cfg(test)]
#[path = "thermal_view_tests.rs"]
mod tests;
