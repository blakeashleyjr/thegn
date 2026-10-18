use super::{CpuModel, USAGE};
use crate::{
    compositor::Rect,
    telemetry::{ScaleMode, TelemetryHistory, Window},
};
use termwiz::surface::Surface;
use thegn_metrics::{
    StatsSnapshot,
    collect::{frame::MonitorFrame, model::cpu_stat::CoreSample},
};

fn fixture() -> (MonitorFrame, TelemetryHistory) {
    let mut frame = MonitorFrame::default();
    frame.parts.cpu_stat.cores.push(CoreSample {
        id: 0,
        online: true,
        ..Default::default()
    });
    let mut history = TelemetryHistory::default();
    for second in 0..=60_u64 {
        history.push(
            &StatsSnapshot {
                cpu_pct: Some((second % 100) as u8),
                ..Default::default()
            },
            second * 1000,
        );
    }
    (frame, history)
}

#[test]
fn warmed_absorb_frame_and_history_allocate_nothing() {
    let (frame, history) = fixture();
    let mut model = CpuModel::default();
    model.absorb_frame(&frame);
    model.absorb_history(&history, Window::from_secs(60), ScaleMode::Window, 60_000);

    let (_, frame_counts) = crate::proc_workload_alloc::measure(|| model.absorb_frame(&frame));
    assert_eq!(
        frame_counts.allocation_calls, 0,
        "warmed absorb_frame allocated: {frame_counts:?}"
    );
    assert_eq!(
        frame_counts.realloc_calls, 0,
        "warmed absorb_frame reallocated: {frame_counts:?}"
    );

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
fn cpu_usage_paint_stays_within_its_pinned_allocation_budget() {
    // The usage plot includes the shared graph renderer's current scratch use.
    const PAINT_ALLOCATION_BUDGET: u64 = 64;
    let (frame, history) = fixture();
    let mut model = CpuModel::default();
    model.absorb_frame(&frame);
    model.absorb_history(&history, Window::from_secs(60), ScaleMode::Window, 60_000);
    let mut surface = Surface::new(160, 45);
    super::paint::paint(&model, &mut surface, USAGE, Rect::full(80, 30));
    super::paint::paint(&model, &mut surface, USAGE, Rect::full(80, 30));
    let (_, counts) = crate::proc_workload_alloc::measure(|| {
        super::paint::paint(&model, &mut surface, USAGE, Rect::full(80, 30));
    });
    assert!(
        counts.allocation_calls <= PAINT_ALLOCATION_BUDGET,
        "CPU usage paint exceeded {PAINT_ALLOCATION_BUDGET} allocations: {counts:?}"
    );
}

#[test]
#[ignore = "opt-in monitor view timing sample; run with --ignored --nocapture"]
fn monitor_view_warm_timings_report_medians() {
    use std::time::Instant;

    fn median(samples: &mut [u128]) -> u128 {
        samples.sort_unstable();
        samples[samples.len() / 2]
    }

    let (frame, history) = fixture();
    let mut model = CpuModel::default();
    model.absorb_frame(&frame);
    model.absorb_history(&history, Window::from_secs(60), ScaleMode::Window, 60_000);
    let mut surface = Surface::new(160, 45);
    super::paint::paint(&model, &mut surface, USAGE, Rect::full(80, 30));

    let mut rebuild_ns = Vec::with_capacity(101);
    let mut paint_ns = Vec::with_capacity(101);
    for _ in 0..101 {
        let started = Instant::now();
        model.absorb_history(&history, Window::from_secs(60), ScaleMode::Window, 60_000);
        rebuild_ns.push(started.elapsed().as_nanos());

        let started = Instant::now();
        super::paint::paint(&model, &mut surface, USAGE, Rect::full(80, 30));
        paint_ns.push(started.elapsed().as_nanos());
    }
    tracing::info!(
        rebuild_median_ns = median(&mut rebuild_ns),
        paint_median_ns = median(&mut paint_ns),
        "CPU monitor view warm timing sample"
    );
}
