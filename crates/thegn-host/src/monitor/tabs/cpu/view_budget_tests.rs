use super::{CpuModel, SampleRing, USAGE};
use crate::{
    compositor::Rect,
    telemetry::{ScaleMode, TelemetryHistory, Window},
};
use termwiz::surface::Surface;
use thegn_metrics::{
    StatsSnapshot,
    collect::{
        frame::MonitorFrame,
        model::{
            cpu_freq::CpuFreqPart,
            cpu_policy::CpuPolicy,
            cpu_stat::{CoreSample, ModeShare},
        },
    },
};

fn varying_frame(iteration: usize) -> MonitorFrame {
    let mut frame = MonitorFrame::default();
    let cores = iteration % 2 + 1;
    for id in 0..cores {
        frame.parts.cpu_stat.cores.push(CoreSample {
            id: id as u16,
            online: true,
            share: Some(ModeShare {
                busy: ((iteration * 37 + id * 113) % 1000) as u16,
                iowait: (iteration % 100) as u16,
                ..Default::default()
            }),
        });
    }
    frame.parts.cpu_freq = CpuFreqPart {
        policies: (0..(iteration % 2 + 1))
            .map(|policy| CpuPolicy {
                policy: policy as u16,
                cur_khz: Some(2_000_000 + iteration as u64),
                hw_min_khz: Some(1_000_000),
                hw_max_khz: Some(3_000_000),
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    };
    frame
}

fn stats(iteration: usize) -> StatsSnapshot {
    StatsSnapshot {
        cpu_pct: Some((iteration % 100) as u8),
        load_avg: (iteration % 2 == 1).then_some((iteration as f32 / 10.0, 1.0, 0.5)),
        ..Default::default()
    }
}

fn paint_fixture() -> (CpuModel, TelemetryHistory) {
    let mut model = CpuModel::default();
    let mut history = TelemetryHistory::default();
    for second in 0..=60_u64 {
        let frame = varying_frame(second as usize);
        model.absorb_frame(&frame);
        history.push(&stats(second as usize), second * 1000);
    }
    model.absorb_history(&history, Window::from_secs(60), ScaleMode::Window, 60_000);
    (model, history)
}

#[test]
fn warmed_absorbs_stay_allocation_free_across_changing_frames_and_history() {
    let mut model = CpuModel::default();
    let mut history = TelemetryHistory::default();
    for iteration in 0..3 {
        let frame = varying_frame(iteration);
        model.absorb_frame(&frame);
        history.push(&stats(iteration), iteration as u64 * 1000);
        model.absorb_history(
            &history,
            Window::from_secs(60),
            ScaleMode::Window,
            iteration as u64 * 1000,
        );
    }
    let changing: Vec<_> = (3..103)
        .map(|iteration| (iteration, varying_frame(iteration), stats(iteration)))
        .collect();

    let (_, counts) = crate::proc_workload_alloc::measure(|| {
        for (iteration, frame, current) in &changing {
            model.absorb_frame(frame);
            history.push(current, *iteration as u64 * 1000);
            model.absorb_history(
                &history,
                Window::from_secs(60),
                ScaleMode::Window,
                *iteration as u64 * 1000,
            );
        }
    });
    assert_eq!(
        counts.allocation_calls, 0,
        "100 changing CPU frame/history absorbs allocated: {counts:?}"
    );
    assert_eq!(
        counts.realloc_calls, 0,
        "100 changing CPU frame/history absorbs reallocated: {counts:?}"
    );
}

#[test]
#[allow(
    clippy::disallowed_macros,
    reason = "budget gate prints observed counts for the primary"
)]
fn cpu_usage_paint_stays_within_its_pinned_allocation_budget() {
    // THE-898 regression ceiling; primary pins the reduced value from observed counts.
    const PAINT_ALLOCATION_BUDGET: u64 = 30_300;
    const PAINT_REALLOC_BUDGET: u64 = 550;
    let (model, _) = paint_fixture();
    let mut surface = Surface::new(240, 60);
    for _ in 0..3 {
        super::paint::paint(&model, &mut surface, USAGE, Rect::full(240, 60));
    }
    let (_, counts) = crate::proc_workload_alloc::measure(|| {
        super::paint::paint(&model, &mut surface, USAGE, Rect::full(240, 60));
    });
    eprintln!("CPU usage paint observed counts: {counts:?}");
    assert!(
        counts.allocation_calls <= PAINT_ALLOCATION_BUDGET
            && counts.realloc_calls <= PAINT_REALLOC_BUDGET,
        "CPU usage paint observed {} allocations ({} reallocations), budget {PAINT_ALLOCATION_BUDGET}: {counts:?}",
        counts.allocation_calls,
        counts.realloc_calls,
    );
}

