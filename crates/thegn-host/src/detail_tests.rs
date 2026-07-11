use super::*;

fn screen() -> Rect {
    Rect {
        x: 0,
        y: 0,
        cols: 120,
        rows: 40,
    }
}

fn item_at(y: usize) -> Rect {
    Rect {
        x: 80,
        y,
        cols: 8,
        rows: 1,
    }
}

fn model_cpu(p: u8) -> FrameModel {
    FrameModel {
        stats: thegn_metrics::StatsSnapshot {
            cpu_pct: Some(p),
            ..Default::default()
        },
        ..Default::default()
    }
}

#[test]
fn cpu_maps_to_a_graph_near_the_item() {
    let model = model_cpu(42);
    let hist = TelemetryHistory::default();
    let ov = open_detail_for(
        &BarItemId::Widget("cpu".into()),
        item_at(0),
        screen(),
        &model,
        &hist,
    )
    .expect("cpu has a detail view");
    assert!(matches!(ov.content, DetailContent::Graph(_)));
    assert_eq!((ov.cols, ov.rows), (40, 12));
    // Item in the top half → drops below.
    assert!(matches!(ov.placement, Placement::NearBelow(_)));
}

#[test]
fn box_rect_encloses_the_drawn_box() {
    let model = model_cpu(42);
    let hist = TelemetryHistory::default();
    let item = item_at(0);
    let ov = open_detail_for(
        &BarItemId::Widget("cpu".into()),
        item,
        screen(),
        &model,
        &hist,
    )
    .expect("cpu has a detail view");
    let b = ov.box_rect(screen()).expect("box fits");
    // A NearBelow popup drops beneath its anchor item.
    assert!(b.y >= item.y + item.rows, "box should sit below the item");
    let contains =
        |r: Rect, x: usize, y: usize| x >= r.x && x < r.x + r.cols && y >= r.y && y < r.y + r.rows;
    // A point just inside the box is contained; a far corner is not.
    assert!(contains(b, b.x + 1, b.y + 1));
    assert!(!contains(b, 0, 0));
}

#[test]
fn absent_data_yields_no_modal() {
    let model = FrameModel::default(); // no gpu, no battery, no temp
    let hist = TelemetryHistory::default();
    for id in [
        "gpu", "battery", "temp", "load", "swap", "freq", "uptime", "pr", "tests", "loc",
    ] {
        assert!(
            open_detail_for(
                &BarItemId::Widget(id.into()),
                item_at(0),
                screen(),
                &model,
                &hist
            )
            .is_none(),
            "{id} with no data should not open a modal"
        );
    }
}

#[test]
fn notifications_badge_is_a_list_even_when_empty() {
    let model = FrameModel::default();
    let ov = open_detail_for(
        &BarItemId::Badge(BarBadge::Notifications),
        item_at(39),
        screen(),
        &model,
        &TelemetryHistory::default(),
    )
    .expect("notifications always opens");
    match ov.content {
        DetailContent::List(l) => {
            assert!(l.rows.is_empty());
            assert!(!l.empty_hint.is_empty());
        }
        _ => panic!("expected a list"),
    }
}

#[test]
fn disk_badge_shows_free_used_total_and_worktree_rows() {
    let mut model = FrameModel::default();
    let gib = 1024u64 * 1024 * 1024;
    model.stats.disk_free_pct = Some(8);
    model.stats.disk_bytes = Some((100 * gib, 8 * gib)); // 100G total, 8G free
    let mut sizes = std::collections::HashMap::new();
    sizes.insert("/wt/a".to_string(), ((40 * gib) as i64, (30 * gib) as i64));
    model.sidebar_status = crate::sidebar::SidebarStatus {
        disk_sizes: sizes,
        ..Default::default()
    };
    let ov = open_detail_for(
        &BarItemId::Badge(BarBadge::DiskWarn),
        item_at(39),
        screen(),
        &model,
        &TelemetryHistory::default(),
    )
    .expect("disk badge opens a modal");
    assert_eq!(ov.title, "Disk space");
    match ov.content {
        DetailContent::KeyVal(kv) => {
            let keys: Vec<&str> = kv.pairs.iter().map(|(k, _, _)| k.as_str()).collect();
            assert_eq!(keys, ["free", "used", "total", "worktrees", "reclaimable"]);
            let free = &kv.pairs[0];
            assert!(free.1.contains("8%"), "free row shows %: {:?}", free.1);
            assert!(free.1.contains("8GB"), "free row shows bytes: {:?}", free.1);
            // 8% ≤ critical (10) → red.
            assert_eq!(free.2, Tok::Hue(Hue::Red));
            assert_eq!(kv.pairs[2].1, "100GB", "total bytes");
            assert_eq!(kv.pairs[3].1, "40GB", "worktree usage sum");
            assert_eq!(kv.pairs[4].1, "30GB", "reclaimable target/ sum");
        }
        _ => panic!("expected a keyval"),
    }
}

