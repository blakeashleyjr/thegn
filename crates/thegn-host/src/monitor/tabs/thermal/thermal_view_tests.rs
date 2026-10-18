use super::{HISTORY_CAP, ThermalModel, ThermalView};
use crate::monitor::kit::grid::CellGrid;
use crate::telemetry::{ScaleMode, TelemetryHistory, Window};
use crate::{
    caps::{self, test_override},
    compositor::Rect,
    monitor::{
        board::{Board, TabView},
        layout::{PanelId, Placed},
    },
};
use std::sync::Arc;
use termwiz::surface::Surface;
use thegn_core::termcaps::{ColorDepth, UnicodeLevel};
use thegn_metrics::collect::{
    CollectorId, CollectorSet,
    frame::MonitorFrame,
    model::{
        amd_smu::{AmdSmuCard, AmdSmuStatus},
        hwmon::{
            ChipGroup, CostClass, HwmonChip, HwmonLayout, HwmonSensor, HwmonValue, SensorKind,
            SensorRole, Thresholds,
        },
        thermal_zone::{ThermalTrip, ThermalZone, TripKind},
    },
    parse::amd_gpu_metrics::{Decoded, GpuMetrics},
};

fn fixture() -> MonitorFrame {
    let chips = [
        ("k10temp", ChipGroup::Cpu),
        ("amdgpu", ChipGroup::Gpu),
        ("nvme", ChipGroup::Nvme),
        ("spd5118", ChipGroup::Dimm),
        ("ztemp", ChipGroup::Cpu),
    ]
    .into_iter()
    .enumerate()
    .map(|(ordinal, (name, group))| HwmonChip {
        name: name.to_owned(),
        device: format!("device{ordinal}"),
        group,
        ordinal: ordinal as u16,
        gated: false,
    })
    .collect::<Vec<_>>();
    let mut sensors = [
        (0, "package", 71.0, Some(85.0), Some(95.0)),
        (1, "edge", 56.0, Some(90.0), Some(105.0)),
        (1, "junction", 78.0, Some(100.0), Some(110.0)),
        (2, "composite", 42.0, None, None),
        (3, "temperature", 39.0, None, None),
        (2, "bad sentinel", 65_261.85, None, None),
        (3, "warming", 0.0, None, None),
    ]
    .into_iter()
    .map(|(chip, label, _value, max, crit)| HwmonSensor {
        key: format!("sensor-{label}"),
        chip,
        kind: SensorKind::Temp,
        channel: 1,
        role: SensorRole::Input,
        label: Some(label.to_owned()),
        thresholds: Thresholds {
            max,
            crit,
            ..Thresholds::default()
        },
        cost: CostClass::Fast,
    })
    .collect::<Vec<_>>();
    for index in 0..16 {
        sensors.push(HwmonSensor {
            key: format!("core-{index}"),
            chip: 4,
            kind: SensorKind::Temp,
            channel: index + 2,
            role: SensorRole::Input,
            label: Some(format!("core{index}")),
            thresholds: Thresholds {
                max: Some(85.0),
                crit: Some(100.0),
                ..Thresholds::default()
            },
            cost: CostClass::Fast,
        });
    }
    let mut frame = MonitorFrame::default();
    frame.parts.hwmon.layout = Some(Arc::new(HwmonLayout {
        generation: 1,
        chips,
        sensors,
    }));
    let mut values = [
        Some(71.0),
        Some(56.0),
        Some(78.0),
        Some(42.0),
        Some(39.0),
        Some(65_261.85),
        None,
    ]
    .into_iter()
    .map(|value| HwmonValue {
        value,
        ..HwmonValue::default()
    })
    .collect::<Vec<_>>();
    values.extend((0..16).map(|index| HwmonValue {
        value: Some(60.0 + (index % 11) as f32),
        ..HwmonValue::default()
    }));
    frame.parts.hwmon.values = values;
    frame.parts.thermal_zone.zones = vec![
        ThermalZone {
            key: "zone-alias".into(),
            zone_type: "acpitz alias".into(),
            device: String::new(),
            policy: None,
            mode: None,
            temp_c: Some(71.0),
            trips: Vec::new(),
            alias: true,
            alarm_events: 0,
        },
        ThermalZone {
            key: "zone-real".into(),
            zone_type: "acpitz".into(),
            device: String::new(),
            policy: None,
            mode: None,
            temp_c: Some(49.0),
            trips: vec![ThermalTrip {
                index: 0,
                kind: TripKind::Critical,
                temp_c: Some(95.0),
                hyst_c: Some(2.0),
            }],
            alias: false,
            alarm_events: 1,
        },
    ];
    let metrics = GpuMetrics {
        layout_verified: true,
        temperature_edge: Some(6_000),
        temperature_gfx: Some(6_200),
        temperature_soc: Some(5_400),
        power_socket: Some(45_000),
        ..GpuMetrics::default()
    };
    frame.parts.amd_smu.cards.push(AmdSmuCard {
        card: 0,
        suspended: false,
        decoded: AmdSmuStatus::Decoded(Decoded::Full),
        metrics,
    });
    frame
}

