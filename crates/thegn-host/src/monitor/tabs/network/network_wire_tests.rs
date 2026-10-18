use super::view::{NetworkModel, NetworkView};
use crate::{
    compositor::Rect,
    detail::StatusCtx,
    monitor::{MonitorOverlay, MonitorPrefs, MonitorTab, board::TabView},
    telemetry::{ScaleMode, TelemetryHistory, Window},
};
use thegn_metrics::{
    StatsSnapshot,
    collect::{CollectorId, frame::MonitorFrame},
};

fn network_overlay(history: &TelemetryHistory) -> MonitorOverlay {
    let model = crate::chrome::FrameModel {
        stats: StatsSnapshot {
            net_bps: Some((100, 50)),
            ..StatsSnapshot::default()
        },
        ..crate::chrome::FrameModel::default()
    };
    let mut ctx = StatusCtx::new_for_test_on(history, Rect::full(240, 60));
    ctx.now_ms = 121_000;
    MonitorOverlay::open(MonitorTab::Network, MonitorPrefs::default(), &model, &ctx)
}

fn telemetry_sample(rx: u64, tx: u64) -> StatsSnapshot {
    StatsSnapshot {
        net_bps: Some((rx, tx)),
        ..StatsSnapshot::default()
    }
}

#[test]
fn network_tab_uses_the_board_and_demands_placed_netdev_panels() {
    let history = TelemetryHistory::default();
    let overlay = network_overlay(&history);
    assert_eq!(overlay.tab(), MonitorTab::Network);
    let view = overlay.view();
    let placed = overlay.board.placed(&view, overlay.body_rect());
    assert!(!placed.is_empty());
    let mut surface = termwiz::surface::Surface::new(240, 60);
    overlay
        .board
        .paint(&view, &mut surface, overlay.body_rect(), 0, &[]);
    let rendered = surface
        .screen_lines()
        .iter()
        .map(|line| {
            line.visible_cells()
                .map(|cell| cell.str().to_owned())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    for title in ["Network throughput", "Interfaces", "Network health"] {
        assert!(
            rendered.contains(title),
            "missing board panel {title}: {rendered}"
        );
    }
    let demand = overlay.demand(1000);
    assert!(demand.collectors.contains(CollectorId::Netdev));
    assert_eq!(demand.collectors.iter().count(), 1);
    let expected = placed.iter().fold(
        thegn_metrics::collect::CollectorSet::empty(),
        |mut set, panel| {
            for collector in view.demand(panel.id).iter() {
                set.insert(collector);
            }
            set
        },
    );
    assert_eq!(demand.collectors, expected);
}

#[test]
fn visible_network_ingest_updates_and_pause_freezes_the_view_model() {
    let history = TelemetryHistory::default();
    let mut overlay = network_overlay(&history);
    let frame = MonitorFrame {
        health: {
            let mut health = MonitorFrame::default().health;
            health[CollectorId::Netdev.index()] = thegn_metrics::collect::health::Health::Ok;
            health
        },
        ..MonitorFrame::default()
    };
    assert!(overlay.ingest_frame(&frame));
    let demand = overlay.demand(1000);
    assert!(demand.collectors.contains(CollectorId::Netdev));
    overlay.handle_key(
        &termwiz::input::KeyCode::Char(' '),
        termwiz::input::Modifiers::NONE,
    );
    let mut changed = MonitorFrame::default();
    changed.health[CollectorId::Netdev.index()] = thegn_metrics::collect::health::Health::Absent(
        thegn_metrics::collect::health::Absent::NotImplemented,
    );
    assert!(!overlay.ingest_frame(&changed));
    assert_eq!(
        overlay.network.health,
        thegn_metrics::collect::health::Health::Ok
    );
    assert_eq!(overlay.demand(1000).collectors, demand.collectors);
}

#[test]
fn telemetry_history_is_separate_bounded_and_right_aligned() {
    let mut history = TelemetryHistory::default();
    for tick in 0..4_u64 {
        history.push(
            &StatsSnapshot {
                net_bps: Some((2_000 + tick * 700, 900 + tick * 300)),
                ..StatsSnapshot::default()
            },
            1_000 + tick * 1_000,
        );
    }
    let mut model = NetworkModel::default();
    model.absorb_history(&history, Window::from_secs(10), ScaleMode::Window, 5_000);
    assert_eq!(model.history_rx.len, 432);
    assert_eq!(model.history_tx.len, 432);
    let rx_capacity = model.history_rx.values.capacity();
    let tx_capacity = model.history_tx.values.capacity();
    for _ in 0..1_000 {
        model.absorb_history(&history, Window::from_secs(10), ScaleMode::Window, 5_000);
    }
    assert_eq!(model.history_rx.values.capacity(), rx_capacity);
    assert_eq!(model.history_tx.values.capacity(), tx_capacity);

    let view = NetworkView(&model);
    assert!(
        view.demand(super::view::THROUGHPUT)
            .contains(CollectorId::Netdev)
    );
}

#[test]
fn rebuild_absorbs_telemetry_and_pause_keeps_the_last_series() {
    let mut history = TelemetryHistory::default();
    history.push(&telemetry_sample(10_000, 4_000), 115_000);
    history.push(&telemetry_sample(20_000, 8_000), 119_000);
    let model = crate::chrome::FrameModel {
        stats: StatsSnapshot {
            net_bps: Some((20_000, 8_000)),
            ..StatsSnapshot::default()
        },
        ..crate::chrome::FrameModel::default()
    };
    let mut ctx = StatusCtx::new_for_test_on(&history, Rect::full(240, 60));
    ctx.now_ms = 121_000;
    let mut overlay =
        MonitorOverlay::open(MonitorTab::Network, MonitorPrefs::default(), &model, &ctx);
    assert!(overlay.network.history_rx.len > 0);
    let mut before = [0.0; super::view::TELEMETRY_HISTORY];
    let before_len = overlay.network.history_rx.copy_ordered(&mut before);

    let mut newer_history = TelemetryHistory::default();
    newer_history.push(&telemetry_sample(10_000, 4_000), 115_000);
    newer_history.push(&telemetry_sample(20_000, 8_000), 119_000);
    newer_history.push(&telemetry_sample(50_000, 15_000), 120_500);
    let mut newer_ctx = StatusCtx::new_for_test_on(&newer_history, Rect::full(240, 60));
    newer_ctx.now_ms = 121_000;

    let mut fresh_model = NetworkModel::default();
    fresh_model.absorb_history(
        &newer_history,
        Window::default(),
        ScaleMode::default(),
        121_000,
    );
    let mut fresh = [0.0; super::view::TELEMETRY_HISTORY];
    let fresh_len = fresh_model.history_rx.copy_ordered(&mut fresh);
    assert_ne!(&fresh[..fresh_len], &before[..before_len]);

    overlay.handle_key(
        &termwiz::input::KeyCode::Char(' '),
        termwiz::input::Modifiers::NONE,
    );
    overlay.rebuild_after_key(&model, &newer_ctx);
    let mut after = [0.0; super::view::TELEMETRY_HISTORY];
    let after_len = overlay.network.history_rx.copy_ordered(&mut after);
    assert_eq!(&after[..after_len], &before[..before_len]);
}
