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
use thegn_metrics::{
    StatsSnapshot,
    collect::{
        frame::MonitorFrame,
        model::cpu_stat::{CoreSample, ModeShare},
    },
};

fn stats(iteration: usize) -> StatsSnapshot {
    StatsSnapshot {
        cpu_pct: Some((iteration % 100) as u8),
        mem_gib: Some((4.0 + (iteration % 8) as f32, 16.0)),
        net_bps: Some((iteration as u64 * 1024, iteration as u64 * 512)),
        disk_io: Some((iteration as u64 * 4096, iteration as u64 * 2048)),
        load_avg: Some((iteration as f32 / 10.0, 1.0, 0.5)),
        ..Default::default()
    }
}

fn frame(iteration: usize) -> MonitorFrame {
    let mut frame = MonitorFrame::default();
    for id in 0..(iteration % 2 + 1) {
        frame.parts.cpu_stat.cores.push(CoreSample {
            id: id as u16,
            online: true,
            share: Some(ModeShare {
                busy: ((iteration * 37 + id * 113) % 1000) as u16,
                ..Default::default()
            }),
        });
    }
    frame
}

fn populated_overview() -> (OverviewModel, TelemetryHistory) {
    let mut model = OverviewModel::default();
    let mut history = TelemetryHistory::default();
    for second in 0..=60_u64 {
        let current = stats(second as usize);
        model.absorb_stats(&current);
        model.absorb_frame(&frame(second as usize));
        history.push(&current, second * 1000);
    }
    model.absorb_history(&history, Window::from_secs(60), ScaleMode::Window, 60_000);
    (model, history)
}

#[test]
fn warmed_absorbs_stay_allocation_free_across_changing_frames_and_history() {
    let mut model = OverviewModel::default();
    let mut history = TelemetryHistory::default();
    for iteration in 0..3 {
        let current = stats(iteration);
        model.absorb_frame(&frame(iteration));
        model.absorb_stats(&current);
        history.push(&current, iteration as u64 * 1000);
        model.absorb_history(
            &history,
            Window::from_secs(60),
            ScaleMode::Window,
            iteration as u64 * 1000,
        );
    }
    let changing: Vec<_> = (3..103)
        .map(|iteration| (iteration, frame(iteration), stats(iteration)))
        .collect();

    let (_, counts) = crate::proc_workload_alloc::measure(|| {
        for (iteration, current_frame, current) in &changing {
            model.absorb_frame(current_frame);
            model.absorb_stats(current);
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
        "100 changing Overview frame/stats/history absorbs allocated: {counts:?}"
    );
    assert_eq!(
        counts.realloc_calls, 0,
        "100 changing Overview frame/stats/history absorbs reallocated: {counts:?}"
    );
}

#[test]
fn populated_overview_panels_stay_within_the_pinned_paint_budget() {
    // THE-898 regression ceiling; primary pins the reduced value from observed counts.
    const PAINT_ALLOCATION_BUDGET: u64 = 160_200;
    const PAINT_REALLOC_BUDGET: u64 = 2_400;
    let (model, _) = populated_overview();
    let view = super::OverviewView(&model);
    let area = Rect::full(240, 60);
    let mut surface = Surface::new(240, 60);
    let panels = [
        "overview.cpu",
        "overview.load",
        "overview.memory",
        "overview.network",
        "overview.disk",
        "overview.thermal",
        "overview.power",
        "overview.activity",
    ];
    let placed: Vec<_> = panels
        .iter()
        .map(|id| Placed {
            id: PanelId(id),
            rect: area,
            variant: VariantKind::Full,
        })
        .collect();
    for _ in 0..3 {
        for panel in &placed {
            view.paint(&mut surface, panel, area, 0, &[] as &[Section]);
        }
    }
    let (_, counts) = crate::proc_workload_alloc::measure(|| {
        for panel in &placed {
            view.paint(&mut surface, panel, area, 0, &[] as &[Section]);
        }
    });
    assert!(
        counts.allocation_calls <= PAINT_ALLOCATION_BUDGET
            && counts.realloc_calls <= PAINT_REALLOC_BUDGET,
        "Overview eight-panel paint observed {} allocations ({} reallocations), budget {PAINT_ALLOCATION_BUDGET}: {counts:?}",
        counts.allocation_calls,
        counts.realloc_calls,
    );
}

#[test]
fn consecutive_overview_paints_reuse_the_style_cache() {
    let model = OverviewModel::default();
    let view = super::OverviewView(&model);
    let area = Rect::full(80, 20);
    let placed = Placed {
        id: PanelId("overview.cpu"),
        rect: area,
        variant: VariantKind::Full,
    };
    let mut surface = Surface::new(80, 20);
    crate::caps::test_override::with_color(thegn_core::termcaps::ColorDepth::Truecolor, || {
        view.paint(&mut surface, &placed, area, 0, &[] as &[Section]);
        let after_first = super::PAINT_SCRATCH.with(|scratch| scratch.borrow().emitter.rebuilds());
        view.paint(&mut surface, &placed, area, 0, &[] as &[Section]);
        let after_second = super::PAINT_SCRATCH.with(|scratch| scratch.borrow().emitter.rebuilds());
        assert_eq!(after_second, after_first);
    });
}
