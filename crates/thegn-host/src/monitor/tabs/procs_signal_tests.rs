use super::*;
use std::cell::Cell;
use termwiz::input::{KeyCode, Modifiers};

thread_local! {
    static FAIL_PID: Cell<Option<u32>> = const { Cell::new(None) };
}

fn fake_signal(
    pid: u32,
    _start: crate::platform::ProcStart,
    signal: crate::platform::ProcSignal,
) -> Result<(), crate::platform::SignalError> {
    super::record_test_signal(pid, signal);
    if FAIL_PID.get() == Some(pid) {
        Err(crate::platform::SignalError::Failed(
            "permission denied".into(),
        ))
    } else {
        Ok(())
    }
}

fn make_overlay() -> MonitorOverlay {
    super::set_test_signal_call(fake_signal);
    super::reset_test_signal_records();
    FAIL_PID.set(None);
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
    let mut overlay = make_overlay();
    key(&mut overlay, KeyCode::Char('X'));
    assert!(matches!(
        overlay.confirm,
        Some(Confirm::SignalMenu { pid: 4242, .. })
    ));
    let Some(signal) = crate::platform::ProcSignal::menu()
        .iter()
        .copied()
        .find(|signal| !signal.needs_confirm())
    else {
        return;
    };
    key(&mut overlay, KeyCode::Char(signal.menu_key()));
    assert!(overlay.confirm.is_none());
    assert_eq!(super::test_signal_records(), vec![(4242, signal)]);
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
        let mut overlay = make_overlay();
        key(&mut overlay, KeyCode::Char('X'));
        key(&mut overlay, KeyCode::Char(signal.menu_key()));
        assert!(matches!(
            overlay.confirm,
            Some(Confirm::Signal { stage, .. }) if stage == signal
        ));
        super::reset_test_signal_records();
        let prefs = crate::monitor::MonitorPrefs::default();
        let confirm = overlay
            .confirm
            .as_ref()
            .expect("confirmation stays set? none");
        let line = crate::monitor::footer::line(crate::monitor::footer::FooterInput {
            tab: crate::monitor::MonitorTab::Procs,
            prefs: &prefs,
            confirm: Some(confirm),
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
        let footer = match line {
            crate::seg::Line::Split { l, .. } => l
                .iter()
                .flat_map(|part| part.text.chars())
                .collect::<String>(),
            _ => panic!("signal confirmation should own the footer"),
        };
        let expected = match signal {
            crate::platform::ProcSignal::Kill => "KILL (no cleanup) pid 4242 worker?",
            crate::platform::ProcSignal::Stop => "send SIGSTOP to pid 4242 worker?",
            _ => unreachable!(),
        };
        assert!(footer.contains(expected));
        key(&mut overlay, KeyCode::Char('y'));
        assert_eq!(super::test_signal_records(), vec![(4242, signal)]);
    }
}

#[test]
fn menu_ignores_y_cancels_and_arrows_select_or_confirm_expected_entries() {
    let mut overlay = make_overlay();
    key(&mut overlay, KeyCode::Char('X'));
    key(&mut overlay, KeyCode::Char('y'));
    assert!(matches!(
        overlay.confirm,
        Some(Confirm::SignalMenu { cursor: 0, .. })
    ));
    assert!(super::test_signal_records().is_empty());
    key(&mut overlay, KeyCode::Escape);
    assert!(overlay.confirm.is_none());
    assert_eq!(overlay.status.as_deref(), Some("cancelled"));

    key(&mut overlay, KeyCode::Char('X'));
    key(&mut overlay, KeyCode::Char('n'));
    assert!(overlay.confirm.is_none());
    assert_eq!(overlay.status.as_deref(), Some("cancelled"));

    key(&mut overlay, KeyCode::Char('X'));
    key(&mut overlay, KeyCode::DownArrow);
    key(&mut overlay, KeyCode::DownArrow);
    key(&mut overlay, KeyCode::Enter);
    if crate::platform::ProcSignal::menu().len() > 2 {
        assert!(overlay.confirm.is_none());
        assert_eq!(
            super::test_signal_records(),
            vec![(4242, crate::platform::ProcSignal::menu()[2])]
        );
    }

    if crate::platform::ProcSignal::menu().contains(&crate::platform::ProcSignal::Kill) {
        let mut overlay = make_overlay();
        key(&mut overlay, KeyCode::Char('X'));
        for _ in 0..3 {
            key(&mut overlay, KeyCode::DownArrow);
        }
        key(&mut overlay, KeyCode::Enter);
        assert!(matches!(
            overlay.confirm,
            Some(Confirm::Signal {
                stage: crate::platform::ProcSignal::Kill,
                ..
            })
        ));
        key(&mut overlay, KeyCode::Char('y'));
        assert_eq!(
            super::test_signal_records(),
            vec![(4242, crate::platform::ProcSignal::Kill)]
        );
    }

    let mut overlay = make_overlay();
    key(&mut overlay, KeyCode::Char('X'));
    key(&mut overlay, KeyCode::UpArrow);
    assert!(matches!(
        overlay.confirm,
        Some(Confirm::SignalMenu { cursor: 0, .. })
    ));
    for _ in 0..20 {
        key(&mut overlay, KeyCode::DownArrow);
    }
    assert!(
        matches!(overlay.confirm, Some(Confirm::SignalMenu { cursor, .. }) if cursor == super::menu_len() - 1)
    );
}

fn family_entry(pid: u32, ppid: Option<u32>, secs: u64) -> super::FamilyEntry {
    super::FamilyEntry {
        pid,
        ppid,
        start: crate::platform::ProcStart {
            secs,
            ticks: Some(secs * 10),
        },
    }
}

#[test]
fn term_tree_targets_are_deepest_first_bounded_and_cycle_safe() {
    let family = [
        family_entry(10, Some(1), 100),
        family_entry(20, Some(10), 200),
        family_entry(21, Some(10), 210),
        family_entry(30, Some(20), 300),
        family_entry(40, Some(4), 400),
    ];
    let mut targets = Vec::new();
    assert_eq!(
        super::term_tree_targets(&family, (10, 100), 999, &mut targets),
        super::TreePlan::Ready {
            truncated: false,
            excluded: 0,
        }
    );
    assert_eq!(
        targets.iter().map(|entry| entry.pid).collect::<Vec<_>>(),
        vec![30, 20, 21, 10]
    );
    assert_eq!(
        super::term_tree_targets(&family, (10, 100), 20, &mut targets),
        super::TreePlan::ContainsThegn
    );
    assert!(targets.is_empty());
    let cycle = [
        family_entry(50, Some(51), 500),
        family_entry(51, Some(50), 510),
    ];
    assert_eq!(
        super::term_tree_targets(&cycle, (50, 500), 999, &mut targets),
        super::TreePlan::Ready {
            truncated: false,
            excluded: 0,
        }
    );
    assert_eq!(targets.len(), 2);
    let mut broad = Vec::with_capacity(301);
    broad.push(family_entry(1, None, 1));
    broad.extend((2..=301).map(|pid| family_entry(pid, Some(1), u64::from(pid))));
    assert_eq!(
        super::term_tree_targets(&broad, (1, 1), 999, &mut targets),
        super::TreePlan::Ready {
            truncated: true,
            excluded: 45,
        }
    );
    assert_eq!(targets.len(), 256);
    assert_eq!(
        super::term_tree_targets(&family, (99, 99), 999, &mut targets),
        super::TreePlan::MissingRoot
    );
}

fn seed_tree(overlay: &mut MonitorOverlay) {
    overlay.proc_rows[0].pid = 10;
    overlay.proc_rows[0].start_time = 100;
    overlay.proc_family = vec![
        family_entry(10, Some(1), 100),
        family_entry(20, Some(10), 200),
        family_entry(21, Some(10), 210),
        family_entry(30, Some(20), 300),
        family_entry(40, Some(4), 400),
    ];
}

fn choose_tree(overlay: &mut MonitorOverlay) {
    key(overlay, KeyCode::Char('X'));
    key(overlay, KeyCode::Char('r'));
}

#[test]
fn term_tree_confirmation_delivers_sampled_targets_deepest_first() {
    let mut overlay = make_overlay();
    seed_tree(&mut overlay);
    choose_tree(&mut overlay);
    assert!(
        matches!(overlay.confirm, Some(Confirm::SignalTree { targets, .. }) if targets.len() == 4)
    );
    let prefs = crate::monitor::MonitorPrefs::default();
    let line = crate::monitor::footer::line(crate::monitor::footer::FooterInput {
        tab: crate::monitor::MonitorTab::Procs,
        prefs: &prefs,
        confirm: overlay.confirm.as_ref(),
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
            .flat_map(|part| part.text.chars())
            .collect::<String>(),
        _ => panic!("tree confirmation should own the footer"),
    };
    assert!(text.contains("4 sampled processes"));
    key(&mut overlay, KeyCode::Char('y'));
    assert_eq!(
        super::test_signal_records(),
        vec![
            (30, crate::platform::ProcSignal::Terminate),
            (20, crate::platform::ProcSignal::Terminate),
            (21, crate::platform::ProcSignal::Terminate),
            (10, crate::platform::ProcSignal::Terminate),
        ]
    );
}

#[test]
fn term_tree_counts_vanished_and_refused_targets_without_stopping() {
    let mut vanished = make_overlay();
    seed_tree(&mut vanished);
    choose_tree(&mut vanished);
    vanished.proc_family.retain(|entry| entry.pid != 21);
    key(&mut vanished, KeyCode::Char('y'));
    assert!(
        vanished
            .status
            .as_deref()
            .is_some_and(|status| status.contains("3 of 4") && status.contains("1 vanished"))
    );
    assert_eq!(super::test_signal_records().len(), 3);

    let mut refused = make_overlay();
    seed_tree(&mut refused);
    choose_tree(&mut refused);
    FAIL_PID.set(Some(20));
    key(&mut refused, KeyCode::Char('y'));
    assert!(
        refused
            .status
            .as_deref()
            .is_some_and(|status| status.contains("1 refused: permission denied"))
    );
    assert_eq!(super::test_signal_records().len(), 4);
}

#[test]
fn missing_selection_does_not_open_and_vanished_identity_is_refused() {
    let mut empty = make_overlay();
    empty.proc_rows.clear();
    key(&mut empty, KeyCode::Char('X'));
    assert!(empty.confirm.is_none());

    let mut vanished = make_overlay();
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
    assert!(menu_text.contains("r TERM tree"));
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

#[test]
fn signal_confirmation_policy_matches_the_platform_menu() {
    for signal in crate::platform::ProcSignal::menu() {
        let expected = match signal {
            crate::platform::ProcSignal::Kill | crate::platform::ProcSignal::Stop => true,
            crate::platform::ProcSignal::Terminate => {
                crate::platform::ProcSignal::menu().len() == 1
            }
            _ => false,
        };
        assert_eq!(signal.needs_confirm(), expected, "{}", signal.label());
    }
    if crate::platform::ProcSignal::Terminate.needs_confirm() {
        let mut overlay = make_overlay();
        key(&mut overlay, KeyCode::Char('X'));
        key(&mut overlay, KeyCode::Char('t'));
        assert!(matches!(
            overlay.confirm,
            Some(Confirm::Signal {
                stage: crate::platform::ProcSignal::Terminate,
                ..
            })
        ));
    }
}

#[test]
fn term_tree_missing_root_reports_that_the_sample_changed() {
    let mut overlay = make_overlay();
    key(&mut overlay, KeyCode::Char('X'));
    key(&mut overlay, KeyCode::Char('r'));
    assert!(overlay.confirm.is_none());
    assert!(overlay.status.as_deref().is_some_and(|status| {
        status.contains("process changed or left the sampled list; select it again")
    }));
}