fn text(surface: &mut Surface) -> String {
    surface.screen_chars_to_string()
}

fn fill_ratio(surface: &Surface, area: Rect) -> f32 {
    let (cols, rows) = surface.dimensions();
    let x0 = area.x.min(cols);
    let y0 = area.y.min(rows);
    let x1 = area.x.saturating_add(area.cols).min(cols);
    let y1 = area.y.saturating_add(area.rows).min(rows);
    let area_cells = x1.saturating_sub(x0).saturating_mul(y1.saturating_sub(y0));
    if area_cells == 0 {
        return 0.0;
    }
    let occupied = surface
        .screen_lines()
        .iter()
        .enumerate()
        .filter(|(y, _)| *y >= y0 && *y < y1)
        .flat_map(|(_, line)| line.visible_cells())
        .filter(|cell| {
            let x = cell.cell_index();
            x < x1
                && x.saturating_add(cell.width()) > x0
                && !cell.str().is_empty()
                && cell.str() != " "
                && cell.str() != "\u{2800}"
        })
        .map(|cell| {
            let x = cell.cell_index();
            cell.width()
                .min(x1.saturating_sub(x))
                .min(x.saturating_add(cell.width()).saturating_sub(x0))
        })
        .sum::<usize>();
    occupied as f32 / area_cells as f32
}

#[test]
fn absorb_frame_groups_and_filters_sensor_and_zone_data() {
    let mut model = ThermalModel::default();
    let frame = fixture();
    model.absorb_frame(&frame);
    assert_eq!(model.sensors.len(), 23);
    let chips = model
        .sensors
        .iter()
        .map(|sensor| sensor.chip.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        chips,
        ["amdgpu", "k10temp", "nvme", "spd5118", "ztemp"]
            .into_iter()
            .collect()
    );
    assert!(model.sensors.iter().any(|sensor| sensor.chip == "nvme"));
    assert!(model.sensors.iter().any(|sensor| sensor.chip == "spd5118"));
    assert!(
        model
            .sensors
            .iter()
            .any(|sensor| sensor.label == "bad sentinel" && sensor.temp_c.is_none())
    );
    assert!(
        model
            .sensors
            .iter()
            .any(|sensor| sensor.label == "warming" && sensor.temp_c.is_none())
    );
    assert_eq!(model.zones.len(), 1);
    assert_eq!(model.zones[0].label, "acpitz");
    assert_eq!(model.package().and_then(|sensor| sensor.temp_c), Some(71.0));
}

#[test]
fn empty_zone_panel_still_requests_its_collector() {
    let mut frame = fixture();
    frame.parts.thermal_zone.zones.clear();
    let mut model = ThermalModel::default();
    model.absorb_frame(&frame);
    let view = ThermalView(&model);
    let board = Board::default();
    let placed = board.placed(&view, Rect::full(160, 45));
    assert!(
        placed
            .iter()
            .any(|panel| panel.id == PanelId("thermal.zones"))
    );
    assert!(
        board
            .demand(&view, Rect::full(160, 45), 1000)
            .collectors
            .contains(CollectorId::ThermalZone)
    );
}

