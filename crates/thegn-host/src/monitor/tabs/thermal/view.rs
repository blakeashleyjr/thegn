use crate::{
    compositor::Rect,
    monitor::{
        board::TabView,
        kit::{emit::Emitter, grid::CellGrid, panel},
        layout::{Node, PanelId, PanelSpec, Placed, Size, Variant, VariantKind},
    },
    telemetry::{Metric, ScaleMode, SeriesBuf, SeriesReq, TelemetryHistory, Window},
};
use std::fmt::Write as _;
use termwiz::surface::Surface;
use thegn_core::series::Agg;
use thegn_metrics::collect::{
    CollectorId, CollectorSet,
    frame::MonitorFrame,
    model::{
        amd_smu::AmdSmuStatus,
        hwmon::{ChipGroup, SensorKind, SensorRole},
        thermal_zone::{CoolingAggregate, ThermalTrip},
    },
    parse::amd_gpu_metrics::Decoded,
    policy::{hwmon_group, thermal},
};

const SENSOR_CAP: usize = 256;
const FAN_CAP: usize = 16;
const HISTORY_CAP: usize = 120;
const PAIR_EVERY: usize = 30;
const PACKAGE: PanelId = PanelId("thermal.package");
const CORES: PanelId = PanelId("thermal.cores");
const SENSORS: PanelId = PanelId("thermal.sensors");
const ZONES: PanelId = PanelId("thermal.zones");
const GPU: PanelId = PanelId("thermal.gpu");
const HEATMAP: PanelId = PanelId("thermal.heatmap");
const COOLING: PanelId = PanelId("thermal.cooling");

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
const SPECS: [PanelSpec; 7] = [
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
        priority: 0,
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
    PanelSpec {
        id: COOLING,
        priority: 3,
        variants: VARIANTS,
    },
];

#[derive(Debug, Clone)]
pub(in crate::monitor) struct ThermalModel {
    sensors: Vec<SensorRow>,
    fans: Vec<FanRow>,
    zones: Vec<ZoneRow>,
    cooling: Vec<CoolingAggregate>,
    cooling_layout_generation: Option<u64>,
    cooling_panel_present: bool,
    pair_samples: usize,
    pair_fan_buf: Vec<f32>,
    pair_sensor_buf: Vec<f32>,
    cores: [Option<f32>; 16],
    core_layout_generation: Option<u64>,
    core_source_hwmon: bool,
    core_panel_present: bool,
    core_history: [[f32; HISTORY_CAP]; 16],
    core_history_len: [usize; 16],
    core_history_next: [usize; 16],
    gfx_c: Option<f32>,
    edge_c: Option<f32>,
    hotspot_c: Option<f32>,
    soc_c: Option<f32>,
    gpu_history: [f32; HISTORY_CAP],
    gpu_history_len: usize,
    gpu_history_next: usize,
    socket_w: Option<f32>,
    package_history: [f32; HISTORY_CAP],
    package_history_len: usize,
    package_history_next: usize,
    package_history_key: Option<String>,
    temp_history: Vec<f32>,
    history_buf: SeriesBuf,
}

