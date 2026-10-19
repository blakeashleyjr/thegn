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
    use termwiz::cell::Intensity;

    let mut overlay = overlay();
    let mut frame = MonitorFrame::default();
    frame.sampled.insert(CollectorId::ContainersStats);
    frame.health[CollectorId::ContainersStats.index()] =
        Health::Absent(thegn_metrics::collect::health::Absent::NotImplemented);
    frame.parts.containers_stats.scopes = 9;
    assert!(overlay.ingest_frame(&frame));
    assert_eq!(overlay.containers.scopes, 9);

    overlay.tab = MonitorTab::Procs;
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
    let area = Rect::full(240, 60);
    let before_view = overlay.view();
    let inventory_panel = overlay
        .board
        .placed(&before_view, area)
        .into_iter()
        .find(|panel| panel.id.0 == "containers.inventory")
        .unwrap();
    let mut before = termwiz::surface::Surface::new(240, 60);
    overlay
        .board
        .paint(&before_view, &mut before, area, 0, &overlay.body);
    let before_row = before.screen_lines()[inventory_panel.rect.y + 2].clone();
    let before_cell = before_row
        .visible_cells()
        .find(|cell| cell.str() == "o")
        .unwrap();
    let selected_bg = crate::seg::Tok::Series(thegn_core::theme::SeriesRole::Selected)
        .resolve(&crate::chrome::palette_snapshot());
    assert_eq!(
        before_cell.attrs().background(),
        selected_bg,
        "owned inventory row should start selected"
    );
    assert_eq!(before_cell.attrs().intensity(), Intensity::Bold);

    overlay.handle_key(&KeyCode::Char('j'), Modifiers::NONE);
    assert_eq!(overlay.sel, 1);
    let after_view = overlay.view();
    let mut after = termwiz::surface::Surface::new(240, 60);
    overlay
        .board
        .paint(&after_view, &mut after, area, 0, &overlay.body);
    let owned_row = after.screen_lines()[inventory_panel.rect.y + 2].clone();
    let owned_cell = owned_row
        .visible_cells()
        .find(|cell| cell.str() == "o")
        .unwrap();
    let foreign_row = after.screen_lines()[inventory_panel.rect.y + 3].clone();
    let foreign_cell = foreign_row
        .visible_cells()
        .find(|cell| cell.str() == "f")
        .unwrap();
    let ghost_fg =
        crate::seg::Tok::Slot(crate::chrome::S::Ghost).resolve(&crate::chrome::palette_snapshot());
    assert_ne!(owned_cell.attrs().background(), selected_bg);
    assert_ne!(owned_cell.attrs().intensity(), Intensity::Bold);
    assert_eq!(foreign_cell.attrs().background(), selected_bg);
    assert_eq!(foreign_cell.attrs().intensity(), Intensity::Bold);
    assert_eq!(foreign_cell.attrs().foreground(), ghost_fg);
}