#[test]
fn statusbar_item_opens_above_itself() {
    let model = model_cpu(10);
    let ov = open_detail_for(
        &BarItemId::Widget("cpu".into()),
        item_at(39),
        screen(),
        &model,
        &TelemetryHistory::default(),
    )
    .unwrap();
    assert!(matches!(ov.placement, Placement::NearAbove(_)));
}

#[test]
fn list_scroll_clamps_at_both_ends() {
    let rows: Vec<DetailRow> = (0..3)
        .map(|i| DetailRow::new(Tok::Slot(S::Text), "•", format!("row {i}")))
        .collect();
    let mut ov = list("L", rows, "empty", 40, 10);
    // Up at the top is a no-op.
    assert_eq!(
        ov.handle_key(&KeyCode::UpArrow, Modifiers::NONE),
        DetailOutcome::Pending
    );
    assert_eq!(ov.scroll, 0);
    // Down clamps to len-1.
    for _ in 0..10 {
        ov.handle_key(&KeyCode::DownArrow, Modifiers::NONE);
    }
    assert_eq!(ov.scroll, 2);
    // A plain (non-actionable) list scrolls but never fires an action.
    assert!(!ov.actionable());
}

#[test]
fn actionable_list_moves_cursor_and_fires_actions() {
    let rows: Vec<DetailRow> = (0..3)
        .map(|i| {
            DetailRow::new(Tok::Slot(S::Text), "•", format!("run {i}"))
                .on_enter(DetailAction::FocusWorktree(format!("/wt/{i}")))
                .action('o', DetailAction::OpenUrl(format!("https://ci/{i}")))
        })
        .collect();
    let mut ov = list("CI", rows, "empty", 56, 6);
    assert!(ov.actionable());
    // j moves the row cursor, not the scroll.
    assert_eq!(
        ov.handle_key(&KeyCode::Char('j'), Modifiers::NONE),
        DetailOutcome::Pending
    );
    assert_eq!(ov.sel, 1);
    assert_eq!(ov.scroll, 0);
    // Enter fires the selected row's drilldown action.
    assert_eq!(
        ov.handle_key(&KeyCode::Enter, Modifiers::NONE),
        DetailOutcome::Act(DetailAction::FocusWorktree("/wt/1".into()))
    );
    // A bound char fires that row's action; an unbound char is a no-op.
    assert_eq!(
        ov.handle_key(&KeyCode::Char('o'), Modifiers::NONE),
        DetailOutcome::Act(DetailAction::OpenUrl("https://ci/1".into()))
    );
    assert_eq!(
        ov.handle_key(&KeyCode::Char('z'), Modifiers::NONE),
        DetailOutcome::Pending
    );
    // Esc still closes.
    assert_eq!(
        ov.handle_key(&KeyCode::Escape, Modifiers::NONE),
        DetailOutcome::Close
    );
}

#[test]
fn ci_badge_detail_is_actionable_with_a_hint() {
    let model = FrameModel {
        panel: crate::panel::PanelData {
            ci_runs: vec![thegn_core::ci::CiRun {
                id: "42".into(),
                name: "CI".into(),
                state: thegn_core::ci::CiState::Running,
                url: "https://example/42".into(),
                ..Default::default()
            }],
            ..Default::default()
        },
        ..Default::default()
    };
    let ov = open_detail_for(
        &BarItemId::Badge(BarBadge::Ci),
        item_at(39),
        screen(),
        &model,
        &TelemetryHistory::default(),
    )
    .expect("ci badge opens a detail overlay");
    assert!(ov.actionable());
    assert!(ov.hint.is_some());
    // `c` cancels the running run (still on the list, before drilling).
    assert_eq!(
        ov.action_for('c'),
        Some(DetailAction::CiCancel {
            run_id: "42".into()
        })
    );
}