impl Default for ThermalModel {
    fn default() -> Self {
        Self {
            sensors: Vec::with_capacity(SENSOR_CAP),
            fans: Vec::with_capacity(FAN_CAP),
            zones: Vec::with_capacity(16),
            cooling: Vec::with_capacity(16),
            cooling_layout_generation: None,
            cooling_panel_present: false,
            pair_samples: 0,
            pair_fan_buf: Vec::with_capacity(HISTORY_CAP),
            pair_sensor_buf: Vec::with_capacity(HISTORY_CAP),
            cores: [None; 16],
            core_layout_generation: None,
            core_source_hwmon: false,
            core_panel_present: false,
            core_history: [[0.0; HISTORY_CAP]; 16],
            core_history_len: [0; 16],
            core_history_next: [0; 16],
            gfx_c: None,
            edge_c: None,
            hotspot_c: None,
            soc_c: None,
            gpu_history: [0.0; HISTORY_CAP],
            gpu_history_len: 0,
            gpu_history_next: 0,
            socket_w: None,
            package_history: [0.0; HISTORY_CAP],
            package_history_len: 0,
            package_history_next: 0,
            package_history_key: None,
            temp_history: Vec::with_capacity(HISTORY_CAP),
            history_buf: SeriesBuf::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct FanRow {
    key: String,
    chip: String,
    label: String,
    rpm: Option<f32>,
    target: Option<f32>,
    peak: f32,
    history: [f32; HISTORY_CAP],
    history_len: usize,
    history_next: usize,
    pair: Option<usize>,
    seen: bool,
}

impl Default for FanRow {
    fn default() -> Self {
        Self {
            key: String::new(),
            chip: String::new(),
            label: String::new(),
            rpm: None,
            target: None,
            peak: 0.0,
            history: [0.0; HISTORY_CAP],
            history_len: 0,
            history_next: 0,
            pair: None,
            seen: false,
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
        history.series_into(
            &SeriesReq {
                metric: Metric::Temp,
                window,
                scale: TelemetryHistory::scale_for(Metric::Temp, scale),
                buckets: HISTORY_CAP,
                agg: Agg::Max,
                now_ms,
            },
            &mut self.history_buf,
        );
        self.temp_history.clear();
        if self.history_buf.empty() {
            return;
        }
        self.temp_history
            .extend_from_slice(self.history_buf.raw_hi());
    }

    pub(in crate::monitor) fn absorb_frame(&mut self, frame: &MonitorFrame) {
        let mut zone_count = 0;
        let mut smu_cores = None;
        for sensor in &mut self.sensors {
            sensor.seen = false;
        }
        for fan in &mut self.fans {
            fan.seen = false;
            fan.rpm = None;
            fan.target = None;
        }
        self.cores.fill(None);
        self.gfx_c = None;
        self.edge_c = None;
        self.hotspot_c = None;
        self.soc_c = None;
        self.socket_w = None;

        let layout = frame.parts.hwmon.layout.as_deref();
        if let Some(layout) = layout
            && self.core_layout_generation != Some(layout.generation)
        {
            let has_hwmon_cores = layout.sensors.iter().any(|sensor| {
                sensor.kind == SensorKind::Temp
                    && sensor.role == SensorRole::Input
                    && layout
                        .chips
                        .get(usize::from(sensor.chip))
                        .is_some_and(|chip| {
                            chip.group == ChipGroup::Cpu
                                && sensor.label.as_deref().is_some_and(is_core_label)
                        })
            });
            self.core_layout_generation = Some(layout.generation);
            self.core_source_hwmon = has_hwmon_cores;
            self.core_panel_present = has_hwmon_cores;
            self.core_history_len = [0; 16];
            self.core_history_next = [0; 16];
        }
        if let Some(layout) = layout
            && self.cooling_layout_generation != Some(layout.generation)
        {
            self.cooling_layout_generation = Some(layout.generation);
            self.cooling_panel_present = layout
                .sensors
                .iter()
                .any(|sensor| sensor.kind == SensorKind::Fan && sensor.role == SensorRole::Input);
        }
        if let Some(layout) = layout {
            for (index, sensor) in layout.sensors.iter().enumerate() {
                if sensor.kind == SensorKind::Fan && sensor.role == SensorRole::Input {
                    let Some(chip) = layout.chips.get(usize::from(sensor.chip)) else {
                        continue;
                    };
                    let row_index = self.fans.iter().position(|row| row.key == sensor.key);
                    if row_index.is_none() && self.fans.len() == FAN_CAP {
                        continue;
                    }
                    let row = if let Some(row_index) = row_index {
                        &mut self.fans[row_index]
                    } else {
                        self.fans.push(FanRow::default());
                        self.fans.last_mut().expect("fan row was just inserted")
                    };
                    row.seen = true;
                    row.key.clone_from(&sensor.key);
                    row.chip.clone_from(&chip.name);
                    row.label.clear();
                    row.label.push_str(
                        sensor
                            .label
                            .as_deref()
                            .filter(|label| !label.is_empty())
                            .unwrap_or(""),
                    );
                    if row.label.is_empty() {
                        row.label.push_str("fan");
                        write!(row.label, "{}", sensor.channel)
                            .expect("writing a fan channel to a String cannot fail");
                    }
                    let reading = frame
                        .parts
                        .hwmon
                        .values
                        .get(index)
                        .and_then(|reading| reading.value)
                        .filter(|rpm| rpm.is_finite() && (0.0..=30_000.0).contains(rpm));
                    row.rpm = reading;
                    if let Some(rpm) = reading {
                        row.peak = row.peak.max(rpm);
                        row.history[row.history_next] = rpm;
                        row.history_next = (row.history_next + 1) % HISTORY_CAP;
                        row.history_len = row.history_len.saturating_add(1).min(HISTORY_CAP);
                    } else if row.history_len > 0 {
                        row.history[row.history_next] = f32::NAN;
                        row.history_next = (row.history_next + 1) % HISTORY_CAP;
                        row.history_len = row.history_len.saturating_add(1).min(HISTORY_CAP);
                    }
                    continue;
                }
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
                row.seen = true;
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
                row.max_c = sensor.thresholds.max;
                row.crit_c = sensor.thresholds.crit;
                if value.is_some_and(|reading| {
                    reading.is_some_and(|value| {
                        hwmon_group::plausible_input(SensorKind::Temp, (value * 1000.0) as i64)
                            .is_none()
                    })
                }) {
                    row.temp_c = None;
                    row.history[row.history_next] = f32::NAN;
                    row.history_next = (row.history_next + 1) % HISTORY_CAP;
                    row.history_len = row.history_len.saturating_add(1).min(HISTORY_CAP);
                    continue;
                }
                let temp_c = value.flatten();
                row.temp_c = temp_c;
                if self.core_source_hwmon
                    && row.group == ChipGroup::Cpu
                    && let Some(core) = row
                        .label
                        .strip_prefix("core")
                        .or_else(|| row.label.strip_prefix("Core "))
                        .and_then(|suffix| suffix.parse::<usize>().ok())
                    && core < self.cores.len()
                {
                    self.cores[core] = temp_c;
                }
                if let Some(temp) = temp_c {
                    row.history[row.history_next] = temp;
                    row.history_next = (row.history_next + 1) % HISTORY_CAP;
                    row.history_len = row.history_len.saturating_add(1).min(HISTORY_CAP);
                }
            }
            for (target_index, target) in layout.sensors.iter().enumerate().filter(|(_, sensor)| {
                sensor.kind == SensorKind::Fan && sensor.role == SensorRole::Target
            }) {
                if let Some(input) = layout.sensors.iter().find(|input| {
                    input.kind == SensorKind::Fan
                        && input.role == SensorRole::Input
                        && input.chip == target.chip
                        && input.channel == target.channel
                }) && let Some(row) = self
                    .fans
                    .iter_mut()
                    .find(|row| row.seen && row.key == input.key)
                {
                    row.target = frame
                        .parts
                        .hwmon
                        .values
                        .get(target_index)
                        .and_then(|reading| reading.value)
                        .filter(|rpm| rpm.is_finite() && (0.0..=30_000.0).contains(rpm));
                }
            }
        }
        self.sensors.retain(|sensor| sensor.seen);
        self.sensors.sort_unstable_by(|left, right| {
            left.chip
                .cmp(&right.chip)
                .then_with(|| left.label.cmp(&right.label))
                .then_with(|| left.key.cmp(&right.key))
        });
        self.fans.retain(|fan| fan.seen);
        self.fans
            .sort_unstable_by(|left, right| left.key.cmp(&right.key));
        for (index, aggregate) in frame.parts.thermal_zone.cooling.iter().take(16).enumerate() {
            if index == self.cooling.len() {
                self.cooling.push(aggregate.clone());
            } else {
                let row = &mut self.cooling[index];
                row.kind.clone_from(&aggregate.kind);
                row.engaged = aggregate.engaged;
                row.total = aggregate.total;
            }
        }
        self.cooling
            .truncate(frame.parts.thermal_zone.cooling.len().min(16));
        self.pair_samples = self.pair_samples.saturating_add(1);
        if self.pair_samples.is_multiple_of(PAIR_EVERY) {
            self.recompute_fan_pairs();
        }

        let package_index = self.package_index();
        let package_temp = package_index.and_then(|index| self.sensors[index].temp_c);
        let package_key_changed = package_index.is_some_and(|index| {
            Some(self.sensors[index].key.as_str()) != self.package_history_key.as_deref()
        }) || (package_index.is_none()
            && self.package_history_key.is_some());
        if package_key_changed {
            self.package_history_len = 0;
            self.package_history_next = 0;
            match package_index {
                Some(index) => match &mut self.package_history_key {
                    Some(current) => current.clone_from(&self.sensors[index].key),
                    None => self.package_history_key = Some(self.sensors[index].key.clone()),
                },
                None => self.package_history_key = None,
            }
        }
        if let Some(temp) = package_temp {
            self.package_history[self.package_history_next] = temp;
            self.package_history_next = (self.package_history_next + 1) % HISTORY_CAP;
            self.package_history_len = self.package_history_len.saturating_add(1).min(HISTORY_CAP);
        }

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
            if card_cores.iter().any(Option::is_some) && smu_cores.is_none() {
                smu_cores = Some(card_cores);
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
        // temperature_core is present in AMD gpu_metrics format 2 content 0-2
        // and format 3 content 0; format 1 and other revisions are dGPU layouts.
        let smu_apu_layout = frame.parts.amd_smu.cards.iter().any(|card| {
            matches!(
                card.decoded,
                thegn_metrics::collect::model::amd_smu::AmdSmuStatus::Decoded(
                    thegn_metrics::collect::parse::amd_gpu_metrics::Decoded::Full
                        | thegn_metrics::collect::parse::amd_gpu_metrics::Decoded::Truncated
                )
            ) && card.metrics.header.is_some_and(|header| {
                matches!((header.format, header.content), (2, 0..=2) | (3, 0))
            })
        });
        self.core_panel_present = self.core_source_hwmon || smu_apu_layout;
        if !self.core_source_hwmon
            && let Some(cores) = smu_cores
        {
            self.cores = cores;
        }
        for (index, temp) in self.cores.iter().enumerate() {
            if temp.is_none() && self.core_history_len[index] == 0 {
                continue;
            }
            self.core_history[index][self.core_history_next[index]] = temp.unwrap_or(f32::NAN);
            self.core_history_next[index] = (self.core_history_next[index] + 1) % HISTORY_CAP;
            self.core_history_len[index] = self.core_history_len[index]
                .saturating_add(1)
                .min(HISTORY_CAP);
        }
        let gpu_temp = self.edge_c.or(self.gfx_c).or_else(|| {
            self.sensors
                .iter()
                .filter(|row| row.group == ChipGroup::Gpu)
                .filter_map(|row| row.temp_c)
                .max_by(f32::total_cmp)
        });
        if gpu_temp.is_some() || self.gpu_history_len > 0 {
            self.gpu_history[self.gpu_history_next] = gpu_temp.unwrap_or(f32::NAN);
            self.gpu_history_next = (self.gpu_history_next + 1) % HISTORY_CAP;
            self.gpu_history_len = self.gpu_history_len.saturating_add(1).min(HISTORY_CAP);
        }
    }

    fn recompute_fan_pairs(&mut self) {
        let mut ordered_sensors = [[f32::NAN; HISTORY_CAP]; SENSOR_CAP];
        let mut sensor_slices: [&[f32]; SENSOR_CAP] = [&[]; SENSOR_CAP];
        let sensor_count = self.sensors.len().min(SENSOR_CAP);
        // Sensor rings skip absent readings, so newest alignment is approximate;
        // the correlation is only a display hint.
        for (sensor_index, sensor) in self.sensors.iter().take(sensor_count).enumerate() {
            let count = sensor.history_len.min(HISTORY_CAP);
            let first = (sensor.history_next + HISTORY_CAP - count) % HISTORY_CAP;
            self.pair_sensor_buf.clear();
            self.pair_sensor_buf.reserve(count);
            for offset in 0..count {
                self.pair_sensor_buf
                    .push(sensor.history[(first + offset) % HISTORY_CAP]);
            }
            ordered_sensors[sensor_index][..count].copy_from_slice(&self.pair_sensor_buf);
            sensor_slices[sensor_index] = &ordered_sensors[sensor_index][..count];
        }
        for fan in &mut self.fans {
            let count = fan.history_len.min(HISTORY_CAP);
            let first = (fan.history_next + HISTORY_CAP - count) % HISTORY_CAP;
            self.pair_fan_buf.clear();
            self.pair_fan_buf.reserve(count);
            for offset in 0..count {
                self.pair_fan_buf
                    .push(fan.history[(first + offset) % HISTORY_CAP]);
            }
            fan.pair = thermal::pair_fan(
                &self.pair_fan_buf,
                &sensor_slices[..sensor_count],
                thermal::PAIR_MIN_SAMPLES,
            );
        }
    }

    fn zone_slot(&mut self, index: usize) -> &mut ZoneRow {
        if index == self.zones.len() {
            self.zones.reserve(16);
            self.zones.push(ZoneRow::default());
        }
        &mut self.zones[index]
    }

    #[cfg(test)]
    fn package(&self) -> Option<&SensorRow> {
        self.package_index().map(|index| &self.sensors[index])
    }

    fn package_index(&self) -> Option<usize> {
        self.sensors
            .iter()
            .enumerate()
            .filter(|(_, row)| row.group == ChipGroup::Cpu && is_package_label(&row.label))
            .min_by_key(|(_, row)| package_label_rank(&row.label))
            .map(|(index, _)| index)
            .or_else(|| {
                self.sensors
                    .iter()
                    .enumerate()
                    .filter(|(_, row)| row.group == ChipGroup::Cpu)
                    .max_by(|(_, left), (_, right)| {
                        left.temp_c
                            .partial_cmp(&right.temp_c)
                            .unwrap_or(std::cmp::Ordering::Equal)
                    })
                    .map(|(index, _)| index)
            })
    }
}

fn is_core_label(label: &str) -> bool {
    label
        .strip_prefix("core")
        .or_else(|| label.strip_prefix("Core "))
        .is_some_and(|suffix| suffix.parse::<usize>().is_ok())
}

fn package_label_rank(label: &str) -> usize {
    if label
        .get(.."package id ".len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("package id "))
    {
        0
    } else if label.eq_ignore_ascii_case("tctl") {
        1
    } else if label.eq_ignore_ascii_case("tdie") {
        2
    } else if label.eq_ignore_ascii_case("package") {
        3
    } else {
        usize::MAX
    }
}

fn is_package_label(label: &str) -> bool {
    package_label_rank(label) != usize::MAX
}

mod paint;

fn paint_history_runs(
    normalized: &[f32],
    count: usize,
    plot: Rect,
    braille: bool,
    surface: &mut Surface,
) {
    let count = count.min(normalized.len());
    let graph_rect = crate::monitor::kit::history_rect::graph_rect(plot, count, braille);
    let slots = if braille {
        graph_rect.cols.saturating_mul(2)
    } else {
        graph_rect.cols
    };
    let shown = count.min(slots);
    let skip = count.saturating_sub(shown);
    let pad = slots.saturating_sub(shown);
    let visible = &normalized[skip..count];
    let mut run_start = 0;
    while run_start < shown {
        while run_start < shown && !visible[run_start].is_finite() {
            run_start += 1;
        }
        let mut run_end = run_start;
        while run_end < shown && visible[run_end].is_finite() {
            run_end += 1;
        }
        if run_start < run_end {
            let slot_start = pad + run_start;
            let offset = if braille { slot_start % 2 } else { 0 };
            let start_col = slot_start / if braille { 2 } else { 1 };
            let run_cols = (offset + run_end - run_start).div_ceil(if braille { 2 } else { 1 });
            let x = graph_rect.x.saturating_add(start_col);
            let right = graph_rect.x.saturating_add(graph_rect.cols);
            let cols = run_cols.min(right.saturating_sub(x));
            let mut aligned = [f32::NAN; HISTORY_CAP + 1];
            let aligned_len = offset + run_end - run_start;
            aligned[offset..aligned_len].copy_from_slice(&visible[run_start..run_end]);
            crate::monitor::graphs::paint_history_graph(
                &aligned[..aligned_len],
                thegn_core::theme::RampId::Temp,
                Rect {
                    x,
                    y: graph_rect.y,
                    cols,
                    rows: graph_rect.rows,
                },
                surface,
            );
        }
        run_start = run_end.saturating_add(1);
    }
}

pub(in crate::monitor) struct ThermalView<'a>(pub &'a ThermalModel);

impl TabView for ThermalView<'_> {
    fn specs(&self) -> &'static [PanelSpec] {
        &SPECS
    }

    fn template(&self) -> Node {
        let mut top = vec![(Size::Weight(2), Node::Panel(PACKAGE))];
        if self.0.core_panel_present {
            top.push((Size::Weight(1), Node::Panel(CORES)));
        }
        top.push((Size::Weight(1), Node::Panel(GPU)));
        if self.0.cooling_panel_present {
            top.push((Size::Weight(1), Node::Panel(COOLING)));
        }
        Node::Col(vec![
            (Size::Weight(1), Node::Row(top)),
            (
                Size::Weight(3),
                Node::Row(vec![
                    (Size::Weight(3), Node::Panel(SENSORS)),
                    (Size::Weight(1), Node::Panel(ZONES)),
                ]),
            ),
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
                set.insert(CollectorId::Hwmon);
                set.insert(CollectorId::AmdSmu);
            }
            ZONES => {
                set.insert(CollectorId::ThermalZone);
            }
            HEATMAP => {
                set.insert(CollectorId::Hwmon);
                set.insert(CollectorId::AmdSmu);
            }
            GPU => {
                set.insert(CollectorId::Hwmon);
                set.insert(CollectorId::AmdSmu);
            }
            COOLING => {
                set.insert(CollectorId::Hwmon);
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
            HEATMAP => "Core history",
            COOLING => "Cooling",
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
                self.0
                    .package_index()
                    .and_then(|index| self.0.sensors.get(index)),
                &self.0.package_history,
                self.0.package_history_len,
                self.0.package_history_next,
            ),
            CORES => paint::cores(&mut grid, inner, &self.0.cores),
            SENSORS => paint::sensors(&mut grid, inner, &self.0.sensors),
            ZONES => paint::zones(&mut grid, inner, &self.0.zones, &self.0.cooling),
            GPU => paint::gpu(&mut grid, inner, self.0),
            HEATMAP => paint::heatmap(
                &mut grid,
                inner,
                &self.0.core_history,
                &self.0.core_history_len,
                &self.0.core_history_next,
            ),
            COOLING => paint::cooling(&mut grid, inner, &self.0.fans, &self.0.sensors),
            _ => {}
        }
        if placed.id == PACKAGE && inner.rows > 5 && !self.0.temp_history.is_empty() {
            paint::telemetry_label(&mut grid, inner);
        }
        if placed.id == GPU && inner.rows > 5 && self.0.gpu_history_len > 0 {
            paint::gpu_history_label(&mut grid, inner);
        }
        Emitter::default().emit(
            &grid,
            Rect::full(rect.cols, rect.rows),
            surface,
            rect.x,
            rect.y,
        );
        if placed.id == COOLING {
            let rows_per_fan = 2;
            let fan_count = self.0.fans.len().min(inner.rows / rows_per_fan);
            let graph_x = rect.x.saturating_add(inner.x).saturating_add(24);
            let graph_width = inner.cols.saturating_sub(24 + 13);
            for (index, fan) in self.0.fans.iter().take(fan_count).enumerate() {
                let count = fan.history_len.min(HISTORY_CAP);
                let first = (fan.history_next + HISTORY_CAP - count) % HISTORY_CAP;
                let mut normalized = [f32::NAN; HISTORY_CAP];
                for (offset, value) in normalized.iter_mut().take(count).enumerate() {
                    let rpm = fan.history[(first + offset) % HISTORY_CAP];
                    if rpm.is_finite() && fan.peak > 0.0 {
                        *value = (rpm / fan.peak).clamp(0.0, 1.0);
                    }
                }
                if count > 1
                    && !normalized[count - 1].is_finite()
                    && crate::monitor::graphs::table().braille
                {
                    // Braille packs two samples into one cell; keep the trailing
                    // missing sample's cell blank instead of showing its older half.
                    normalized[count - 2] = f32::NAN;
                }
                paint_history_runs(
                    &normalized,
                    count,
                    Rect {
                        x: graph_x,
                        y: rect
                            .y
                            .saturating_add(inner.y)
                            .saturating_add(index * rows_per_fan),
                        cols: graph_width,
                        rows: 1,
                    },
                    crate::monitor::graphs::table().braille,
                    surface,
                );
            }
        }
        if placed.id == PACKAGE && inner.rows > 5 && !self.0.temp_history.is_empty() {
            let count = self.0.temp_history.len().min(HISTORY_CAP);
            let first = self.0.temp_history.len().saturating_sub(count);
            let mut normalized = [f32::NAN; HISTORY_CAP];
            for (index, value) in normalized.iter_mut().take(count).enumerate() {
                let temp = self.0.temp_history[first + index];
                if temp.is_finite() {
                    *value = paint::thermal_level(temp, 80.0, 95.0);
                }
            }
            let plot = Rect {
                x: rect.x.saturating_add(inner.x),
                y: rect.y.saturating_add(inner.y).saturating_add(5),
                cols: inner.cols,
                rows: inner.rows.saturating_sub(5),
            };
            paint_history_runs(
                &normalized,
                count,
                plot,
                crate::monitor::graphs::table().braille,
                surface,
            );
        }
        if placed.id == GPU && inner.rows > 5 && self.0.gpu_history_len > 0 {
            let count = self.0.gpu_history_len.min(HISTORY_CAP);
            let first = (self.0.gpu_history_next + HISTORY_CAP - count) % HISTORY_CAP;
            let mut normalized = [f32::NAN; HISTORY_CAP];
            for (index, value) in normalized.iter_mut().take(count).enumerate() {
                let temp = self.0.gpu_history[(first + index) % HISTORY_CAP];
                if temp.is_finite() {
                    *value = paint::thermal_level(temp, 80.0, 95.0);
                }
            }
            let plot = Rect {
                x: rect.x.saturating_add(inner.x),
                y: rect.y.saturating_add(inner.y).saturating_add(5),
                cols: inner.cols,
                rows: inner.rows.saturating_sub(5),
            };
            paint_history_runs(
                &normalized,
                count,
                plot,
                crate::monitor::graphs::table().braille,
                surface,
            );
        }
        if placed.id == SENSORS {
            let spark_x = rect
                .x
                .saturating_add(inner.x)
                .saturating_add(paint::SENSOR_HISTORY_X);
            let spark_width = inner.cols.saturating_sub(paint::SENSOR_HISTORY_X);
            for (index, sensor) in self
                .0
                .sensors
                .iter()
                .enumerate()
                .take(inner.rows.saturating_sub(1))
            {
                let mut ordered = [0.0_f32; HISTORY_CAP];
                let count = if let Some(limit) = sensor.crit_c.or(sensor.max_c) {
                    let count = paint::history_into(sensor, spark_width, &mut ordered);
                    for sample in &mut ordered[..count] {
                        if sample.is_finite() {
                            *sample = (*sample / limit).clamp(0.0, 1.0);
                        }
                    }
                    count
                } else {
                    0
                };
                let row_y = rect.y.saturating_add(inner.y).saturating_add(index + 1);
                let mut run_start = 0;
                while run_start < count {
                    while run_start < count && !ordered[run_start].is_finite() {
                        run_start += 1;
                    }
                    let mut run_end = run_start;
                    while run_end < count && ordered[run_end].is_finite() {
                        run_end += 1;
                    }
                    if run_start < run_end {
                        let offset = spark_width.saturating_sub(count).saturating_add(run_start);
                        crate::monitor::graphs::paint_sparkline(
                            &ordered[run_start..run_end],
                            run_end - run_start,
                            thegn_core::theme::RampId::Temp,
                            Rect {
                                x: spark_x.saturating_add(offset),
                                y: row_y,
                                cols: run_end - run_start,
                                rows: 1,
                            },
                            surface,
                        );
                    }
                    run_start = run_end.saturating_add(1);
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "thermal_view_tests.rs"]
mod tests;
