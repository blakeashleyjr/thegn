use super::{CpuModel, USAGE};
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
    assert!(
        counts.allocation_calls <= PAINT_ALLOCATION_BUDGET
            && counts.realloc_calls <= PAINT_REALLOC_BUDGET,
        "CPU usage paint observed {} allocations ({} reallocations), budget {PAINT_ALLOCATION_BUDGET}: {counts:?}",
        counts.allocation_calls,
        counts.realloc_calls,
    );
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
