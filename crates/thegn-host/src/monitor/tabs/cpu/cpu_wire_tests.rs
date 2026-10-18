use crate::{
    compositor::Rect,
    detail::StatusCtx,
    monitor::{MonitorOverlay, MonitorPrefs, MonitorTab},
    telemetry::TelemetryHistory,
};
use thegn_metrics::collect::{CollectorId, frame::MonitorFrame};

fn overlay() -> MonitorOverlay {
    let history = TelemetryHistory::default();
    let model = crate::chrome::FrameModel::default();
    let ctx = StatusCtx::new_for_test_on(&history, Rect::full(240, 60));
    MonitorOverlay::open(MonitorTab::Cpu, MonitorPrefs::default(), &model, &ctx)
}

#[test]
fn cpu_overlay_places_cpu_panels_and_demands_only_their_sources() {
    let overlay = overlay();
    assert_eq!(overlay.tab(), MonitorTab::Cpu);
    let view = overlay.view();
    let placed = overlay.board.placed(&view, overlay.body_rect());
    assert!(!placed.is_empty());
    assert!(placed.iter().all(|panel| panel.id.0.starts_with("cpu.")));
    let demand = overlay.demand(1000);
    assert!(demand.collectors.contains(CollectorId::CpuStat));
    assert!(demand.collectors.contains(CollectorId::CpuTopology));
    assert!(demand.collectors.contains(CollectorId::CpuIdle));
    assert!(demand.collectors.contains(CollectorId::CpuSched));
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
fn cpu_overlay_ingest_updates_visible_model_and_pause_freezes_it() {
    let mut overlay = overlay();
    let frame = MonitorFrame {
        parts: thegn_metrics::collect::frame::Parts {
            cpu_stat: thegn_metrics::collect::model::cpu_stat::CpuStatPart {
                procs_running: Some(7),
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    };
    assert!(overlay.ingest_frame(&frame));
    assert_eq!(overlay.cpu.procs_running(), Some(7));
    overlay.handle_key(
        &termwiz::input::KeyCode::Char(' '),
        termwiz::input::Modifiers::NONE,
    );
    let next = MonitorFrame {
        parts: thegn_metrics::collect::frame::Parts {
            cpu_stat: thegn_metrics::collect::model::cpu_stat::CpuStatPart {
                procs_running: Some(99),
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    };
    assert!(!overlay.ingest_frame(&next));
    assert_eq!(overlay.cpu.procs_running(), Some(7));
}