#[test]
#[allow(
    clippy::disallowed_macros,
    reason = "phase budget gate prints observed counts for the primary"
)]
fn cpu_usage_paint_phases_report_counts() {
    const PAINT_ALLOCATION_BUDGET: u64 = 30_300;
    const PAINT_REALLOC_BUDGET: u64 = 550;
    let (model, _) = paint_fixture();
    let mut surface = Surface::new(240, 60);

    let mut grid_model = model.clone();
    grid_model.cpu_history.clear();
    grid_model.iowait_history.clear();
    grid_model.steal_history.clear();
    for _ in 0..3 {
        super::paint::paint(&grid_model, &mut surface, USAGE, Rect::full(240, 60));
    }
    let (_, grid_counts) = crate::proc_workload_alloc::measure(|| {
        super::paint::paint(&grid_model, &mut surface, USAGE, Rect::full(240, 60));
    });
    eprintln!("CPU usage grid and emit phase observed counts: {grid_counts:?}");
    assert!(
        grid_counts.allocation_calls <= PAINT_ALLOCATION_BUDGET
            && grid_counts.realloc_calls <= PAINT_REALLOC_BUDGET,
        "CPU usage grid and emit phase observed counts above total budget: {grid_counts:?}"
    );

    let mut history_model = model.clone();
    history_model.iowait_history.clear();
    history_model.steal_history.clear();
    for _ in 0..3 {
        super::paint::paint(&history_model, &mut surface, USAGE, Rect::full(240, 60));
    }
    let (_, history_counts) = crate::proc_workload_alloc::measure(|| {
        super::paint::paint(&history_model, &mut surface, USAGE, Rect::full(240, 60));
    });
    let history_delta = crate::proc_workload_alloc::Counts {
        allocation_calls: history_counts
            .allocation_calls
            .saturating_sub(grid_counts.allocation_calls),
        realloc_calls: history_counts
            .realloc_calls
            .saturating_sub(grid_counts.realloc_calls),
        ..Default::default()
    };
    eprintln!(
        "CPU usage history graph phase cumulative counts: {history_counts:?}; delta (history - grid): {history_delta:?}"
    );
    assert!(
        history_counts.allocation_calls <= PAINT_ALLOCATION_BUDGET
            && history_counts.realloc_calls <= PAINT_REALLOC_BUDGET,
        "CPU usage history graph phase observed counts above total budget: {history_counts:?}"
    );

    let mut spark_model = model;
    spark_model.cpu_history.clear();
    for _ in 0..3 {
        super::paint::paint(&spark_model, &mut surface, USAGE, Rect::full(240, 60));
    }
    let (_, spark_counts) = crate::proc_workload_alloc::measure(|| {
        super::paint::paint(&spark_model, &mut surface, USAGE, Rect::full(240, 60));
    });
    let spark_delta = crate::proc_workload_alloc::Counts {
        allocation_calls: spark_counts
            .allocation_calls
            .saturating_sub(grid_counts.allocation_calls),
        realloc_calls: spark_counts
            .realloc_calls
            .saturating_sub(grid_counts.realloc_calls),
        ..Default::default()
    };
    eprintln!(
        "CPU usage io/steal sparkline phase cumulative counts: {spark_counts:?}; delta (spark - grid): {spark_delta:?}"
    );
    assert!(
        spark_counts.allocation_calls <= PAINT_ALLOCATION_BUDGET
            && spark_counts.realloc_calls <= PAINT_REALLOC_BUDGET,
        "CPU usage io/steal sparkline phase observed counts above total budget: {spark_counts:?}"
    );
}