#[test]
fn deterministic_snapshots_cover_sizes_capabilities_and_demand() {
    let mut model = ThermalModel::default();
    let mut frame = fixture();
    for sample in 0..120 {
        let temp = if sample == 119 {
            71.0
        } else {
            45.0 + (sample % 26) as f32
        };
        frame.parts.hwmon.values[0].value = Some(temp);
        for (index, reading) in frame.parts.hwmon.values.iter_mut().skip(1).enumerate() {
            reading.value = Some(55.0 + ((sample + index * 3) % 30) as f32);
        }
        model.absorb_frame(&frame);
    }
    assert!(
        model
            .sensors
            .iter()
            .filter(|sensor| sensor.temp_c.is_some())
            .all(|sensor| sensor.history_len == 120)
    );
    assert!(model.core_history_len.iter().all(|len| *len == HISTORY_CAP));
    assert_eq!(model.core_history_len[0], HISTORY_CAP);
    let newest = (model.core_history_next[0] + HISTORY_CAP - 1) % HISTORY_CAP;
    let previous = (newest + HISTORY_CAP - 1) % HISTORY_CAP;
    assert_ne!(
        model.core_history[0][newest],
        model.core_history[0][previous]
    );
    let mut telemetry = TelemetryHistory::default();
    for sample in 0..120_u64 {
        telemetry.push(
            &thegn_metrics::StatsSnapshot {
                cpu_temp_c: Some(45.0 + (sample % 30) as f32),
                ..thegn_metrics::StatsSnapshot::default()
            },
            1_000 + sample * 1_000,
        );
    }
    model.absorb_history(
        &telemetry,
        Window::from_secs(180),
        ScaleMode::Fixed,
        121_000,
    );
    let view = ThermalView(&model);
    for (cols, rows) in [(100, 30), (160, 45), (240, 60)] {
        for (depth, unicode) in [
            (ColorDepth::Truecolor, UnicodeLevel::Full),
            (ColorDepth::Ansi16, UnicodeLevel::Full),
            (ColorDepth::Truecolor, UnicodeLevel::Ascii),
        ] {
            test_override::with_color(depth, || {
                test_override::with_unicode(unicode, || {
                    let mut surface = Surface::new(cols, rows);
                    let area = Rect::full(cols, rows);
                    let board = Board::default();
                    let placed = board.placed(&view, area);
                    board.paint(&view, &mut surface, area, 0, &[]);
                    let output = text(&mut surface);
                    assert!(output.contains("CPU package"));
                    assert!(output.contains("71.0 C"));
                    assert!(output.contains("nvme"));
                    assert!(output.contains("socket power"));
                    assert!(output.contains("critical"));
                    if cols != 100 {
                        assert!(output.contains("CPU cores"));
                        assert!(output.contains("Sensors"));
                        assert!(output.contains("Thermal zones"));
                        assert!(output.contains("GPU thermals"));
                        assert!(output.contains("k10temp"));
                        assert!(!output.contains("ref 100"));
                        assert!(output.contains("amdgpu"));
                        assert!(output.contains("spd5118"));
                        assert!(output.contains("alarm 1"));
                    }
                    let demand = board.demand(&view, area, 1000).collectors;
                    let mut expected = CollectorSet::empty();
                    for panel in placed {
                        for collector in view.demand(panel.id).iter() {
                            expected.insert(collector);
                        }
                    }
                    assert_eq!(demand, expected);
                    if cols == 100 {
                        assert!(output.contains("00 72C"));
                        assert!(!output.contains("Thermal zones"));
                        assert!(!demand.contains(CollectorId::ThermalZone));
                    }
                    assert_eq!(output.matches("composite").count(), 1);
                    if cols == 240 {
                        let r = fill_ratio(&surface, area);
                        // density follow-up (batch 51): estimated, primary sets from the printed value
                        assert!(r >= 0.33, "fill ratio {r:.3}");
                    }
                })
            });
        }
    }
}

#[test]
fn sensor_history_follows_stable_identity_and_stays_bounded() {
    let mut model = ThermalModel::default();
    let mut frame = fixture();
    model.absorb_frame(&frame);
    let package = model
        .sensors
        .iter()
        .find(|sensor| sensor.key == "sensor-package")
        .expect("package sensor");
    assert_eq!(package.history_len, 1);
    assert_eq!(package.history[0], 71.0);

    frame.parts.hwmon.layout = frame.parts.hwmon.layout.as_ref().map(|layout| {
        let mut reordered = (**layout).clone();
        reordered.sensors.swap(0, 1);
        Arc::new(reordered)
    });
    frame.parts.hwmon.values.swap(0, 1);
    for sample in 0..(HISTORY_CAP + 4) {
        if sample == HISTORY_CAP / 2 {
            frame.parts.hwmon.layout = frame.parts.hwmon.layout.as_ref().map(|layout| {
                let mut reordered = (**layout).clone();
                reordered.sensors.swap(0, 1);
                Arc::new(reordered)
            });
            frame.parts.hwmon.values.swap(0, 1);
        }
        let package_index = frame
            .parts
            .hwmon
            .layout
            .as_ref()
            .and_then(|layout| {
                layout
                    .sensors
                    .iter()
                    .position(|sensor| sensor.key == "sensor-package")
            })
            .expect("package sensor remains in layout");
        frame.parts.hwmon.values[package_index].value = Some(72.0 + sample as f32);
        model.absorb_frame(&frame);
    }
    let package = model
        .sensors
        .iter()
        .find(|sensor| sensor.key == "sensor-package")
        .expect("package sensor after reorder");
    assert_eq!(package.history_len, HISTORY_CAP);
    assert_eq!(
        package.history[(package.history_next + HISTORY_CAP - 1) % HISTORY_CAP],
        72.0 + (HISTORY_CAP + 3) as f32
    );
}

#[test]
fn implausible_transient_keeps_sensor_identity_and_records_a_gap() {
    let mut model = ThermalModel::default();
    let mut frame = fixture();
    model.absorb_frame(&frame);
    frame.parts.hwmon.values[0].value = Some(65_261.85);
    model.absorb_frame(&frame);
    let package = model
        .sensors
        .iter()
        .find(|sensor| sensor.key == "sensor-package")
        .expect("implausible sample must not drop the sensor row");
    assert_eq!(package.history_len, 2);
    assert!(package.history[(package.history_next + HISTORY_CAP - 1) % HISTORY_CAP].is_nan());
}