#[test]
fn esc_and_enter_close() {
    let mut ov = keyval(
        "k",
        vec![("a".into(), "b".into(), Tok::Slot(S::Text))],
        20,
        Placement::Center,
    );
    assert_eq!(
        ov.handle_key(&KeyCode::Enter, Modifiers::NONE),
        DetailOutcome::Close
    );
    assert_eq!(
        ov.handle_key(&KeyCode::Escape, Modifiers::NONE),
        DetailOutcome::Close
    );
    assert_eq!(
        ov.handle_key(&KeyCode::Char('c'), Modifiers::CTRL),
        DetailOutcome::Close
    );
    // A graph ignores arrows (no list to scroll) but stays open.
    assert_eq!(
        ov.handle_key(&KeyCode::DownArrow, Modifiers::NONE),
        DetailOutcome::Pending
    );
}

#[test]
fn renders_without_panic_and_is_legible() {
    let model = model_cpu(55);
    let mut hist = TelemetryHistory::default();
    for i in 0..50 {
        hist.push(&thegn_metrics::StatsSnapshot {
            cpu_pct: Some((i % 100) as u8),
            ..Default::default()
        });
    }
    let ov = open_detail_for(
        &BarItemId::Widget("cpu".into()),
        item_at(0),
        screen(),
        &model,
        &hist,
    )
    .unwrap();
    let mut s = Surface::new(120, 40);
    ov.render(&mut s, screen());
    assert!(seg::text_contrast_violations(&mut s, 3.0).is_empty());
}

fn model_loc(n: usize) -> FrameModel {
    use thegn_core::loc::{LocLang, LocReport};
    let langs = (0..n)
        .map(|i| LocLang {
            name: format!("Lang{i:02}"),
            files: i + 1,
            lines: (i + 1) * 30,
            code: (i + 1) * 20,
            comments: (i + 1) * 6,
            blanks: (i + 1) * 4,
        })
        .collect();
    FrameModel {
        loc: Some(LocReport::from_langs(langs)),
        ..Default::default()
    }
}

#[test]
fn loc_opens_a_scrollable_tokei_table() {
    let model = model_loc(20);
    let mut ov = open_detail_for(
        &BarItemId::Widget("loc".into()),
        item_at(39),
        screen(),
        &model,
        &TelemetryHistory::default(),
    )
    .expect("loc opens a detail overlay");
    // A table (not a keyval), with the Total footer and the full header set.
    let (headers, len) = match &ov.content {
        DetailContent::Table(t) => {
            assert_eq!(t.total[0], "Total");
            assert_eq!(t.headers.len(), 6);
            assert_eq!(t.headers[0], "Language");
            (t.headers.clone(), t.rows.len())
        }
        _ => panic!("expected a table"),
    };
    assert_eq!(len, 20);
    assert_eq!(headers[3], "Code");
    // Non-actionable: j/k scroll and clamp at the last row; Enter closes.
    assert!(!ov.actionable());
    for _ in 0..50 {
        ov.handle_key(&KeyCode::DownArrow, Modifiers::NONE);
    }
    assert_eq!(ov.scroll, len - 1);
    assert_eq!(
        ov.handle_key(&KeyCode::Enter, Modifiers::NONE),
        DetailOutcome::Close
    );
}

#[test]
fn loc_table_renders_legibly() {
    let model = model_loc(8);
    let ov = open_detail_for(
        &BarItemId::Widget("loc".into()),
        item_at(39),
        screen(),
        &model,
        &TelemetryHistory::default(),
    )
    .unwrap();
    let mut s = Surface::new(120, 40);
    ov.render(&mut s, screen());
    assert!(seg::text_contrast_violations(&mut s, 3.0).is_empty());
}

// --- notifications + log viewer ---------------------------------------

use thegn_core::notification::{Notification, NotificationKind};

fn notif(kind: NotificationKind, source_ref: &str, msg: &str, age_secs: i64) -> Notification {
    Notification {
        id: 1,
        kind,
        source_ref: source_ref.into(),
        message: msg.into(),
        created_at_ms: thegn_core::util::now() - age_secs,
        read: false,
        worktree_path: String::new(),
    }
}

fn err_line(msg: &str) -> LogLine {
    LogLine {
        timestamp: "2026-06-05T12:00:00".into(),
        level: LogLevel::Error,
        target: "thegn".into(),
        message: msg.into(),
        raw: format!("2026-06-05T12:00:00  ERROR thegn  {msg}"),
    }
}

