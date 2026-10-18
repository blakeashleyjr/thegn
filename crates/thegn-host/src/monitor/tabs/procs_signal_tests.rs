use super::*;
use std::cell::Cell;
use termwiz::input::{KeyCode, Modifiers};

thread_local! {
    static SIGNAL_SENT: Cell<Option<crate::platform::ProcSignal>> = const { Cell::new(None) };
}

fn fake_signal(
    _pid: u32,
    _start: crate::platform::ProcStart,
    signal: crate::platform::ProcSignal,
) -> Result<(), crate::platform::SignalError> {
    SIGNAL_SENT.set(Some(signal));
    Ok(())
}

fn overlay() -> MonitorOverlay {
    let history = crate::telemetry::TelemetryHistory::default();
    let model = crate::chrome::FrameModel::default();
    let screen = crate::compositor::Rect::full(120, 40);
    let ctx = crate::detail::StatusCtx::new_for_test_on(&history, screen);
    let mut overlay = MonitorOverlay::open(
        crate::monitor::MonitorTab::Procs,
        crate::monitor::MonitorPrefs::default(),
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
fn x_opens_menu_and_non_destructive_letter_delivers_immediately_through_seam() {
    set_test_signal_call(fake_signal);
    SIGNAL_SENT.set(None);
    let mut overlay = overlay();
    key(&mut overlay, KeyCode::Char('X'));
    assert!(matches!(
        overlay.confirm,
        Some(Confirm::SignalMenu { pid: 4242, .. })
    ));
    let signal =
        if crate::platform::ProcSignal::menu().contains(&crate::platform::ProcSignal::Hangup) {
            key(&mut overlay, KeyCode::Char('h'));
            crate::platform::ProcSignal::Hangup
        } else {
            key(&mut overlay, KeyCode::Char('t'));
            crate::platform::ProcSignal::Terminate
        };
    assert!(overlay.confirm.is_none());
    assert_eq!(SIGNAL_SENT.get(), Some(signal));
    let sent_text = format!("SIG{}", signal.label());
    assert!(
        overlay
            .status
            .as_deref()
            .is_some_and(|s| s.contains(sent_text.as_str()))
    );
}

#[test]
fn kill_and_stop_open_the_identity_checked_confirmation() {
    for signal in [
        crate::platform::ProcSignal::Kill,
        crate::platform::ProcSignal::Stop,
    ] {
        if !crate::platform::ProcSignal::menu().contains(&signal) {
            continue;
        }
        let mut overlay = overlay();
        key(&mut overlay, KeyCode::Char('X'));
        key(&mut overlay, KeyCode::Char(signal.menu_key()));
        assert!(matches!(
            overlay.confirm,
            Some(Confirm::Signal { stage, .. }) if stage == signal
        ));
        SIGNAL_SENT.set(None);
        key(&mut overlay, KeyCode::Char('y'));
        assert_eq!(SIGNAL_SENT.get(), Some(signal));
    }
}

#[test]
fn menu_ignores_y_cancels_and_arrows_select_the_third_entry() {
    let mut overlay = overlay();
    key(&mut overlay, KeyCode::Char('X'));
    key(&mut overlay, KeyCode::Char('y'));
    assert!(matches!(
        overlay.confirm,
        Some(Confirm::SignalMenu { cursor: 0, .. })
    ));
    key(&mut overlay, KeyCode::Escape);
    assert!(overlay.confirm.is_none());
    assert_eq!(overlay.status.as_deref(), Some("cancelled"));

    key(&mut overlay, KeyCode::Char('X'));
    key(&mut overlay, KeyCode::DownArrow);
    key(&mut overlay, KeyCode::DownArrow);
    key(&mut overlay, KeyCode::Enter);
    let third_or_last =
        crate::platform::ProcSignal::menu()[2.min(crate::platform::ProcSignal::menu().len() - 1)];
    assert!(matches!(
        overlay.confirm,
        Some(Confirm::Signal { stage, .. }) if stage == third_or_last
    ));
}

#[test]
fn missing_selection_does_not_open_and_vanished_identity_is_refused() {
    let mut empty = overlay();
    empty.proc_rows.clear();
    key(&mut empty, KeyCode::Char('X'));
    assert!(empty.confirm.is_none());

    let mut vanished = overlay();
    key(&mut vanished, KeyCode::Char('X'));
    vanished.proc_rows.clear();
    key(&mut vanished, KeyCode::Char('t'));
    assert!(vanished.confirm.is_none());
    assert!(vanished.status.as_deref().is_some_and(|s| {
        s.contains("process changed or left the sampled list; select it again")
    }));
}

#[test]
fn footer_names_menu_entries_and_non_legacy_signal_confirmation() {
    let prefs = crate::monitor::MonitorPrefs::default();
    let footer_input = crate::monitor::footer::FooterInput {
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
    super::hints(&footer_input, &mut hints);
    let hint_text = hints
        .iter()
        .flatten()
        .flat_map(|segment| segment.text.chars())
        .collect::<String>();
    assert!(hint_text.contains("X menu"));

    let menu_confirm = Confirm::SignalMenu {
        pid: 4242,
        start_time: 17,
        start_ticks: Some(170),
        label: "pid 4242 worker".into(),
        cursor: 0,
    };
    let line = crate::monitor::footer::line(crate::monitor::footer::FooterInput {
        tab: crate::monitor::MonitorTab::Procs,
        prefs: &prefs,
        confirm: Some(&menu_confirm),
        filtering: false,
        filter: "",
        notice: None,
        status: None,
        paused: false,
        container_ours: false,
        disk_rows: 0,
        board_panels: 0,
        board_zoomed: false,
    });
    let (menu_text, menu_hint) = match line {
        crate::seg::Line::Split { l, r } => (
            l.iter()
                .flat_map(|segment| segment.text.chars())
                .collect::<String>(),
            r.iter()
                .flat_map(|segment| segment.text.chars())
                .collect::<String>(),
        ),
        _ => panic!("signal menu should own both footer slots"),
    };
    assert!(menu_text.contains(">t TERM"));
    assert!(menu_text.contains(crate::platform::ProcSignal::menu()[0].label()));
    assert_eq!(menu_hint, "Up/Down choose · Enter select · Esc cancel");

    let hangup = Confirm::Signal {
        pid: 4242,
        start_time: 17,
        start_ticks: Some(170),
        label: "pid 4242 worker".into(),
        stage: crate::platform::ProcSignal::Hangup,
    };
    let line = crate::monitor::footer::line(crate::monitor::footer::FooterInput {
        tab: crate::monitor::MonitorTab::Procs,
        prefs: &prefs,
        confirm: Some(&hangup),
        filtering: false,
        filter: "",
        notice: None,
        status: None,
        paused: false,
        container_ours: false,
        disk_rows: 0,
        board_panels: 0,
        board_zoomed: false,
    });
    let text = match line {
        crate::seg::Line::Split { l, .. } => l
            .iter()
            .flat_map(|segment| segment.text.chars())
            .collect::<String>(),
        _ => panic!("signal confirmation should own the footer"),
    };
    assert!(text.contains("send SIGHUP to pid 4242 worker?"));
}
