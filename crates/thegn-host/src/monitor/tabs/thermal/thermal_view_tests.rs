use super::{ThermalModel, ThermalView};
use crate::{
    caps::{self, test_override},
    compositor::Rect,
    monitor::{
        board::{Board, TabView},
        layout::PanelId,
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

const GOLDEN: &str =
    "CPU package|CPU cores|Sensors|Thermal zones|GPU thermals|k10temp|amdgpu|nvme|spd5118";

fn fixture() -> MonitorFrame {
    let chips = [
        ("k10temp", ChipGroup::Cpu),
        ("amdgpu", ChipGroup::Gpu),
        ("nvme", ChipGroup::Nvme),
        ("spd5118", ChipGroup::Dimm),
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
    let sensors = [
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
    let mut frame = MonitorFrame::default();
    frame.parts.hwmon.layout = Some(Arc::new(HwmonLayout {
        generation: 1,
        chips,
        sensors,
    }));
    frame.parts.hwmon.values = [
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
    .collect();
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
    let mut metrics = GpuMetrics {
        layout_verified: true,
        temperature_edge: Some(6_000),
        temperature_gfx: Some(6_200),
        temperature_soc: Some(5_400),
        power_socket: Some(45_000),
        ..GpuMetrics::default()
    };
    metrics.temperature_core[..4].copy_from_slice(&[
        Some(7_200),
        Some(7_400),
        Some(7_100),
        Some(7_600),
    ]);
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
    model.absorb_frame(&fixture());
    assert_eq!(model.sensors.len(), 6);
    assert!(model.sensors.iter().any(|sensor| sensor.chip == "nvme"));
    assert!(model.sensors.iter().any(|sensor| sensor.chip == "spd5118"));
    assert!(
        !model
            .sensors
            .iter()
            .any(|sensor| sensor.label == "bad sentinel")
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
fn no_zones_hides_panel_and_removes_thermal_zone_demand() {
    let mut frame = fixture();
    frame.parts.thermal_zone.zones.clear();
    let mut model = ThermalModel::default();
    model.absorb_frame(&frame);
    let view = ThermalView(&model);
    let board = Board::default();
    let placed = board.placed(&view, Rect::full(160, 45));
    assert!(
        !placed
            .iter()
            .any(|panel| panel.id == PanelId("thermal.zones"))
    );
    assert!(
        !board
            .demand(&view, Rect::full(160, 45), 1000)
            .collectors
            .contains(CollectorId::ThermalZone)
    );
}

#[test]
fn deterministic_snapshots_cover_sizes_capabilities_and_demand() {
    let mut model = ThermalModel::default();
    model.absorb_frame(&fixture());
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
                        assert!(output.contains("alarm 1"));
                    }
                    let required_text = if cols == 100 {
                        "CPU package|CPU cores|Sensors|GPU thermals|k10temp|amdgpu|nvme|spd5118"
                    } else {
                        GOLDEN
                    };
                    for required in required_text.split('|') {
                        assert!(
                            output.contains(required),
                            "missing {required} at {cols}x{rows}: {output}"
                        );
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
                        assert!(
                            fill_ratio(&surface, area) >= 0.55,
                            "fill ratio {:.3}",
                            fill_ratio(&surface, area)
                        );
                    }
                })
            });
        }
    }
}

#[test]
fn amd_cards_keep_first_available_cores_and_gpu_readings() {
    let mut frame = fixture();
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
    let mut model = ThermalModel::default();
    model.absorb_frame(&frame);
    let view = ThermalView(&model);
    let board = Board::default();
    let area = Rect::full(160, 45);
    assert!(
        !board
            .placed(&view, area)
            .iter()
            .any(|panel| { panel.id == PanelId("thermal.cores") })
    );
    assert!(
        !board
            .demand(&view, area, 1000)
            .collectors
            .contains(CollectorId::AmdSmu)
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