#[test]
#[allow(
    clippy::disallowed_macros,
    reason = "allocation gate prints observed counts for the primary"
)]
fn warmed_cpu_heatmap_and_usage_paints_report_allocations() {
    const PAINT_ALLOCATION_BUDGET: u64 = 30_300;
    const PAINT_REALLOC_BUDGET: u64 = 550;
    let (model, _) = paint_fixture();
    let mut surface = Surface::new(240, 60);
    for _ in 0..3 {
        super::paint::paint(&model, &mut surface, super::CORES, Rect::full(240, 60));
        super::paint::paint(&model, &mut surface, USAGE, Rect::full(240, 60));
    }
    let (_, heatmap_counts) = crate::proc_workload_alloc::measure(|| {
        super::paint::paint(&model, &mut surface, super::CORES, Rect::full(240, 60));
    });
    eprintln!("Warmed CPU heatmap paint observed counts: {heatmap_counts:?}");
    assert!(
        heatmap_counts.allocation_calls <= PAINT_ALLOCATION_BUDGET
            && heatmap_counts.realloc_calls <= PAINT_REALLOC_BUDGET,
        "warmed CPU heatmap paint observed counts above total budget: {heatmap_counts:?}"
    );

    let (_, usage_counts) = crate::proc_workload_alloc::measure(|| {
        super::paint::paint(&model, &mut surface, USAGE, Rect::full(240, 60));
    });
    eprintln!("Warmed CPU usage paint observed counts: {usage_counts:?}");
    assert!(
        usage_counts.allocation_calls <= PAINT_ALLOCATION_BUDGET
            && usage_counts.realloc_calls <= PAINT_REALLOC_BUDGET,
        "warmed CPU usage paint observed counts above total budget: {usage_counts:?}"
    );
}

#[test]
fn reused_cpu_paint_buffers_match_fresh_surfaces_after_rows_shrink() {
    use thegn_metrics::collect::model::{
        cpu_idle::{CpuIdleCore, CpuIdleState},
        cpu_sched::CoreSchedSample,
        cpu_stat::{CoreSample, ModeShare},
    };

    fn model_with_core_count(count: usize) -> CpuModel {
        let mut model = CpuModel::default();
        let mut frame = thegn_metrics::collect::frame::MonitorFrame::default();
        frame.parts.cpu_stat.cores = (0..count)
            .map(|id| CoreSample {
                id: id as u16,
                online: true,
                share: Some(ModeShare {
                    busy: (id as u16 + 1) * 100,
                    ..Default::default()
                }),
            })
            .collect();
        model.absorb_frame(&frame);
        model
    }

    fn assert_reused_matches_fresh(
        long: &CpuModel,
        short: &CpuModel,
        panel: crate::monitor::layout::PanelId,
    ) {
        let rect = Rect::full(80, 12);
        let mut reused = Surface::new(80, 12);
        super::paint::paint(long, &mut reused, panel, rect);
        super::paint::paint(short, &mut reused, panel, rect);

        let mut fresh = Surface::new(80, 12);
        for _ in 0..3 {
            super::paint::paint(short, &mut fresh, panel, rect);
        }
        assert_eq!(
            reused.screen_chars_to_string(),
            fresh.screen_chars_to_string(),
            "{panel:?} paint retained stale text after rows shrank"
        );
    }

    let mut long = model_with_core_count(2);
    let mut short = model_with_core_count(1);
    long.cpu_sched.cores = (0..2)
        .map(|id| CoreSchedSample {
            id,
            online: true,
            run_queue_wait_ms_per_s: Some(f64::from(id) + 0.5),
            ..Default::default()
        })
        .collect();
    short.cpu_sched.cores = long.cpu_sched.cores[..1].to_vec();
    long.idle.cores = (0..2)
        .map(|cpu| CpuIdleCore {
            cpu,
            c0_pct: Some(25.0),
            states: vec![CpuIdleState {
                name: Some("C1-long".into()),
                residency_pct: Some(75.0),
                ..Default::default()
            }],
        })
        .collect();
    short.idle.cores = long.idle.cores[..1].to_vec();

    assert_reused_matches_fresh(&long, &short, super::CORES);
    assert_reused_matches_fresh(&long, &short, super::SCHED);
    assert_reused_matches_fresh(&long, &short, super::IDLE);
}

#[test]
fn copy_ordered_into_matches_ordered_and_retains_capacity() {
    for values in [Vec::new(), (0..17).map(|value| value as f32).collect()] {
        let mut ring = SampleRing::default();
        for value in values {
            ring.push(value);
        }
        let mut out = Vec::new();
        ring.copy_ordered_into(&mut out);
        assert_eq!(out, ring.ordered());
        let capacity = out.capacity();
        for _ in 0..1_000 {
            ring.copy_ordered_into(&mut out);
            assert_eq!(out, ring.ordered());
            assert_eq!(out.capacity(), capacity);
        }
    }

    let mut wrapped = SampleRing::default();
    for value in 0..(super::CORE_HISTORY_SAMPLES + 5) {
        wrapped.push(value as f32);
    }
    let mut out = Vec::new();
    wrapped.copy_ordered_into(&mut out);
    assert_eq!(out, wrapped.ordered());
    let capacity = out.capacity();
    for _ in 0..1_000 {
        wrapped.copy_ordered_into(&mut out);
        assert_eq!(out, wrapped.ordered());
        assert_eq!(out.capacity(), capacity);
    }
}

