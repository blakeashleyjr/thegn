use crate::{
    chrome::FrameModel,
    detail::StatusCtx,
    monitor::{MonitorOutcome, MonitorOverlay, MonitorPrefs, MonitorTab},
};
use termwiz::input::{KeyCode, Modifiers};
use thegn_metrics::collect::{frame::MonitorFrame, model::cpu_stat::CoreSample};

fn overlay() -> MonitorOverlay {
    let history = crate::telemetry::TelemetryHistory::default();
    let model = FrameModel::default();
    let screen = crate::compositor::Rect::full(120, 40);
    let ctx = StatusCtx::new_for_test_on(&history, screen);
    let mut overlay = MonitorOverlay::open(MonitorTab::Cpu, MonitorPrefs::default(), &model, &ctx);
    let mut frame = MonitorFrame::default();
    frame.parts.cpu_stat.cores = (0..3)
        .map(|id| CoreSample {
            id,
            online: true,
            ..CoreSample::default()
        })
        .collect();
    overlay.cpu.absorb_frame(&frame);
    overlay
}

fn key(overlay: &mut MonitorOverlay, key: KeyCode) -> MonitorOutcome {
    overlay.handle_key(&key, Modifiers::NONE)
}

#[test]
fn cpu_cursor_navigates_clamps_and_end_selects_last_core() {
    let mut overlay = overlay();
    assert_eq!(overlay.sel, 0);
    key(&mut overlay, KeyCode::Char('j'));
    assert_eq!(overlay.sel, 1);
    key(&mut overlay, KeyCode::Char('j'));
    key(&mut overlay, KeyCode::Char('j'));
    assert_eq!(overlay.sel, 2);
    key(&mut overlay, KeyCode::End);
    assert_eq!(overlay.sel, 2);
}

#[test]
fn enter_toggles_core_detail_only_on_cpu_and_tab_switch_resets_cursor() {
    let mut overlay = overlay();
    key(&mut overlay, KeyCode::End);
    assert_eq!(overlay.sel, 2);
    assert_eq!(key(&mut overlay, KeyCode::Enter), MonitorOutcome::Pending);
    assert!(overlay.cpu.core_detail());
    assert_eq!(key(&mut overlay, KeyCode::Enter), MonitorOutcome::Pending);
    assert!(!overlay.cpu.core_detail());

    key(&mut overlay, KeyCode::Tab);
    assert_ne!(overlay.tab, MonitorTab::Cpu);
    key(&mut overlay, KeyCode::Enter);
    assert!(!overlay.cpu.core_detail());
    assert_eq!(overlay.sel, 0);
}