#[test]
fn package_without_threshold_shows_temperature_without_a_meter() {
    let mut model = ThermalModel::default();
    model.absorb_frame(&fixture());
    let mut package = model.package().expect("package sensor").clone();
    package.max_c = None;
    package.crit_c = None;
    let mut grid = CellGrid::new(60, 8);
    super::paint::package(
        &mut grid,
        Rect::full(60, 8),
        Some(&package),
        &model.package_history,
        model.package_history_len,
        model.package_history_next,
    );
    assert!((0..60).all(|x| grid.get(x, 0).expect("meter row cell").ch == ' '));
    let reading = (0..60)
        .map(|x| grid.get(x, 1).expect("temperature row cell").ch)
        .collect::<String>();
    assert!(reading.contains("71.0 C"));

    let mut sensor_grid = CellGrid::new(100, 3);
    super::paint::sensors(&mut sensor_grid, Rect::full(100, 3), &[package]);
    let bar_row = (59..71)
        .map(|x| sensor_grid.get(x, 1).expect("sensor bar cell").ch)
        .collect::<String>();
    let (filled, _) = caps::bar_track(0.71, 12);
    assert!(!bar_row.contains(&filled));
}

#[test]
fn telemetry_label_and_graph_paint_without_a_package_sensor() {
    let mut history = TelemetryHistory::default();
    for (at_ms, temp_c) in [(1_000, 51.0), (2_000, 58.0), (3_000, 63.0)] {
        history.push(
            &thegn_metrics::StatsSnapshot {
                cpu_temp_c: Some(temp_c),
                ..thegn_metrics::StatsSnapshot::default()
            },
            at_ms,
        );
    }
    let mut model = ThermalModel::default();
    model.absorb_history(&history, Window::from_secs(10), ScaleMode::Fixed, 3_001);
    assert!(model.sensors.is_empty());
    let panel = Placed {
        id: super::PACKAGE,
        rect: Rect::full(80, 16),
        variant: super::FULL.kind,
    };
    let mut surface = Surface::new(80, 16);
    let mut empty_surface = Surface::new(80, 16);
    test_override::with_unicode(UnicodeLevel::Ascii, || {
        ThermalView(&model).paint(&mut surface, &panel, Rect::full(80, 16), 0, &[]);
        ThermalView(&ThermalModel::default()).paint(
            &mut empty_surface,
            &panel,
            Rect::full(80, 16),
            0,
            &[],
        );
    });
    let output = text(&mut surface);
    let empty_output = text(&mut empty_surface);
    assert!(output.contains("telemetry 80/95 C"));
    assert_ne!(
        output.lines().skip(6).take(9).collect::<Vec<_>>(),
        empty_output.lines().skip(6).take(9).collect::<Vec<_>>()
    );
}

#[test]
fn implausible_sensor_gap_keeps_the_newest_spark_cell_blank() {
    let mut model = ThermalModel::default();
    let mut frame = fixture();
    model.absorb_frame(&frame);
    frame.parts.hwmon.values[0].value = Some(65_261.85);
    model.absorb_frame(&frame);
    let package_index = model
        .sensors
        .iter()
        .position(|sensor| sensor.key == "sensor-package")
        .expect("package sensor row");
    let panel = Placed {
        id: super::SENSORS,
        rect: Rect::full(160, 45),
        variant: super::FULL.kind,
    };
    let mut surface = Surface::new(160, 45);
    test_override::with_unicode(UnicodeLevel::Ascii, || {
        ThermalView(&model).paint(&mut surface, &panel, Rect::full(160, 45), 0, &[]);
    });
    let output = text(&mut surface);
    let row = output.lines().nth(package_index + 2).expect("package row");
    let newest_column =
        1 + super::paint::SENSOR_HISTORY_X + (160 - 2 - super::paint::SENSOR_HISTORY_X) - 1;
    assert_eq!(row.chars().nth(newest_column), Some(' '));
}

