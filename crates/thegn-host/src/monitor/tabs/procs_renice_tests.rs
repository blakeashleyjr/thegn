use super::*;
use std::cell::Cell;
use termwiz::input::{KeyCode, Modifiers};

thread_local! {
    static NICE_VALUE: Cell<i32> = const { Cell::new(4) };
    static NICE_SET: Cell<Option<i32>> = const { Cell::new(None) };
    static SET_CALLS: Cell<usize> = const { Cell::new(0) };
    static DENY_SET: Cell<bool> = const { Cell::new(false) };
}

fn fake_get_nice(
    _pid: u32,
    _start: crate::platform::ProcStart,
) -> Result<i32, crate::platform::PriorityError> {
    Ok(NICE_VALUE.get())
}

fn fake_set_nice(
    _pid: u32,
    _start: crate::platform::ProcStart,
    nice: i32,
) -> Result<(), crate::platform::PriorityError> {
    NICE_SET.set(Some(nice));
    SET_CALLS.set(SET_CALLS.get() + 1);
    if DENY_SET.get() {
        Err(crate::platform::PriorityError::Denied(
            "Operation not permitted".into(),
        ))
    } else {
        Ok(())
    }
}

fn fake_get_ioprio(
    _pid: u32,
    _start: crate::platform::ProcStart,
) -> Result<crate::platform::IoClass, crate::platform::PriorityError> {
    Ok(crate::platform::IoClass::Idle)
}

fn fake_set_ioprio(
    _pid: u32,
    _start: crate::platform::ProcStart,
    _class: crate::platform::IoClass,
) -> Result<(), crate::platform::PriorityError> {
    Ok(())
}

fn install_fake_priority_calls() {
    TEST_PRIORITY_CALLS.set(Some(PriorityCalls {
        get_nice: fake_get_nice,
        set_nice: fake_set_nice,
        get_ioprio: fake_get_ioprio,
        set_ioprio: fake_set_ioprio,
    }));
    NICE_VALUE.set(4);
    NICE_SET.set(None);
    SET_CALLS.set(0);
    DENY_SET.set(false);
}

fn overlay() -> MonitorOverlay {
    let history = crate::telemetry::TelemetryHistory::default();
    let model = crate::chrome::FrameModel::default();
    let screen = crate::compositor::Rect::full(120, 40);
    let ctx = crate::detail::StatusCtx::new_for_test_on(&history, screen);
    let mut overlay = MonitorOverlay::open(
        super::super::super::MonitorTab::Procs,
        super::super::super::MonitorPrefs::default(),
        &model,
        &ctx,
    );
    overlay.proc_rows.push(ProcRow {
        pid: 4242,
        start_time: 17,
        start_ticks: Some(170),
        name: "worker".into(),
        owner: thegn_metrics::ProcOwner::Other,
        cpu_pct: 0.0,
        rss_bytes: 0,
        depth: 0,
        elided_parent: false,
    });
    overlay
}

fn key(overlay: &mut MonitorOverlay, key: KeyCode) {
    overlay.handle_key(&key, Modifiers::NONE);
}

#[test]
fn renice_prompt_adjusts_and_y_applies_through_the_injected_seam() {
    install_fake_priority_calls();
    let mut overlay = overlay();
    key(&mut overlay, KeyCode::Char('='));
    assert!(matches!(
        overlay.confirm,
        Some(Confirm::Renice {
            current: 4,
            target: 5,
            ..
        })
    ));
    key(&mut overlay, KeyCode::Char('-'));
    assert!(matches!(
        overlay.confirm,
        Some(Confirm::Renice { target: 4, .. })
    ));
    key(&mut overlay, KeyCode::Char('+'));
    key(&mut overlay, KeyCode::Char('+'));
    key(&mut overlay, KeyCode::Char('y'));
    assert_eq!(NICE_SET.get(), Some(6));
    assert_eq!(SET_CALLS.get(), 1);
}

