use super::view::{MemoryModel, MemoryView};
use crate::{
    compositor::Rect,
    monitor::board::{Board, TabView as _},
    telemetry::TelemetryHistory,
};
use crate::{
    detail::StatusCtx,
    monitor::{MonitorOverlay, MonitorPrefs, MonitorTab},
};
use thegn_metrics::StatsSnapshot;
use thegn_metrics::collect::{CollectorId, frame::MonitorFrame, model::cgroup::Limit};

#[test]
fn cgroup_limit_is_absorbed_and_demanded_by_its_placed_panel() {
    let mut frame = MonitorFrame::default();
    frame.parts.cgroup_slice.own_current = Some(4096);
    frame.parts.cgroup_slice.effective_limit = Some(Limit::Value(8192));
    frame.parts.cgroup_slice.sample.mem_current = Some(16_384);
    frame.parts.cgroup_slice.sample.mem_high = Some(Limit::Max);
    frame.parts.cgroup_slice.sample.mem_max = Some(Limit::Value(32_768));

    let mut model = MemoryModel::default();
    model.absorb_frame(&frame);
    assert_eq!(model.own_current, Some(4096));
    assert_eq!(model.effective_limit, Some(Limit::Value(8192)));
    assert_eq!(model.slice_current, Some(16_384));

    let view = MemoryView(&model);
    let board = Board::default();
    let placed = board.placed(&view, Rect::full(160, 45));
    let limit = placed
        .iter()
        .find(|panel| panel.id.0 == "memory.limit")
        .expect("limit panel is placed");
    assert!(view.demand(limit.id).contains(CollectorId::CgroupSlice));
}

#[test]
fn panel_demand_does_not_depend_on_current_readings() {
    let empty = MemoryModel::default();
    let mut populated = MemoryModel::default();
    let mut frame = MonitorFrame::default();
    thegn_metrics::collect::parse::meminfo::parse(
        b"MemTotal: 1 kB\nMemAvailable: 0 kB\n",
        &mut frame.parts.meminfo.raw,
    );
    populated.absorb_frame(&frame);
    let empty_view = MemoryView(&empty);
    let populated_view = MemoryView(&populated);
    for id in [
        "memory.composition",
        "memory.verdict",
        "memory.pressure",
        "memory.swap",
        "memory.paging",
        "memory.limit",
    ] {
        let panel = crate::monitor::layout::PanelId(id);
        assert_eq!(empty_view.demand(panel), populated_view.demand(panel));
    }
}

#[test]
fn overlay_rebuild_absorbs_memory_telemetry_and_pause_freezes_it() {
    let mut history = TelemetryHistory::default();
    for (tick, used, swap) in [(1_u64, 20.0, 10.0), (2, 45.0, 30.0), (3, 70.0, 55.0)] {
        history.push(
            &StatsSnapshot {
                mem_gib: Some((used, 100.0)),
                swap_gib: Some((swap, 100.0)),
                ..Default::default()
            },
            tick * 1000,
        );
    }
    let mut ctx = StatusCtx::new_for_test_on(&history, Rect::full(240, 60));
    ctx.now_ms = 3000;
    let frame = thegn_host_frame_model();
    let mut overlay =
        MonitorOverlay::open(MonitorTab::Memory, MonitorPrefs::default(), &frame, &ctx);
    assert_eq!(overlay.memory.history_state().0, 120);
    assert_eq!(overlay.memory.history_state().1, Some(1.0));

    overlay.paused = true;
    let frozen = overlay.memory.history_state();
    overlay.rebuild(&frame, &ctx, false);
    assert_eq!(overlay.memory.history_state(), frozen);
}

#[test]
fn telemetry_history_absorbs_into_fixed_capacity_memory_rings() {
    let mut history = TelemetryHistory::default();
    for tick in 1_u64..=3 {
        history.push(
            &StatsSnapshot {
                mem_gib: Some((tick as f32 * 10.0, 100.0)),
                swap_gib: Some((tick as f32 * 5.0, 100.0)),
                ..Default::default()
            },
            tick * 1000,
        );
    }
    let mut model = MemoryModel::default();
    for now_ms in 3000_u64..4000 {
        model.absorb_history(
            &history,
            crate::telemetry::Window::EVERYTHING,
            crate::telemetry::ScaleMode::Window,
            now_ms,
        );
    }
    let (len, latest, used_capacity, swap_capacity) = model.history_state();
    assert_eq!(len, 120);
    assert_eq!(latest, Some(1.0));
    assert_eq!(used_capacity, 120);
    assert_eq!(swap_capacity, 120);
}

fn thegn_host_frame_model() -> crate::chrome::FrameModel {
    crate::chrome::FrameModel {
        stats: StatsSnapshot {
            mem_gib: Some((70.0, 100.0)),
            swap_gib: Some((55.0, 100.0)),
            ..Default::default()
        },
        ..Default::default()
    }
}