#[test]
fn warm_absorbs_do_not_grow_sensor_or_zone_storage() {
    let frame = fixture();
    let mut model = ThermalModel::default();
    model.absorb_frame(&frame);
    let sensor_capacity = model.sensors.capacity();
    let package_key_capacity = model
        .package_history_key
        .as_ref()
        .map_or(0, String::capacity);
    let zone_capacity = model.zones.capacity();
    let trip_capacities = model
        .zones
        .iter()
        .map(|zone| zone.trips.capacity())
        .collect::<Vec<_>>();
    let sensor_string_capacities = model
        .sensors
        .iter()
        .map(|sensor| (sensor.key.capacity(), sensor.label.capacity()))
        .collect::<Vec<_>>();
    let (_, counts) = crate::proc_workload_alloc::measure(|| {
        for _ in 0..1000 {
            model.absorb_frame(&frame);
        }
    });
    assert_eq!(
        counts.allocation_calls, 0,
        "1000 warm thermal absorbs allocated: {counts:?}"
    );
    assert_eq!(
        counts.realloc_calls, 0,
        "1000 warm thermal absorbs reallocated: {counts:?}"
    );
    assert_eq!(model.sensors.capacity(), sensor_capacity);
    assert_eq!(
        model
            .package_history_key
            .as_ref()
            .map_or(0, String::capacity),
        package_key_capacity
    );
    assert_eq!(model.zones.capacity(), zone_capacity);
    assert_eq!(
        model
            .zones
            .iter()
            .map(|zone| zone.trips.capacity())
            .collect::<Vec<_>>(),
        trip_capacities
    );
    assert_eq!(
        model
            .sensors
            .iter()
            .map(|sensor| (sensor.key.capacity(), sensor.label.capacity()))
            .collect::<Vec<_>>(),
        sensor_string_capacities
    );
    assert!(
        model
            .sensors
            .iter()
            .filter(|sensor| sensor.temp_c.is_some())
            .all(|sensor| sensor.history_len == HISTORY_CAP)
    );
}

#[test]
fn telemetry_temp_series_is_separate_from_frame_package_history() {
    let mut history = TelemetryHistory::default();
    for (at_ms, temp_c) in [(1_000, 51.0), (2_000, 58.0), (3_000, 63.0)] {
        history.push(
            &thegn_metrics::StatsSnapshot {
                cpu_temp_c: Some(temp_c),
                ..thegn_metrics::StatsSnapshot::default()
            },
            at_ms,
        );
    }
    let mut model = ThermalModel::default();
    model.absorb_frame(&fixture());
    // Telemetry windows are [start, now), so put the latest recorded sample
    // just before the exclusive upper bound.
    model.absorb_history(&history, Window::from_secs(10), ScaleMode::Window, 3_001);
    assert_eq!(model.temp_history.len(), HISTORY_CAP);
    assert_eq!(model.temp_history.last(), Some(&63.0));
    assert_eq!(model.package_history_len, 1);
    assert_eq!(model.package_history[0], 71.0);

    let mut without_telemetry = model.clone();
    without_telemetry.temp_history.clear();
    let area = Rect::full(160, 45);
    let mut with_telemetry_surface = Surface::new(160, 45);
    let mut without_telemetry_surface = Surface::new(160, 45);
    Board::default().paint(
        &ThermalView(&model),
        &mut with_telemetry_surface,
        area,
        0,
        &[],
    );
    Board::default().paint(
        &ThermalView(&without_telemetry),
        &mut without_telemetry_surface,
        area,
        0,
        &[],
    );
    let with_text = text(&mut with_telemetry_surface);
    let without_text = text(&mut without_telemetry_surface);
    assert_ne!(with_text, without_text);
    assert!(with_text.contains("CPU package"));
    assert!(with_text.contains("telemetry"));
    assert_eq!(model.package_history_len, 1);
    assert_eq!(model.package_history[0], 71.0);

    let mut graph_surface = Surface::new(160, 45);
    let mut frame_only_surface = Surface::new(160, 45);
    let panel = Placed {
        id: super::PACKAGE,
        rect: area,
        variant: super::FULL.kind,
    };
    ThermalView(&model).paint(&mut graph_surface, &panel, area, 0, &[]);
    ThermalView(&without_telemetry).paint(&mut frame_only_surface, &panel, area, 0, &[]);
    let graph_text = text(&mut graph_surface);
    let frame_only_text = text(&mut frame_only_surface);
    assert!(graph_text.contains("telemetry 80/95 C"));
    let graph_rows = graph_text.lines().collect::<Vec<_>>();
    let frame_rows = frame_only_text.lines().collect::<Vec<_>>();
    for row in 0..=4 {
        assert_eq!(graph_rows[row], frame_rows[row]);
    }
    assert!(graph_rows[5].contains("telemetry 80/95 C"));
    assert_ne!(graph_rows[5], frame_rows[5]);
    assert!(
        graph_rows[6..=43]
            .iter()
            .zip(&frame_rows[6..=43])
            .any(|(left, right)| left != right)
    );
    assert!(frame_rows[6..=43].iter().all(|row| {
        row.chars()
            .skip(1)
            .take(158)
            .all(|cell| cell == ' ' || cell == '\u{2800}')
    }));
    let newest_graph_cell = graph_rows[43]
        .chars()
        .nth(158)
        .expect("package graph's final inner column");
    assert!(newest_graph_cell != ' ' && newest_graph_cell != '\u{2800}');

    let mut with_graph = CellGrid::new(80, 12);
    let mut without_graph = CellGrid::new(80, 12);
    super::paint::package(
        &mut with_graph,
        Rect::full(80, 12),
        model.package(),
        &model.package_history,
        model.package_history_len,
        model.package_history_next,
    );
    super::paint::package(
        &mut without_graph,
        Rect::full(80, 12),
        model.package(),
        &model.package_history,
        model.package_history_len,
        model.package_history_next,
    );
    let row = |grid: &CellGrid, y| {
        (0..80)
            .map(|x| grid.get(x, y).expect("package graph cell").ch)
            .collect::<String>()
    };
    assert_eq!(row(&with_graph, 3), row(&without_graph, 3));
}