fn info_line(msg: &str) -> LogLine {
    LogLine {
        timestamp: "2026-06-05T12:00:01".into(),
        level: LogLevel::Info,
        target: "thegn".into(),
        message: msg.into(),
        raw: format!("2026-06-05T12:00:01  INFO  thegn  {msg}"),
    }
}

fn notif_model(notifications: Vec<Notification>, log_tail: Vec<LogLine>) -> FrameModel {
    FrameModel {
        panel: crate::panel::PanelData {
            notifications,
            log_tail,
            ..Default::default()
        },
        ..Default::default()
    }
}

fn open_notifications(model: &FrameModel) -> DetailOverlay {
    open_detail_for(
        &BarItemId::Badge(BarBadge::Notifications),
        item_at(39),
        screen(),
        model,
        &TelemetryHistory::default(),
    )
    .expect("notifications always opens")
}

#[test]
fn notification_note_is_a_real_age_not_a_millisecond_bug() {
    // Regression: `created_at_ms` is epoch *seconds*, so the note must go
    // through `util::age` — a 3-minute-old entry reads "3m ago", never the
    // "20617d" a ms-vs-s mixup produced.
    let model = notif_model(
        vec![notif(NotificationKind::WorktreeCreated, "wt", "ready", 180)],
        vec![],
    );
    let ov = open_notifications(&model);
    let DetailContent::List(l) = &ov.content else {
        panic!("expected a list");
    };
    let note = l.rows[0].note.as_deref().unwrap();
    assert!(note.ends_with("ago"), "note: {note}");
    assert!(!note.contains("20617"), "note: {note}");
    assert!(note.starts_with('3'), "note: {note}");
}

#[test]
fn notifications_are_actionable_with_dismiss_clear_keys() {
    let model = notif_model(
        vec![notif(NotificationKind::WorktreeCreated, "wt", "ready", 5)],
        vec![],
    );
    let mut ov = open_notifications(&model);
    assert!(ov.actionable());
    assert!(ov.hint.is_some());
    assert_eq!(
        ov.handle_key(&KeyCode::Char('x'), Modifiers::NONE),
        DetailOutcome::Act(DetailAction::DismissNotification { id: 1 })
    );
    assert_eq!(
        ov.handle_key(&KeyCode::Char('X'), Modifiers::NONE),
        DetailOutcome::Act(DetailAction::ClearNotifications)
    );
}

#[test]
fn log_error_notification_drills_into_the_log_view_in_place() {
    let model = notif_model(
        vec![notif(
            NotificationKind::LogError,
            "log:thegn",
            "1 error in thegn.log",
            5,
        )],
        vec![info_line("started"), err_line("boom"), info_line("more")],
    );
    let mut ov = open_notifications(&model);
    // `o` on the log row opens the full-log pager.
    assert_eq!(
        ov.handle_key(&KeyCode::Char('o'), Modifiers::NONE),
        DetailOutcome::Act(DetailAction::OpenLogPager)
    );
    // Enter drills in place: content becomes the (error-gated) log view.
    assert_eq!(
        ov.handle_key(&KeyCode::Enter, Modifiers::NONE),
        DetailOutcome::Pending
    );
    let DetailContent::Log(l) = &ov.content else {
        panic!("expected the log view");
    };
    assert_eq!(l.level, Some(LogLevel::Error));
    assert_eq!(l.matches().len(), 1, "only the ERROR line matches");
    // `l` widens the gate to warn+, which now also admits the INFO lines…
    ov.handle_key(&KeyCode::Char('l'), Modifiers::NONE);
    // …cycle all the way to "all" and every line is visible.
    for _ in 0..4 {
        ov.handle_key(&KeyCode::Char('l'), Modifiers::NONE);
    }
    let DetailContent::Log(l) = &ov.content else {
        panic!("expected the log view");
    };
    assert_eq!(l.level, None, "cycled to all levels");
    assert_eq!(l.matches().len(), 3);
    // `F` opens the full log; Enter copies the selected line; Esc closes.
    assert_eq!(
        ov.handle_key(&KeyCode::Char('F'), Modifiers::NONE),
        DetailOutcome::Act(DetailAction::OpenLogPager)
    );
    assert!(matches!(
        ov.handle_key(&KeyCode::Enter, Modifiers::NONE),
        DetailOutcome::Act(DetailAction::CopyLine(_))
    ));
    assert_eq!(
        ov.handle_key(&KeyCode::Escape, Modifiers::NONE),
        DetailOutcome::Close
    );
}

