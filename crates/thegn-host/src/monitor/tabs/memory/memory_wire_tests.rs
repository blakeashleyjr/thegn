use super::view::{MemoryModel, MemoryView};
use crate::{
    compositor::Rect,
    monitor::board::{Board, TabView as _},
};
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
