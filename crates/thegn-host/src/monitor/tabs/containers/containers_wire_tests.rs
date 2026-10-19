use crate::{
    compositor::Rect,
    detail::StatusCtx,
    monitor::{MonitorOverlay, MonitorPrefs, MonitorTab, board::TabView as _},
    telemetry::TelemetryHistory,
};
use termwiz::input::{KeyCode, Modifiers};
use thegn_metrics::collect::{CollectorId, CollectorSet, frame::MonitorFrame, health::Health};

fn model() -> crate::chrome::FrameModel {
    crate::chrome::FrameModel {
        containers: vec![
            container("owned-one", true),
            container("foreign-one", false),
        ],
        ..Default::default()
    }
}

fn container(name: &str, ours: bool) -> thegn_core::sandbox::ContainerInfo {
    thegn_core::sandbox::ContainerInfo {
        name: name.into(),
        image: String::new(),
        status: "Up 1 minute".into(),
        ours,
        backend: "podman".into(),
        cpu: String::new(),
        mem: String::new(),
        net: String::new(),
        containment: String::new(),
        mounts: String::new(),
    }
}

fn overlay() -> MonitorOverlay {
    let history = TelemetryHistory::default();
    MonitorOverlay::open(
        MonitorTab::Containers,
        MonitorPrefs::default(),
        &model(),
        &StatusCtx::new_for_test_on(&history, Rect::full(300, 75)),
    )
}

#[test]
fn containers_opens_board_and_placed_panels_demand_stats_except_inventory() {
    let overlay = overlay();
    let view = overlay.view();
    assert!(matches!(
        view,
        crate::monitor::board::ActiveView::Containers(_)
    ));
    for (cols, rows) in [(100, 30), (160, 45), (240, 60)] {
        let area = Rect::full(cols, rows);
        let placed = overlay.board.placed(&view, area);
        assert!(
            placed
                .iter()
                .any(|panel| panel.id.0 == "containers.inventory")
        );
        let expected = placed.iter().fold(CollectorSet::empty(), |mut set, panel| {
            for id in view.demand(panel.id).iter() {
                set.insert(id);
            }
            set
        });
        assert!(expected.iter().any(|id| id == CollectorId::ContainersStats));
        assert_eq!(overlay.board.demand(&view, area, 1000).collectors, expected);
        assert!(
            view.demand(crate::monitor::layout::PanelId("containers.inventory"))
                .is_empty()
        );
    }
}

#[test]
fn containers_ingest_is_visible_unpaused_and_absent_stats_keep_inventory_painted() {
    let mut overlay = overlay();
    let mut frame = MonitorFrame::default();
    frame.health[CollectorId::ContainersStats.index()] =
        Health::Absent(thegn_metrics::collect::health::Absent::NotImplemented);
    frame.parts.containers_stats.scopes = 9;
    assert!(overlay.ingest_frame(&frame));
    assert_eq!(overlay.containers.scopes, 9);

    overlay.tab = MonitorTab::Cpu;
    frame.parts.containers_stats.scopes = 12;
    assert!(!overlay.ingest_frame(&frame));
    assert_eq!(overlay.containers.scopes, 9);
    overlay.tab = MonitorTab::Containers;
    overlay.handle_key(&KeyCode::Char(' '), Modifiers::NONE);
    assert!(!overlay.ingest_frame(&frame));

    overlay.paused = false;
    let view = overlay.view();
    let mut surface = termwiz::surface::Surface::new(240, 60);
    overlay
        .board
        .paint(&view, &mut surface, Rect::full(240, 60), 0, &overlay.body);
    let output = surface.screen_chars_to_string();
    assert!(
        output.contains("owned-one"),
        "inventory vanished with absent stats: {output}"
    );
    assert!(
        output.contains("foreign-one"),
        "inventory vanished with absent stats: {output}"
    );
    assert!(
        output.contains("1 owned · 1 foreign"),
        "ownership note missing: {output}"
    );
    assert!(overlay.ingest_frame(&frame));
    overlay.handle_key(&KeyCode::Char('j'), Modifiers::NONE);
    assert_eq!(overlay.sel, 1);
}