#[test]
fn log_drilldown_shows_error_that_scrolled_past_the_plain_tail() {
    // Regression: the notification counts errors over the whole file, but the
    // drilldown payload used to be the last 400 lines of *all* levels. A single
    // ERROR older than that window left the error-gated view empty ("no matching
    // log lines"). `error_inclusive_tail` folds the recent errors back in.
    let mut all_lines = vec![err_line("boom")]; // the counted error, at the very start
    all_lines.extend((0..1000).map(|i| info_line(&format!("noise {i}"))));
    let log_tail = thegn_core::log_view::error_inclusive_tail(&all_lines, 400, 200);
    let model = notif_model(
        vec![notif(
            NotificationKind::LogError,
            "log:thegn",
            "1 error in thegn.log",
            5,
        )],
        log_tail,
    );
    let mut ov = open_notifications(&model);
    assert_eq!(
        ov.handle_key(&KeyCode::Enter, Modifiers::NONE),
        DetailOutcome::Pending
    );
    let DetailContent::Log(l) = &ov.content else {
        panic!("expected the log view");
    };
    assert_eq!(l.level, Some(LogLevel::Error));
    assert!(
        !l.matches().is_empty(),
        "the scrolled-out ERROR must still appear in the drilldown"
    );
}

#[test]
fn log_view_text_filter_narrows_and_reclamps() {
    let model = notif_model(
        vec![notif(NotificationKind::LogError, "log:thegn", "errs", 5)],
        vec![err_line("connection refused"), err_line("disk full")],
    );
    let mut ov = open_notifications(&model);
    ov.handle_key(&KeyCode::Enter, Modifiers::NONE);
    // `/` enters filter-edit; typing narrows the view; letters don't close.
    ov.handle_key(&KeyCode::Char('/'), Modifiers::NONE);
    for c in "disk".chars() {
        assert_eq!(
            ov.handle_key(&KeyCode::Char(c), Modifiers::NONE),
            DetailOutcome::Pending
        );
    }
    let DetailContent::Log(l) = &ov.content else {
        panic!("expected the log view");
    };
    assert!(l.filter_edit);
    assert_eq!(l.matches().len(), 1);
    // Enter leaves edit mode (does not copy while editing).
    assert_eq!(
        ov.handle_key(&KeyCode::Enter, Modifiers::NONE),
        DetailOutcome::Pending
    );
    assert!(matches!(&ov.content, DetailContent::Log(l) if !l.filter_edit));
}

#[test]
fn log_view_renders_legibly() {
    let model = notif_model(
        vec![notif(NotificationKind::LogError, "log:thegn", "errs", 5)],
        vec![err_line("boom"), info_line("ok"), err_line("kaboom")],
    );
    let mut ov = open_notifications(&model);
    ov.handle_key(&KeyCode::Enter, Modifiers::NONE);
    let mut s = Surface::new(120, 40);
    ov.render(&mut s, screen());
    assert!(seg::text_contrast_violations(&mut s, 3.0).is_empty());
}

/// A model with disk + network + gpu + battery populated, for the sectioned
/// widget popups.
fn model_full() -> FrameModel {
    FrameModel {
        stats: thegn_metrics::StatsSnapshot {
            mem_gib: Some((6.0, 16.0)),
            swap_gib: Some((0.5, 8.0)),
            gpu_pct: Some(40),
            gpu_mem_mib: Some((2048, 8192)),
            gpu_temp_c: Some(55.0),
            gpu_power_w: Some(60.0),
            net_bps: Some((1024, 2048)),
            net_ifaces: vec![("eth0".into(), 1024, 2048), ("wlan0".into(), 512, 256)],
            battery: Some((72, false)),
            battery_power_w: Some(12.5),
            disks: vec![
                thegn_metrics::DiskInfo {
                    name: "nvme0n1p2".into(),
                    mount: "/".into(),
                    free_pct: 42,
                    read_bps: 1_500_000,
                    write_bps: 200_000,
                    kind: thegn_metrics::DiskKind::Ssd,
                },
                thegn_metrics::DiskInfo {
                    name: "sda1".into(),
                    mount: "/mnt/data".into(),
                    free_pct: 8,
                    read_bps: 0,
                    write_bps: 0,
                    kind: thegn_metrics::DiskKind::Hdd,
                },
            ],
            ..Default::default()
        },
        ..Default::default()
    }
}

