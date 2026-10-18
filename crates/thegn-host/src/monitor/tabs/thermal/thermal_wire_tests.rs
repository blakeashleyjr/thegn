use crate::{
    chrome::FrameModel,
    compositor::Rect,
    detail::StatusCtx,
    monitor::{MonitorOverlay, MonitorPrefs, MonitorTab},
    telemetry::TelemetryHistory,
};
use std::sync::Arc;
use termwiz::{
    input::{KeyCode, Modifiers},
    surface::Surface,
};
use thegn_metrics::{
    StatsSnapshot,
    collect::{
        CollectorId, CollectorSet,
        frame::MonitorFrame,
        model::hwmon::{
            ChipGroup, CostClass, HwmonChip, HwmonLayout, HwmonSensor, HwmonValue, SensorKind,
            SensorRole, Thresholds,
        },
    },
};

fn thermal_overlay() -> (MonitorOverlay, FrameModel, TelemetryHistory) {
    let history = TelemetryHistory::default();
    let model = FrameModel {
        stats: StatsSnapshot {
            cpu_temp_c: Some(61.0),
            temps: vec![("coretemp".into(), 61.0), ("nvme".into(), 44.0)],
            ..StatsSnapshot::default()
        },
        ..FrameModel::default()
    };
    let screen = Rect::full(240, 60);
    let ctx = StatusCtx::new_for_test_on(&history, screen);
    let overlay = MonitorOverlay::open(MonitorTab::Thermal, MonitorPrefs::default(), &model, &ctx);
    (overlay, model, history)
}

fn frame(temp_c: f32) -> MonitorFrame {
    let mut frame = MonitorFrame::default();
    frame.parts.hwmon.layout = Some(Arc::new(HwmonLayout {
        generation: 1,
        chips: vec![HwmonChip {
            name: "coretemp".into(),
            device: "cpu0".into(),
            group: ChipGroup::Cpu,
            ordinal: 0,
            gated: false,
        }],
        sensors: vec![HwmonSensor {
            key: "coretemp@cpu0#temp1_input".into(),
            chip: 0,
            kind: SensorKind::Temp,
            channel: 1,
            role: SensorRole::Input,
            label: Some("Package".into()),
            thresholds: Thresholds {
                max: Some(85.0),
                crit: Some(100.0),
                ..Thresholds::default()
            },
            cost: CostClass::Fast,
        }],
    }));
    frame.parts.hwmon.values.push(HwmonValue {
        value: Some(temp_c),
        ..HwmonValue::default()
    });
    frame
}

fn rendered(overlay: &MonitorOverlay) -> String {
    let mut surface = Surface::new(240, 60);
    overlay.render(&mut surface, Rect::full(240, 60));
    surface.screen_chars_to_string()
}

#[test]
fn thermal_overlay_places_board_panels_and_requests_only_their_collectors() {
    let (overlay, _model, _history) = thermal_overlay();
    let text = rendered(&overlay);
    assert!(
        text.contains("CPU package"),
        "thermal board was not painted: {text}"
    );
    assert!(
        text.contains("Sensors"),
        "sensor panel was not placed: {text}"
    );
    assert!(
        text.contains("CPU cores"),
        "cores panel was not placed: {text}"
    );
    assert!(
        text.contains("GPU thermals"),
        "GPU panel was not placed: {text}"
    );
    assert!(
        text.contains("Thermal zones"),
        "zones panel was not placed: {text}"
    );
    assert!(
        text.contains("Sensor history"),
        "heatmap was not placed: {text}"
    );
    let demand = overlay.demand(1000).collectors;
    let mut expected = CollectorSet::empty();
    expected.insert(CollectorId::Hwmon);
    expected.insert(CollectorId::AmdSmu);
    expected.insert(CollectorId::ThermalZone);
    assert_eq!(demand, expected);
    assert!(demand.len() < CollectorId::ALL.len());
}

#[test]
fn thermal_ingest_updates_visible_model_and_pause_freezes_it() {
    let (mut overlay, _model, _history) = thermal_overlay();
    assert!(overlay.ingest_frame(&frame(61.0)));
    assert!(rendered(&overlay).contains("61.0 C"));

    overlay.handle_key(&KeyCode::Char(' '), Modifiers::NONE);
    assert!(!overlay.ingest_frame(&frame(79.0)));
    let frozen = rendered(&overlay);
    assert!(frozen.contains("61.0 C"), "paused view changed: {frozen}");
    assert!(
        !frozen.contains("79.0 C"),
        "paused frame was absorbed: {frozen}"
    );
}