#[test]
fn hwmon_core_source_stays_selected_for_the_layout_generation() {
    let mut frame = fixture();
    if let Some(card) = frame.parts.amd_smu.cards.first_mut() {
        card.metrics.temperature_core[0] = Some(9_000);
    }
    let mut model = ThermalModel::default();
    frame.parts.hwmon.values[7].value = Some(61.0);
    model.absorb_frame(&frame);
    assert_eq!(model.cores[0], Some(61.0));
    frame.parts.hwmon.values[7].value = Some(62.0);
    if let Some(card) = frame.parts.amd_smu.cards.first_mut() {
        card.metrics.temperature_core[0] = Some(9_100);
    }
    model.absorb_frame(&frame);
    let newest = (model.core_history_next[0] + HISTORY_CAP - 1) % HISTORY_CAP;
    assert_eq!(model.core_history[0][newest], 62.0);
}

#[test]
fn smu_core_panel_is_placed_before_the_first_core_reading() {
    let mut first_frame = fixture();
    if let Some(card) = first_frame.parts.amd_smu.cards.first_mut() {
        card.metrics.temperature_core = [None; 16];
    }
    if let Some(layout) = first_frame.parts.hwmon.layout.as_deref() {
        let retained = layout
            .sensors
            .iter()
            .enumerate()
            .filter(|(_, sensor)| {
                !sensor
                    .label
                    .as_deref()
                    .is_some_and(|label| label.starts_with("core"))
            })
            .collect::<Vec<_>>();
        let mut without_core_layout = (*layout).clone();
        without_core_layout.sensors = retained
            .iter()
            .map(|(_, sensor)| (**sensor).clone())
            .collect();
        first_frame.parts.hwmon.values = retained
            .iter()
            .filter_map(|(index, _)| first_frame.parts.hwmon.values.get(*index).cloned())
            .collect();
        first_frame.parts.hwmon.layout = Some(Arc::new(without_core_layout));
    }
    let mut second_frame = first_frame.clone();
    if let Some(card) = second_frame.parts.amd_smu.cards.first_mut() {
        card.metrics.temperature_core[0] = Some(8_000);
    }
    let area = Rect::full(160, 45);
    let mut model = ThermalModel::default();
    model.absorb_frame(&first_frame);
    let first_placed = Board::default()
        .placed(&ThermalView(&model), area)
        .into_iter()
        .map(|panel| panel.id)
        .collect::<Vec<_>>();
    model.absorb_frame(&second_frame);
    let second_placed = Board::default()
        .placed(&ThermalView(&model), area)
        .into_iter()
        .map(|panel| panel.id)
        .collect::<Vec<_>>();
    assert_eq!(first_placed, second_placed);
    assert!(first_placed.contains(&super::CORES));
}

#[test]
fn core_panel_presence_does_not_follow_missing_frame_values() {
    let mut frame = fixture();
    let mut model = ThermalModel::default();
    model.absorb_frame(&frame);
    let area = Rect::full(160, 45);
    let first_placed = Board::default()
        .placed(&ThermalView(&model), area)
        .into_iter()
        .map(|panel| panel.id)
        .collect::<Vec<_>>();
    for value in frame.parts.hwmon.values.iter_mut().skip(7) {
        value.value = None;
    }
    model.absorb_frame(&frame);
    let view = ThermalView(&model);
    let second_placed = Board::default()
        .placed(&view, area)
        .into_iter()
        .map(|panel| panel.id)
        .collect::<Vec<_>>();
    assert_eq!(first_placed, second_placed);
    assert!(
        Board::default()
            .placed(&view, area)
            .iter()
            .any(|panel| panel.id == super::CORES)
    );
}

