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
        &model,
        &StatusCtx::new_for_test(&hist),
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
        &model,
        &StatusCtx::new_for_test(&hist),
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
                &model,
                &StatusCtx::new_for_test(&hist)
            )
            .is_none(),
            "{id} with no data should not open a modal"
        );
    }
}

#[test]
fn notifications_badge_is_the_unified_surface_with_a_logs_entry() {
    // The unified surface always offers at least the Logs group's quiet entry
    // point, even when nothing needs the user and the inbox is empty.
    let model = FrameModel::default();
    let ov = open_detail_for(
        &BarItemId::Badge(BarBadge::Notifications),
        item_at(39),
        &model,
        &StatusCtx::new_for_test(&TelemetryHistory::default()),
    )
    .expect("notifications always opens");
    assert_eq!(ov.title(), "Notifications");
    match ov.content {
        DetailContent::List(l) => {
            let texts: Vec<&str> = l.rows.iter().map(|r| r.text.as_str()).collect();
            assert_eq!(texts, ["Logs", "open thegn.log"]);
            assert!(l.rows[0].header, "the Logs label is a heading row");
            assert!(!l.rows[1].header, "the log entry point is selectable");
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
        &model,
        &StatusCtx::new_for_test(&TelemetryHistory::default()),
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
        &model,
        &StatusCtx::new_for_test(&TelemetryHistory::default()),
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
        &model,
        &StatusCtx::new_for_test(&TelemetryHistory::default()),
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
        hist.push(
            &thegn_metrics::StatsSnapshot {
                cpu_pct: Some((i % 100) as u8),
                ..Default::default()
            },
            (i as u64 + 1) * 1000,
        );
    }
    let ov = open_detail_for(
        &BarItemId::Widget("cpu".into()),
        item_at(0),
        &model,
        &StatusCtx::new_for_test(&hist),
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
        &model,
        &StatusCtx::new_for_test(&TelemetryHistory::default()),
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
        &model,
        &StatusCtx::new_for_test(&TelemetryHistory::default()),
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
        model,
        &StatusCtx::new_for_test(&TelemetryHistory::default()),
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
    // The row lives under the "Notifications" group heading now — find it by text.
    let row = l
        .rows
        .iter()
        .find(|r| !r.header && r.text == "ready")
        .expect("the notification row");
    let note = row.note.as_deref().unwrap();
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
    // The one clear-all key across every surface is `a` (the `X`/`R` aliases
    // were retired) — and from a notification row it is the same total clear
    // (acks the live needs-you set too), not a notifications-only sweep.
    assert_eq!(
        ov.handle_key(&KeyCode::Char('a'), Modifiers::NONE),
        DetailOutcome::Act(DetailAction::AckAllAttention)
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

// --- unified surface: grouping, dedup, cursor-skip, log gate -----------

/// Attach a needs-you (Failure-tier) attention signal for `path` to `model`.
fn needs_you(model: &mut FrameModel, path: &str) {
    use thegn_core::attention::{AttentionReason, AttentionScore, AttentionTier};
    model.sidebar_status.attention.insert(
        path.into(),
        AttentionScore {
            tier: AttentionTier::Failure,
            sub: 2,
            reason: AttentionReason::ProcessFailed,
            since: Some(60),
            episode: 0,
        },
    );
}

#[test]
fn unified_surface_groups_by_section_and_dedups_worktree_alerts() {
    // A per-worktree failure that already shows under "Needs you" must NOT be
    // repeated in "Alerts"; a host-global alert (no worktree) still shows there.
    let mut model = notif_model(
        vec![
            Notification {
                worktree_path: "/wt/a".into(),
                ..notif(NotificationKind::ProcessFailed, "proc", "a failed", 60)
            },
            notif(NotificationKind::AgentFailed, "agent", "global boom", 120),
            notif(
                NotificationKind::Assigned,
                "linear:1",
                "assigned to you",
                200,
            ),
        ],
        vec![],
    );
    needs_you(&mut model, "/wt/a");
    let ov = open_notifications(&model);
    let DetailContent::List(l) = &ov.content else {
        panic!("expected a list");
    };
    let headers: Vec<&str> = l
        .rows
        .iter()
        .filter(|r| r.header)
        .map(|r| r.text.as_str())
        .collect();
    assert_eq!(headers, ["Needs you", "Alerts", "Notifications", "Logs"]);
    let body: Vec<&str> = l
        .rows
        .iter()
        .filter(|r| !r.header)
        .map(|r| r.text.as_str())
        .collect();
    // /wt/a surfaces once — the Needs-you rollup row — and its raw alert is gone.
    assert!(
        body.iter().any(|t| t.contains("process failed")),
        "needs-you row present: {body:?}"
    );
    assert!(
        !body.contains(&"a failed"),
        "worktree alert deduped: {body:?}"
    );
    // The host-global alert and the notice history both survive.
    assert!(
        body.contains(&"global boom"),
        "global alert in Alerts: {body:?}"
    );
    assert!(
        body.contains(&"assigned to you"),
        "notice history: {body:?}"
    );
    assert!(body.contains(&"open thegn.log"), "logs entry: {body:?}");
}

#[test]
fn needs_you_rows_group_other_repos_separately() {
    // Two needs-you worktrees, only one in the active repo's scope. The
    // out-of-scope one must NOT be silently swallowed — it moves to its own
    // "Other repos" group, still selectable and still ackable.
    let mut model = notif_model(vec![], vec![]);
    needs_you(&mut model, "/wt/mine");
    needs_you(&mut model, "/wt/theirs");
    model.sidebar_status.repo_scope = Some(["/wt/mine".to_string()].into_iter().collect());

    let headers = |m: &FrameModel| {
        let ov = open_notifications(m);
        let DetailContent::List(l) = &ov.content else {
            panic!("expected a list");
        };
        l.rows
            .iter()
            .filter(|r| r.header)
            .map(|r| r.text.clone())
            .collect::<Vec<_>>()
    };
    let h = headers(&model);
    assert_eq!(h, ["Needs you", "Other repos (1)", "Logs"], "{h:?}");

    // Each group holds exactly one row, and the out-of-scope one keeps its `x`
    // ack action — scoping quiets the nag, it doesn't remove the affordance.
    let ov = open_notifications(&model);
    let DetailContent::List(l) = &ov.content else {
        panic!("expected a list");
    };
    let body: Vec<&crate::detail::DetailRow> = l.rows.iter().filter(|r| !r.header).collect();
    assert_eq!(body.len(), 3, "one per group plus the logs entry");
    assert!(
        body.iter()
            .filter(|r| r.text.contains("process failed"))
            .all(|r| r.actions.iter().any(|(k, _)| *k == 'x')),
        "both needs-you rows keep the dismiss key"
    );

    // Widened (`repo_scope: None`, what the `g` toggle hydrates): one group.
    model.sidebar_status.repo_scope = None;
    let h = headers(&model);
    assert_eq!(h, ["Needs you", "Logs"], "{h:?}");
}

#[test]
fn cursor_skips_group_headers() {
    // Walk the whole surface: the row cursor is never allowed to rest on a dim
    // group heading.
    let mut model = notif_model(
        vec![
            notif(NotificationKind::AgentFailed, "agent", "boom", 10),
            notif(NotificationKind::Assigned, "linear:1", "assigned", 20),
        ],
        vec![],
    );
    needs_you(&mut model, "/wt/a");
    let mut ov = open_notifications(&model);
    let on_header = |ov: &DetailOverlay| {
        let DetailContent::List(l) = &ov.content else {
            panic!("expected a list");
        };
        l.rows[ov.sel].header
    };
    assert!(!on_header(&ov), "opens on a selectable row, not a header");
    for _ in 0..8 {
        ov.handle_key(&KeyCode::DownArrow, Modifiers::NONE);
        assert!(!on_header(&ov), "cursor landed on a header");
    }
    for _ in 0..8 {
        ov.handle_key(&KeyCode::UpArrow, Modifiers::NONE);
        assert!(!on_header(&ov), "cursor landed on a header");
    }
}

#[test]
fn logs_group_is_a_single_quiet_entry_folding_dev_log_errors() {
    // In dev mode a `log:thegn` notification exists; it folds into the one Logs
    // entry (labelled by its message) and never becomes a red Alerts row — so a
    // flapping log can't stack duplicate rows.
    let model = notif_model(
        vec![notif(
            NotificationKind::LogError,
            "log:thegn",
            "2 errors in thegn.log",
            30,
        )],
        vec![err_line("boom")],
    );
    let ov = open_notifications(&model);
    let DetailContent::List(l) = &ov.content else {
        panic!("expected a list");
    };
    let headers: Vec<&str> = l
        .rows
        .iter()
        .filter(|r| r.header)
        .map(|r| r.text.as_str())
        .collect();
    assert_eq!(
        headers,
        ["Logs"],
        "a lone log error is Logs-only, never Alerts"
    );
    let log_rows: Vec<&str> = l
        .rows
        .iter()
        .filter(|r| matches!(r.enter, Some(DetailAction::ShowLog(_))))
        .map(|r| r.text.as_str())
        .collect();
    assert_eq!(log_rows, ["2 errors in thegn.log"]);
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
            &model,
            &StatusCtx::new_for_test(&hist),
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
            ..Default::default()
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
    for i in 0..60u64 {
        hist.push(&model.stats, (i + 1) * 1000);
    }
    for w in ["disk", "net", "gpu", "battery", "mem"] {
        let ov = open_detail_for(
            &BarItemId::Widget(w.into()),
            item_at(0),
            &model,
            &StatusCtx::new_for_test(&hist),
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
fn daemon_chip_opens_expanded_status_modal() {
    use crate::chrome::{DaemonChipState, DaemonStatus};
    let model = FrameModel {
        daemon_state: DaemonChipState::Persist,
        ..Default::default()
    };
    // A present daemon so the identity section renders pid/version/uptime.
    let daemon = DaemonStatus {
        present: true,
        pid: Some(4242),
        version: "9.9.9".into(),
        hostname: "box".into(),
        started_at_ms: 0,
        heartbeat_at: 60_000,
        ..Default::default()
    };
    let hist = TelemetryHistory::default();
    let sessions = crate::detail::DaemonSessions::default();
    let dcfg = thegn_core::config::DaemonConfig::default();
    let ctx = StatusCtx {
        hist: &hist,
        loop_perf: &crate::telemetry::LoopPerfHistory::default(),
        daemon: &daemon,
        sessions: &sessions,
        sessions_age_secs: None,
        daemon_cfg: &dcfg,
        screen: screen(),
        now_ms: 60_000, // 60s of daemon uptime
        uptime_secs: 125,
    };
    let ov = open_detail_for(
        &BarItemId::Badge(BarBadge::Persist),
        item_at(39), // bottom half → floats upward
        &model,
        &ctx,
    )
    .expect("daemon chip has a detail view");
    assert_eq!(ov.title(), "thegn status");
    assert!(matches!(ov.content, DetailContent::Sections(_)));
    // Anchored upward from a bottom-bar item.
    assert!(matches!(ov.placement, Placement::NearAbove(_)));
    // It renders without a contrast violation (readable on the theme).
    let mut s = Surface::new(120, 40);
    ov.render(&mut s, screen());
    assert!(seg::text_contrast_violations(&mut s, 3.0).is_empty());
}

/// Build a status modal against `screen`, with the given daemon session state.
#[cfg(test)]
fn status_modal_on(
    sessions: &crate::detail::DaemonSessions,
    screen_rect: Rect,
) -> crate::detail::DetailOverlay {
    use crate::chrome::{DaemonChipState, DaemonStatus};
    let model = FrameModel {
        daemon_state: DaemonChipState::Persist,
        daemon_panes: 2,
        pane_count: 3,
        ..Default::default()
    };
    let daemon = DaemonStatus {
        present: true,
        pid: Some(4242),
        version: "9.9.9".into(),
        hostname: "box".into(),
        endpoint: "/run/user/1000/thegn/daemon.sock".into(),
        scope: "/home/u/.local/state/thegn".into(),
        daemon_id: "9f3c2a11deadbeef".into(),
        started_at_ms: 0,
        heartbeat_at: 60_000,
        ..Default::default()
    };
    let hist = TelemetryHistory::default();
    let dcfg = thegn_core::config::DaemonConfig::default();
    let ctx = StatusCtx {
        hist: &hist,
        loop_perf: &crate::telemetry::LoopPerfHistory::default(),
        daemon: &daemon,
        sessions,
        sessions_age_secs: None,
        daemon_cfg: &dcfg,
        screen: screen_rect,
        now_ms: 60_000,
        uptime_secs: 125,
    };
    open_detail_for(
        &BarItemId::Badge(BarBadge::Persist),
        item_at(screen_rect.rows.saturating_sub(1)),
        &model,
        &ctx,
    )
    .expect("daemon chip has a detail view")
}

/// One daemon session, as `/v1/sessions` would report it.
#[cfg(test)]
fn session_info(id: &str, program: &str, attached: u32) -> thegn_svc::control::SessionInfo {
    thegn_svc::control::SessionInfo {
        id: id.into(),
        worktree: Some("/repo/app/feature-x".into()),
        program: program.into(),
        cwd: None,
        rows: 40,
        cols: 120,
        created_at_ms: 0,
        attached_clients: attached,
        lease_expires_at: None,
        pid: Some(77),
    }
}

/// Render an overlay and flatten it to text, for content assertions.
#[cfg(test)]
fn rendered_text(ov: &crate::detail::DetailOverlay, screen_rect: Rect) -> String {
    let mut s = Surface::new(screen_rect.cols, screen_rect.rows);
    ov.render(&mut s, screen_rect);
    s.screen_chars_to_string()
}

/// The regression guard for the reported bug: a daemon serving live sessions
/// must NOT render a zero count. The old modal derived counts from the lease
/// table, which records only detached sessions, so this read `0 (0 attached)`
/// while panes were being served.
#[test]
fn status_modal_reports_live_sessions_not_zero() {
    let sessions = crate::detail::DaemonSessions::Live(vec![
        session_info("9f3c2a11", "nvim", 1),
        session_info("2b7ec140", "zsh", 1),
        session_info("4410ffa2", "cargo", 0),
    ]);
    let sc = Rect::full(120, 40);
    let text = rendered_text(&status_modal_on(&sessions, sc), sc);
    assert!(text.contains("3 live"), "session count missing:\n{text}");
    assert!(
        text.contains("2 attached"),
        "attached count missing:\n{text}"
    );
    assert!(text.contains("1 warm"), "warm count missing:\n{text}");
    // The table itself renders, not just the summary.
    assert!(text.contains("9f3c2a11"), "session id missing:\n{text}");
    assert!(text.contains("nvim"), "program missing:\n{text}");
    // And never the old, always-wrong rendering.
    assert!(
        !text.contains("0 (0 attached)"),
        "stale zero count:\n{text}"
    );
}

/// "Never asked" and "asked, none" must not render the same.
#[test]
fn status_modal_distinguishes_probe_states() {
    use crate::detail::DaemonSessions;
    let sc = Rect::full(120, 40);
    for (state, want) in [
        (DaemonSessions::Probing, "probing"),
        (DaemonSessions::NoDaemon, "inline panes only"),
        (DaemonSessions::Live(vec![]), "none"),
    ] {
        let text = rendered_text(&status_modal_on(&state, sc), sc);
        assert!(
            text.contains(want),
            "{state:?} should say {want:?}:\n{text}"
        );
    }
}

/// Bug A guard: a stack taller than the screen must clamp its box AND stay
/// reachable by the scroll keys. `sections()` used to set `rows` to the full
/// content height, which made `scroll_max()` zero — the overflow was clipped by
/// the layer and unreachable.
#[test]
fn status_modal_scrolls_when_taller_than_screen() {
    let sessions = crate::detail::DaemonSessions::Live(
        (0..6)
            .map(|i| session_info(&format!("s{i}"), "zsh", 1))
            .collect(),
    );
    let sc = Rect::full(120, 24);
    let mut ov = status_modal_on(&sessions, sc);
    assert!(ov.rows < ov.content_rows(), "expected an overflowing stack");
    assert_eq!(
        ov.rows,
        sc.rows - 3,
        "box must clamp to the drawable height"
    );
    assert!(ov.scroll_max() > 0, "overflow must be reachable");
    for _ in 0..100 {
        ov.handle_key(&KeyCode::DownArrow, Modifiers::NONE);
    }
    assert_eq!(ov.scroll, ov.content_rows() - ov.rows);
}

/// Bug B guard: `viz::braille_graph` truncates to the first `w*2` samples rather
/// than resampling, and the series are right-aligned with "now" at the right —
/// so a sample count derived from the *requested* width would silently drop the
/// newest samples on any narrower terminal.
#[test]
fn status_modal_series_length_matches_the_clamped_width() {
    let sessions = crate::detail::DaemonSessions::default();
    let sc = Rect::full(74, 40); // clamps content to 74 - 6 = 68 cells
    let ov = status_modal_on(&sessions, sc);
    assert_eq!(ov.cols, 68, "content width must clamp to the screen");
    let DetailContent::Sections(d) = &ov.content else {
        panic!("expected a sections popup");
    };
    let graph = d
        .sections
        .iter()
        .find_map(|s| match s {
            Section::Graph(g) if g.label == "RSS" => Some(g),
            _ => None,
        })
        .expect("the RSS graph");
    assert_eq!(graph.series.len(), 68 * 2);
}

/// Readable on the theme at both the wide (2-column grid) and narrow
/// (single-column) layouts.
#[test]
fn status_modal_is_readable_at_every_width() {
    let sessions = crate::detail::DaemonSessions::Live(vec![session_info("a1", "zsh", 1)]);
    for cols in [120, 94, 74, 60] {
        let sc = Rect::full(cols, 40);
        let ov = status_modal_on(&sessions, sc);
        let mut s = Surface::new(cols, 40);
        ov.render(&mut s, sc);
        assert!(
            seg::text_contrast_violations(&mut s, 3.0).is_empty(),
            "contrast violation at {cols} cols"
        );
    }
}

/// `refresh_open` is title-guarded: it must leave any other overlay alone.
#[test]
fn refresh_open_only_touches_the_status_modal() {
    let hist = TelemetryHistory::default();
    let model = FrameModel::default();
    let ctx = StatusCtx::new_for_test(&hist);
    // No overlay at all.
    let mut slot = None;
    assert!(!crate::detail::status_modal::refresh_open(
        &mut slot, &model, &ctx
    ));
    // A different overlay stays untouched.
    let mut slot = Some(crate::detail::usage_loading(60, 10));
    assert!(!crate::detail::status_modal::refresh_open(
        &mut slot, &model, &ctx
    ));
    assert_eq!(slot.as_ref().map(|o| o.title()), Some("Usage"));
}

#[test]
fn tall_sections_popup_scrolls() {
    // A popup whose stacked height exceeds the SCREEN scrolls by row. The box
    // height must come from `sections()`'s own clamp — this test used to force
    // `ov.rows = 10` by hand, which meant the real clamp was never exercised and
    // a genuinely tall popup silently clipped with `scroll_max() == 0`.
    let secs = vec![Section::KeyVal(
        (0..30)
            .map(|i| (format!("k{i}"), format!("v{i}"), Tok::Slot(S::Text)))
            .collect(),
    )];
    let short = Rect::full(80, 13); // drawable content height = 13 - 3 = 10
    let mut ov = sections("Tall", 30, secs, Placement::Center, short);
    assert_eq!(ov.rows, 10, "box height must clamp to the screen");
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

#[test]
fn sections_popup_that_fits_does_not_scroll() {
    let secs = vec![Section::KeyVal(
        (0..5)
            .map(|i| (format!("k{i}"), format!("v{i}"), Tok::Slot(S::Text)))
            .collect(),
    )];
    let ov = sections("Short", 30, secs, Placement::Center, Rect::full(80, 40));
    assert_eq!(ov.rows, 5);
    assert_eq!(ov.scroll_max(), 0);
}

#[test]
fn grid_height_is_row_major_ceil() {
    let cells: Vec<_> = (0..8)
        .map(|i| (format!("k{i}"), format!("v{i}"), Tok::Slot(S::Text)))
        .collect();
    assert_eq!(Section::Grid { cols: 3, cells }.height(), 3);
    let one = vec![("k".to_string(), "v".to_string(), Tok::Slot(S::Text))];
    assert_eq!(
        Section::Grid {
            cols: 2,
            cells: one
        }
        .height(),
        1
    );
    // A degenerate column count must not divide by zero.
    assert_eq!(
        Section::Grid {
            cols: 0,
            cells: vec![],
        }
        .height(),
        0
    );
}

#[test]
fn grid_sizes_each_column_independently() {
    // Column 0 holds short values, column 1 a very long one. Column 1's width
    // must not pad column 0 (that is the whole point of the grid over KeyVal).
    let cells = vec![
        ("a".into(), "1".into(), Tok::Slot(S::Text)),
        ("b".into(), "x".repeat(40), Tok::Slot(S::Text)),
        ("cc".into(), "22".into(), Tok::Slot(S::Text)),
        ("d".into(), "y".into(), Tok::Slot(S::Text)),
    ];
    let (kw, vw) = crate::sections::grid_widths(2, &cells);
    assert_eq!(kw, vec![2, 1], "keys: max('a','cc')=2, max('b','d')=1");
    assert_eq!(vw, vec![2, 40], "values sized per column");
}

#[test]
fn grid_uses_display_width_not_char_count() {
    // A CJK glyph is two cells wide; padding by char count would misalign.
    let cells = vec![
        ("k".into(), "日本".into(), Tok::Slot(S::Text)),
        ("k2".into(), "ab".into(), Tok::Slot(S::Text)),
    ];
    let (_, vw) = crate::sections::grid_widths(1, &cells);
    assert_eq!(vw, vec![4], "two wide glyphs occupy four cells");
}

#[test]
fn grid_drops_columns_that_would_overflow() {
    // Three columns of ~20 cells each into a 24-cell box: only the first fits,
    // and nothing may be drawn past the clip.
    let cells: Vec<_> = (0..6)
        .map(|i| (format!("key{i}"), "v".repeat(16), Tok::Slot(S::Text)))
        .collect();
    let sec = Section::Grid { cols: 3, cells };
    let secs = vec![sec];
    let sc = Rect::full(40, 20);
    let ov = sections("Grid", 24, secs, Placement::Center, sc);
    let mut s = Surface::new(40, 20);
    ov.render(&mut s, sc);
    let text = s.screen_chars_to_string();
    assert!(text.contains("key0"), "first column must draw:\n{text}");
    assert!(
        !text.contains("key1"),
        "second column must be dropped, not wrapped:\n{text}"
    );
    // Still readable — a dropped column must not corrupt the row.
    assert!(seg::text_contrast_violations(&mut s, 3.0).is_empty());
}