#[test]
#[ignore = "controlled performance investigation; machine-dependent, opt-in only"]
#[allow(
    clippy::disallowed_macros,
    reason = "opt-in timing sample prints medians"
)]
fn monitor_view_warm_timings_report_medians() {
    use std::time::Instant;

    use crate::{
        monitor::{
            board::TabView,
            layout::{PanelId, Placed, VariantKind},
            tabs::overview::{OverviewModel, OverviewView},
        },
        sections::Section,
    };

    fn median(samples: &mut [u128]) -> u128 {
        samples.sort_unstable();
        samples[samples.len() / 2]
    }

    let mut overview = OverviewModel::default();
    let mut overview_history = TelemetryHistory::default();
    let mut cpu = CpuModel::default();
    let mut cpu_history = TelemetryHistory::default();
    for second in 0..=60_u64 {
        let current = stats(second as usize);
        let current_frame = varying_frame(second as usize);
        overview.absorb_frame(&current_frame);
        overview.absorb_stats(&current);
        overview_history.push(&current, second * 1000);
        cpu.absorb_frame(&current_frame);
        cpu_history.push(&current, second * 1000);
    }
    let changing: Vec<_> = (0..101)
        .map(|iteration| (varying_frame(iteration), stats(iteration)))
        .collect();
    overview.absorb_history(
        &overview_history,
        Window::from_secs(60),
        ScaleMode::Window,
        60_000,
    );
    cpu.absorb_history(
        &cpu_history,
        Window::from_secs(60),
        ScaleMode::Window,
        60_000,
    );

    let area = Rect::full(240, 60);
    let placed = Placed {
        id: PanelId("overview.cpu"),
        rect: area,
        variant: VariantKind::Full,
    };
    let mut overview_surface = Surface::new(240, 60);
    let mut cpu_surface = Surface::new(240, 60);
    for _ in 0..3 {
        OverviewView(&overview).paint(&mut overview_surface, &placed, area, 0, &[] as &[Section]);
        super::paint::paint(&cpu, &mut cpu_surface, USAGE, area);
    }

    let mut overview_rebuild_ns = Vec::with_capacity(101);
    let mut overview_paint_ns = Vec::with_capacity(101);
    let mut cpu_rebuild_ns = Vec::with_capacity(101);
    let mut cpu_paint_ns = Vec::with_capacity(101);
    for (iteration, (current_frame, current)) in changing.iter().enumerate() {
        let started = Instant::now();
        overview.absorb_frame(current_frame);
        overview.absorb_stats(current);
        overview.absorb_history(
            &overview_history,
            Window::from_secs(60),
            ScaleMode::Window,
            60_000 + iteration as u64,
        );
        overview_rebuild_ns.push(started.elapsed().as_nanos());

        let started = Instant::now();
        OverviewView(&overview).paint(&mut overview_surface, &placed, area, 0, &[] as &[Section]);
        overview_paint_ns.push(started.elapsed().as_nanos());

        let started = Instant::now();
        cpu.absorb_frame(current_frame);
        cpu.absorb_history(
            &cpu_history,
            Window::from_secs(60),
            ScaleMode::Window,
            60_000 + iteration as u64,
        );
        cpu_rebuild_ns.push(started.elapsed().as_nanos());

        let started = Instant::now();
        super::paint::paint(&cpu, &mut cpu_surface, USAGE, area);
        cpu_paint_ns.push(started.elapsed().as_nanos());
    }
    eprintln!(
        "monitor medians at 240x60 (reference only; budgets 2 ms rebuild / 0.3 ms paint): overview rebuild={}ns paint={}ns; CPU rebuild={}ns paint={}ns",
        median(&mut overview_rebuild_ns),
        median(&mut overview_paint_ns),
        median(&mut cpu_rebuild_ns),
        median(&mut cpu_paint_ns),
    );
}