#[test]
fn telemetry_series_storage_stays_warm_across_one_thousand_absorbs() {
    let mut history = TelemetryHistory::default();
    for sample in 0..120_u64 {
        history.push(
            &thegn_metrics::StatsSnapshot {
                cpu_temp_c: Some(40.0 + (sample % 45) as f32),
                ..thegn_metrics::StatsSnapshot::default()
            },
            1_000 + sample * 1_000,
        );
    }
    let mut model = ThermalModel::default();
    model.absorb_history(&history, Window::from_secs(180), ScaleMode::Window, 121_000);
    let history_capacity = model.temp_history.capacity();
    assert_eq!(model.temp_history.len(), HISTORY_CAP);
    let (_, counts) = crate::proc_workload_alloc::measure(|| {
        for _ in 0..1_000 {
            model.absorb_history(&history, Window::from_secs(180), ScaleMode::Window, 121_000);
        }
    });
    assert_eq!(
        counts.allocation_calls, 0,
        "1000 telemetry history absorbs allocated: {counts:?}"
    );
    assert_eq!(
        counts.realloc_calls, 0,
        "1000 telemetry history absorbs reallocated: {counts:?}"
    );
    assert_eq!(model.temp_history.capacity(), history_capacity);
    assert_eq!(model.temp_history.last(), Some(&69.0));
}

#[test]
fn telemetry_temp_history_keeps_leading_gaps_and_holds_interior_gaps() {
    let mut history = TelemetryHistory::default();
    for (at_ms, temp_c) in [(1_000, 50.0), (9_000, 65.0)] {
        history.push(
            &thegn_metrics::StatsSnapshot {
                cpu_temp_c: Some(temp_c),
                ..thegn_metrics::StatsSnapshot::default()
            },
            at_ms,
        );
    }
    let mut model = ThermalModel::default();
    model.absorb_history(&history, Window::from_secs(10), ScaleMode::Window, 10_001);
    let first_sample = model
        .temp_history
        .iter()
        .position(|value| value.is_finite())
        .expect("recorded telemetry sample");
    assert!(first_sample > 0);
    assert!(
        model.temp_history[..first_sample]
            .iter()
            .all(|value| value.is_nan())
    );
    assert!(
        model.temp_history[first_sample..]
            .iter()
            .all(|value| value.is_finite())
    );
    assert!(
        model.temp_history[first_sample..]
            .windows(2)
            .any(|pair| pair[0] == pair[1])
    );
}

#[test]
fn zoomed_cores_panel_keeps_hwmon_demand() {
    let mut model = ThermalModel::default();
    model.absorb_frame(&fixture());
    let view = ThermalView(&model);
    let mut board = Board::default();
    let area = Rect::full(160, 45);
    let placed = board.placed(&view, area);
    assert!(placed.iter().any(|panel| panel.id == super::CORES));
    let core_index = placed
        .iter()
        .position(|panel| panel.id == super::CORES)
        .expect("cores panel placement");
    for _ in 0..=core_index {
        board.focus_next(&placed);
    }
    board.toggle_zoom(&placed);
    let demand = board.demand(&view, area, 1000);
    assert!(demand.collectors.contains(CollectorId::Hwmon));
}

#[test]
fn sensor_history_overlay_starts_after_the_last_bar_cell() {
    let mut grid = CellGrid::new(100, 5);
    let mut model = ThermalModel::default();
    model.absorb_frame(&fixture());
    super::paint::sensors(&mut grid, Rect::full(100, 5), &model.sensors);
    assert_eq!(super::paint::SENSOR_HISTORY_X, 72);
    assert_ne!(grid.get(70, 1).expect("bar cell is in grid").ch, ' ');
}

#[test]
fn core_heat_cells_keep_individual_temperatures_in_unicode_and_ascii() {
    for unicode in [UnicodeLevel::Full, UnicodeLevel::Ascii] {
        for depth in [ColorDepth::Truecolor, ColorDepth::Ansi16] {
            test_override::with_unicode(unicode, || {
                test_override::with_color(depth, || {
                    let mut grid = CellGrid::new(24, 2);
                    let cores = [
                        Some(40.0),
                        Some(95.0),
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                    ];
                    super::paint::cores(&mut grid, Rect::full(24, 2), &cores);
                    let cool = grid.get(0, 0).expect("cool core heat cell");
                    let hot = grid.get(10, 0).expect("hot core heat cell");
                    assert_ne!(cool.fg, hot.fg);
                });
            });
        }
    }
}

#[test]
fn heatmap_labels_name_core_three_in_unicode_and_ascii() {
    for (unicode, row, label) in [
        (UnicodeLevel::Full, 1, "c02/03 "),
        (UnicodeLevel::Ascii, 3, "core 03 "),
    ] {
        test_override::with_unicode(unicode, || {
            let mut history = [[f32::NAN; HISTORY_CAP]; 16];
            let mut history_len = [0; 16];
            let history_next = [0; 16];
            history[3] = [95.0; HISTORY_CAP];
            history_len[3] = HISTORY_CAP;
            let mut grid = CellGrid::new(32, 16);
            super::paint::heatmap(
                &mut grid,
                Rect::full(32, 16),
                &history,
                &history_len,
                &history_next,
            );
            let label_width = label.len();
            let actual_label = (0..label_width)
                .map(|col| grid.get(col, row).expect("heatmap label cell").ch)
                .collect::<String>();
            assert_eq!(actual_label, label);
            assert_ne!(
                grid.get(label_width, row).expect("core 3 heat cell").ch,
                ' '
            );
            assert_eq!(
                grid.get(0, row + 1).expect("no nonexistent core label").ch,
                ' '
            );
        });
    }
}