#[test]
fn rich_widgets_map_to_sections() {
    let model = model_full();
    let hist = TelemetryHistory::default();
    for w in ["disk", "mem", "net", "gpu", "battery"] {
        let ov = open_detail_for(
            &BarItemId::Widget(w.into()),
            item_at(0),
            screen(),
            &model,
            &hist,
        )
        .unwrap_or_else(|| panic!("{w} should open a detail"));
        assert!(
            matches!(ov.content, DetailContent::Sections(_)),
            "{w} should be a sectioned popup"
        );
    }
}

#[test]
fn section_height_sums_its_rows() {
    assert_eq!(
        Section::Heading {
            label: "h".into(),
            note: None
        }
        .height(),
        1
    );
    assert_eq!(
        Section::Sparkrow {
            label: "s".into(),
            spark: vec![0.1, 0.2],
            cur: "x".into(),
            tone: Tok::Slot(S::Text),
        }
        .height(),
        1
    );
    let g = |height, footer: Option<&str>| {
        Section::Graph(GraphSection {
            label: "g".into(),
            cur: "c".into(),
            footer: footer.map(str::to_string),
            series: vec![],
            tone: Tok::Slot(S::Text),
            height,
            series2: None,
        })
    };
    assert_eq!(g(5, Some("f")).height(), 7); // header + 5 + footer
    assert_eq!(g(5, None).height(), 6); // header + 5
    assert_eq!(Section::KeyVal(vec![]).height(), 0);
    let tbl = |header: Vec<String>, n: usize| {
        Section::Table(TableSection {
            header,
            rows: (0..n)
                .map(|_| vec![Cell::Text("x".into(), Tok::Slot(S::Text))])
                .collect(),
        })
    };
    assert_eq!(tbl(vec!["h".into()], 2).height(), 3); // header + 2
    assert_eq!(tbl(vec![], 2).height(), 2); // no header
}

#[test]
fn battery_eta_projects_from_slope() {
    // Discharging on battery → a projected time (leading zeros ignored).
    assert!(
        battery_eta(&[0.0, 0.0, 0.9, 0.8, 0.7, 0.6], false)
            .unwrap()
            .starts_with('~')
    );
    // Charging on AC → time-to-full.
    assert!(battery_eta(&[0.4, 0.5, 0.6, 0.7], true).is_some());
    // Flat charge → no projection.
    assert_eq!(battery_eta(&[0.5, 0.5, 0.5], false), None);
    // Slope contradicts the source (falling while "on AC") → no guess.
    assert_eq!(battery_eta(&[0.9, 0.8, 0.7], true), None);
    // Too little history → None.
    assert_eq!(battery_eta(&[0.8], false), None);
}

#[test]
fn sections_popup_renders_legibly() {
    let model = model_full();
    let mut hist = TelemetryHistory::default();
    for i in 0..60 {
        hist.push(&model.stats);
        let _ = i;
    }
    for w in ["disk", "net", "gpu", "battery", "mem"] {
        let ov = open_detail_for(
            &BarItemId::Widget(w.into()),
            item_at(0),
            screen(),
            &model,
            &hist,
        )
        .unwrap();
        let mut s = Surface::new(120, 40);
        ov.render(&mut s, screen());
        assert!(
            seg::text_contrast_violations(&mut s, 3.0).is_empty(),
            "{w} popup has an unreadable cell"
        );
    }
}

#[test]
fn tall_sections_popup_scrolls() {
    // A popup whose stacked height exceeds its box scrolls by row.
    let secs = vec![Section::KeyVal(
        (0..30)
            .map(|i| (format!("k{i}"), format!("v{i}"), Tok::Slot(S::Text)))
            .collect(),
    )];
    let mut ov = sections("Tall", 30, secs, Placement::Center);
    // Cap the visible rows so it overflows.
    ov.rows = 10;
    assert!(ov.content_rows() > ov.rows);
    for _ in 0..100 {
        ov.handle_key(&KeyCode::DownArrow, Modifiers::NONE);
    }
    assert_eq!(ov.scroll, ov.content_rows() - ov.rows);
    for _ in 0..100 {
        ov.handle_key(&KeyCode::UpArrow, Modifiers::NONE);
    }
    assert_eq!(ov.scroll, 0);
}