#[test]
fn renice_adjustment_clamps_to_the_nice_limits() {
    install_fake_priority_calls();
    let mut overlay = overlay();
    key(&mut overlay, KeyCode::Char('='));
    for _ in 0..40 {
        key(&mut overlay, KeyCode::Char('+'));
    }
    assert!(matches!(
        overlay.confirm,
        Some(Confirm::Renice { target: 19, .. })
    ));
    for _ in 0..50 {
        key(&mut overlay, KeyCode::Char('-'));
    }
    assert!(matches!(
        overlay.confirm,
        Some(Confirm::Renice { target: -20, .. })
    ));
}

#[test]
fn cancel_and_identity_change_never_call_the_priority_backend() {
    install_fake_priority_calls();
    let mut cancelled = overlay();
    key(&mut cancelled, KeyCode::Char('='));
    key(&mut cancelled, KeyCode::Char('n'));
    assert!(cancelled.confirm.is_none());
    assert_eq!(SET_CALLS.get(), 0);

    let mut changed = overlay();
    key(&mut changed, KeyCode::Char('='));
    changed.proc_rows[0].start_time += 1;
    key(&mut changed, KeyCode::Char('y'));
    assert_eq!(SET_CALLS.get(), 0);
    assert!(
        changed
            .status
            .as_deref()
            .is_some_and(|status| status.contains("process changed"))
    );
}

#[test]
fn ionice_order_and_platform_visibility_follow_the_platform_constant() {
    use crate::platform::IoClass::{BestEffort, Idle, RealTime};
    assert_eq!(next_ioprio(Idle, true), BestEffort(0));
    assert_eq!(next_ioprio(BestEffort(7), true), RealTime(0));
    assert_eq!(next_ioprio(RealTime(0), false), BestEffort(7));
    assert_eq!(next_ioprio(BestEffort(0), false), Idle);

    let prefs = crate::monitor::MonitorPrefs::default();
    let input = crate::monitor::footer::FooterInput {
        tab: crate::monitor::MonitorTab::Procs,
        prefs: &prefs,
        confirm: None,
        filtering: false,
        filter: "",
        notice: None,
        status: None,
        paused: false,
        container_ours: false,
        disk_rows: 0,
        board_panels: 0,
        board_zoomed: false,
    };
    let mut hints = Vec::new();
    super::hints(&input, &mut hints);
    let hint_text = hints
        .iter()
        .flatten()
        .flat_map(|segment| segment.text.chars())
        .collect::<String>();
    assert!(hint_text.contains("= nice"));
    assert_eq!(
        hint_text.contains("I ionice"),
        crate::platform::IONICE_SUPPORTED
    );

    install_fake_priority_calls();
    let mut overlay = overlay();
    key(&mut overlay, KeyCode::Char('I'));
    if crate::platform::IONICE_SUPPORTED {
        assert!(matches!(overlay.confirm, Some(Confirm::Ionice { .. })));
    } else {
        assert!(overlay.confirm.is_none());
    }
}

#[test]
fn escape_cancels_the_renice_prompt() {
    install_fake_priority_calls();
    let mut overlay = overlay();
    key(&mut overlay, KeyCode::Char('='));
    key(&mut overlay, KeyCode::Escape);
    assert!(overlay.confirm.is_none());
    assert_eq!(SET_CALLS.get(), 0);
}

#[test]
fn denied_lower_nice_is_reported_once_without_retry() {
    install_fake_priority_calls();
    DENY_SET.set(true);
    let mut overlay = overlay();
    key(&mut overlay, KeyCode::Char('='));
    key(&mut overlay, KeyCode::Char('-'));
    key(&mut overlay, KeyCode::Char('-'));
    key(&mut overlay, KeyCode::Char('y'));
    assert_eq!(SET_CALLS.get(), 1);
    assert_eq!(
        overlay.status.as_deref(),
        Some("pid 4242 worker: lowering priority needs privileges (not retried)")
    );
}
