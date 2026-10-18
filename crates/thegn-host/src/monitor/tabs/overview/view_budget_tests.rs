use super::OverviewModel;
use crate::{
    compositor::Rect,
    monitor::{
        board::TabView,
        layout::{PanelId, Placed, VariantKind},
    },
    sections::Section,
    telemetry::{ScaleMode, TelemetryHistory, Window},
};
use termwiz::surface::Surface;
use thegn_metrics::{StatsSnapshot, collect::frame::MonitorFrame};

#[test]
fn warmed_absorb_frame_and_history_allocate_nothing() {
    let mut model = OverviewModel::default();
    let mut frame = MonitorFrame::default();
    frame
        .parts
        .cpu_stat
        .cores
        .push(thegn_metrics::collect::model::cpu_stat::CoreSample {
            id: 0,
            online: true,
            share: Some(thegn_metrics::collect::model::cpu_stat::ModeShare {
                busy: 400,
                ..Default::default()
            }),
            ..Default::default()
        });
    model.absorb_frame(&frame);
    let (_, frame_counts) = crate::proc_workload_alloc::measure(|| model.absorb_frame(&frame));
    assert_eq!(
        frame_counts.allocation_calls, 0,
        "warmed absorb_frame allocated: {frame_counts:?}"
    );
    assert_eq!(
        frame_counts.realloc_calls, 0,
        "warmed absorb_frame reallocated: {frame_counts:?}"
    );

    let stats = StatsSnapshot {
        cpu_pct: Some(40),
        ..Default::default()
    };
    let mut history = TelemetryHistory::default();
    for second in 0..=60_u64 {
        history.push(&stats, second * 1000);
    }
    model.absorb_history(&history, Window::from_secs(60), ScaleMode::Window, 60_000);
    let (_, history_counts) = crate::proc_workload_alloc::measure(|| {
        model.absorb_history(&history, Window::from_secs(60), ScaleMode::Window, 60_000);
    });
    assert_eq!(
        history_counts.allocation_calls, 0,
        "warmed absorb_history allocated: {history_counts:?}"
    );
    assert_eq!(
        history_counts.realloc_calls, 0,
        "warmed absorb_history reallocated: {history_counts:?}"
    );
}

#[test]
fn overview_cpu_paint_stays_within_its_pinned_allocation_budget() {
    // Panel formatting currently creates owned display strings on each paint.
    const PAINT_ALLOCATION_BUDGET: u64 = 64;
    let model = OverviewModel::default();
    let view = super::OverviewView(&model);
    let area = Rect::full(80, 20);
    let placed = Placed {
        id: PanelId("overview.cpu"),
        rect: area,
        variant: VariantKind::Full,
    };
    let mut surface = Surface::new(160, 45);
    view.paint(&mut surface, &placed, area, 0, &[] as &[Section]);
    view.paint(&mut surface, &placed, area, 0, &[] as &[Section]);
    let (_, counts) = crate::proc_workload_alloc::measure(|| {
        view.paint(&mut surface, &placed, area, 0, &[] as &[Section]);
    });
    assert!(
        counts.allocation_calls <= PAINT_ALLOCATION_BUDGET,
        "Overview CPU paint exceeded {PAINT_ALLOCATION_BUDGET} allocations: {counts:?}"
    );
}
