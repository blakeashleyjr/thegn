use crate::{
    compositor::Rect,
    detail::StatusCtx,
    monitor::{MonitorOverlay, MonitorPrefs, MonitorTab, board::TabView},
    telemetry::TelemetryHistory,
};
use thegn_metrics::{
    StatsSnapshot,
    collect::{CollectorId, frame::MonitorFrame},
};

fn overlay() -> MonitorOverlay {
    let history = TelemetryHistory::default();
    let model = crate::chrome::FrameModel {
        stats: StatsSnapshot {
            battery: Some((73, false)),
            ..StatsSnapshot::default()
        },
        ..crate::chrome::FrameModel::default()
    };
    let ctx = StatusCtx::new_for_test_on(&history, Rect::full(240, 60));
    MonitorOverlay::open(MonitorTab::Power, MonitorPrefs::default(), &model, &ctx)
}

#[test]
fn power_overlay_places_power_panels_and_demands_only_their_sources() {
    let overlay = overlay();
    assert_eq!(overlay.tab(), MonitorTab::Power);
    let view = overlay.view();
    let placed = overlay.board.placed(&view, overlay.body_rect());
    assert!(!placed.is_empty());
    assert!(placed.iter().all(|panel| panel.id.0.starts_with("power.")));
    let demand = overlay.demand(1000);
    assert!(demand.collectors.contains(CollectorId::PowerSupply));
    assert!(demand.collectors.contains(CollectorId::Powercap));
    assert!(demand.collectors.contains(CollectorId::AmdSmu));
    assert!(demand.collectors.contains(CollectorId::CpuFreq));
    assert_eq!(
        demand.collectors.iter().count(),
        [
            CollectorId::PowerSupply,
            CollectorId::Powercap,
            CollectorId::AmdSmu,
            CollectorId::CpuFreq,
        ]
        .len()
    );
    assert!(demand.collectors.iter().count() < CollectorId::ALL.len());
    let expected = placed.iter().fold(
        thegn_metrics::collect::CollectorSet::empty(),
        |mut set, panel| {
            for id in view.demand(panel.id).iter() {
                set.insert(id);
            }
            set
        },
    );
    assert_eq!(demand.collectors, expected);
}

#[test]
fn power_overlay_ingest_updates_visible_model_and_pause_freezes_it() {
    let mut overlay = overlay();
    let frame = MonitorFrame {
        parts: thegn_metrics::collect::frame::Parts {
            power_supply: thegn_metrics::collect::model::power_supply::PowerSupplyPart {
                supplies: vec![thegn_metrics::collect::model::power_supply::SupplyReading {
                    name: "BAT1".into(),
                    kind: thegn_metrics::collect::model::power_supply::SupplyKind::Battery,
                    device_scope: false,
                    online: None,
                    status: Some(
                        thegn_metrics::collect::model::power_supply::SupplyStatus::Discharging,
                    ),
                    capacity_pct: Some(73),
                    energy_now_uwh: None,
                    energy_full_uwh: None,
                    energy_full_design_uwh: None,
                    voltage_now_uv: None,
                    voltage_min_design_uv: None,
                    current_now_ua: None,
                    power_now_uw: None,
                    cycle_count: None,
                    usb_type_active: None,
                }],
            },
            ..Default::default()
        },
        ..MonitorFrame::default()
    };
    assert!(overlay.ingest_frame(&frame));
    assert_eq!(overlay.power.batteries[0].capacity, Some(73));
    overlay.handle_key(
        &termwiz::input::KeyCode::Char(' '),
        termwiz::input::Modifiers::NONE,
    );
    let next = MonitorFrame::default();
    assert!(!overlay.ingest_frame(&next));
    assert_eq!(overlay.power.batteries[0].capacity, Some(73));
}