#[test]
fn amd_cards_keep_first_available_cores_and_gpu_readings() {
    let mut frame = fixture();
    if let Some(layout) = frame.parts.hwmon.layout.as_deref() {
        let retained = layout
            .sensors
            .iter()
            .enumerate()
            .filter(|(_, sensor)| {
                !sensor
                    .label
                    .as_deref()
                    .is_some_and(|label| label.starts_with("core"))
            })
            .collect::<Vec<_>>();
        let mut filtered = (*layout).clone();
        filtered.generation = 2;
        filtered.sensors = retained
            .iter()
            .map(|(_, sensor)| (**sensor).clone())
            .collect();
        frame.parts.hwmon.values = retained
            .iter()
            .filter_map(|(index, _)| frame.parts.hwmon.values.get(*index).cloned())
            .collect();
        frame.parts.hwmon.layout = Some(Arc::new(filtered));
    }
    let mut dgpu = GpuMetrics {
        temperature_edge: Some(8_100),
        temperature_gfx: Some(8_200),
        temperature_soc: Some(8_300),
        ..GpuMetrics::default()
    };
    dgpu.temperature_core.fill(None);
    let mut apu = GpuMetrics {
        temperature_edge: Some(6_100),
        temperature_gfx: Some(6_200),
        temperature_soc: Some(6_300),
        ..GpuMetrics::default()
    };
    apu.temperature_core[0] = Some(7_200);
    frame.parts.amd_smu.cards.clear();
    frame.parts.amd_smu.cards.extend([
        AmdSmuCard {
            card: 0,
            suspended: false,
            decoded: AmdSmuStatus::Decoded(Decoded::Full),
            metrics: dgpu,
        },
        AmdSmuCard {
            card: 1,
            suspended: false,
            decoded: AmdSmuStatus::Decoded(Decoded::Full),
            metrics: apu,
        },
    ]);
    let mut model = ThermalModel::default();
    model.absorb_frame(&frame);
    assert_eq!(model.cores[0], Some(72.0));
    assert_eq!(model.core_history_len[0], 1);
    assert_eq!(model.gfx_c, Some(82.0));
    assert_eq!(model.edge_c, Some(81.0));
    assert_eq!(model.soc_c, Some(83.0));
    let mut surface = Surface::new(160, 45);
    Board::default().paint(
        &ThermalView(&model),
        &mut surface,
        Rect::full(160, 45),
        0,
        &[],
    );
    assert!(text(&mut surface).contains("edge  81.0 C"));
}

#[test]
fn cores_panel_is_absent_without_core_temperatures() {
    let mut frame = fixture();
    frame.parts.amd_smu.cards.clear();
    frame.health[CollectorId::AmdSmu.index()] = thegn_metrics::collect::health::Health::Absent(
        thegn_metrics::collect::health::Absent::NoHardware,
    );
    if let Some(layout) = frame.parts.hwmon.layout.as_deref() {
        let retained = layout
            .sensors
            .iter()
            .enumerate()
            .filter(|(_, sensor)| {
                !sensor
                    .label
                    .as_deref()
                    .is_some_and(|label| label.starts_with("core"))
            })
            .collect::<Vec<_>>();
        let mut filtered = (*layout).clone();
        filtered.generation = 2;
        filtered.sensors = retained
            .iter()
            .map(|(_, sensor)| (**sensor).clone())
            .collect();
        frame.parts.hwmon.values = retained
            .iter()
            .filter_map(|(index, _)| frame.parts.hwmon.values.get(*index).cloned())
            .collect();
        frame.parts.hwmon.layout = Some(Arc::new(filtered));
    }
    let mut model = ThermalModel::default();
    model.absorb_frame(&frame);
    let view = ThermalView(&model);
    let board = Board::default();
    let area = Rect::full(160, 45);
    assert!(
        !board
            .placed(&view, area)
            .iter()
            .any(|panel| panel.id == PanelId("thermal.cores"))
    );
}

#[test]
fn absent_values_render_a_mark_without_zero_readings() {
    let mut frame = fixture();
    for value in &mut frame.parts.hwmon.values {
        value.value = None;
    }
    frame.parts.amd_smu.cards.clear();
    frame.parts.thermal_zone.zones.clear();
    let mut model = ThermalModel::default();
    model.absorb_frame(&frame);
    let view = ThermalView(&model);
    let mut surface = Surface::new(100, 30);
    Board::default().paint(&view, &mut surface, Rect::full(100, 30), 0, &[]);
    let output = text(&mut surface);
    assert!(output.contains(caps::active_glyphs().emdash));
    assert!(!output.contains("0.0 C"));
    assert!(!output.contains("0 C"));
}
