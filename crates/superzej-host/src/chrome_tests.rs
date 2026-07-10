use super::*;
use crate::emulator::AlacrittyEmulator;
use crate::layout;

fn lines(s: &Surface) -> Vec<String> {
    s.screen_chars_to_string()
        .lines()
        .map(|l| l.to_string())
        .collect()
}

/// Build a minimal sidebar row for renderer tests.
fn row(kind: crate::sidebar::RowKind, label: &str) -> crate::sidebar::SidebarRow {
    crate::sidebar::SidebarRow {
        kind,
        depth: (kind != crate::sidebar::RowKind::Workspace) as u8,
        label: label.into(),
        workspace_slug: "app".into(),
        tab_target: None,
        active: false,
        worktree_path: None,
        pin_key: label.into(),
        branch: None,
        git: None,
        agent: None,
        sandbox_backend: None,
        env_name: None,
        activity: crate::sidebar::ActivityState::None,
        visible: true,
        collapsed: false,
        dir: false,
        pr_count: None,
        pr_number: None,
        unread_count: 0,
        alert_count: 0,
        disk_bytes: None,
        target_bytes: None,
        terminal_connection: None,
        attention: None,
        mq_status: None,
    }
}

#[test]
fn redraw_pane_card_restores_a_nibbled_border() {
    use crate::center::CenterTree;
    let tree = CenterTree::single(1);
    let area = Rect {
        x: 0,
        y: 0,
        cols: 20,
        rows: 5,
    };
    let frames = tree.layout_framed(area);
    let (_, _frame, content) = frames[0];
    let right = area.x + area.cols - 1;
    let model = FrameModel {
        center_focused: true,
        ..Default::default()
    };
    let mut s = Surface::new(20, 5);
    redraw_pane_card(&mut s, &frames, 1, 1, &model, &|_| String::new());
    assert_eq!(
        s.screen_cells()[2][right].str(),
        "\u{2502}",
        "card border drawn"
    );
    // Simulate content overflowing into the border: a wide glyph straddling
    // the last content column and the border column.
    s.add_change(Change::CursorPosition {
        x: Position::Absolute(content.x + content.cols - 1),
        y: Position::Absolute(2),
    });
    s.add_change(Change::Text("\u{6f22}".into()));
    assert_ne!(
        s.screen_cells()[2][right].str(),
        "\u{2502}",
        "the wide glyph nibbled the border (the bug)"
    );
    // Repainting the card heals every interior border row.
    redraw_pane_card(&mut s, &frames, 1, 1, &model, &|_| String::new());
    for y in (area.y + 1)..(area.y + area.rows - 1) {
        assert_eq!(
            s.screen_cells()[y][right].str(),
            "\u{2502}",
            "border solid again at row {y}"
        );
    }
}

#[test]
fn redraw_pane_card_skips_panes_without_a_card() {
    use crate::center::CenterTree;
    let frames = CenterTree::single(1).layout_framed(Rect {
        x: 0,
        y: 0,
        cols: 20,
        rows: 5,
    });
    let mut s = Surface::new(20, 5);
    // A pane id not in `frames` (e.g. the drawer) is a no-op: nothing drawn.
    redraw_pane_card(&mut s, &frames, 99, 99, &FrameModel::default(), &|_| {
        String::new()
    });
    assert!(
        s.screen_chars_to_string().trim().is_empty(),
        "no card drawn for an absent pane"
    );
}

#[test]
fn sidebar_selection_bar_persists_dimmed_when_unfocused() {
    let rect = Rect {
        x: 0,
        y: 0,
        cols: 24,
        rows: 6,
    };
    use crate::sidebar::RowKind;
    let model = FrameModel {
        sidebar_rows: vec![
            row(RowKind::Workspace, "app"),
            row(RowKind::Worktree, "home"),
        ],
        sidebar_selected: 1,
        sidebar_focused: true,
        ..Default::default()
    };
    // The cursor row sits at list_y (header + blank gap) + workspace row.
    let bar_y = rect.y + 2 + 1;
    let mut s = Surface::new(24, 6);
    draw_sidebar(&mut s, rect, &model);
    assert!(
        s.screen_chars_to_string().contains('\u{2590}'),
        "focused cursor bar present"
    );
    let focused_fg = s.screen_cells()[bar_y][0].attrs().foreground();

    // Unfocused: the bar persists so the selection stays visible...
    let mut unfocused = model.clone();
    unfocused.sidebar_focused = false;
    let mut s2 = Surface::new(24, 6);
    draw_sidebar(&mut s2, rect, &unfocused);
    assert!(
        s2.screen_chars_to_string().contains('\u{2590}'),
        "cursor bar persists when unfocused"
    );
    // ...but dims to a distinct tone so focus is still legible.
    let unfocused_fg = s2.screen_cells()[bar_y][0].attrs().foreground();
    assert_ne!(
        focused_fg, unfocused_fg,
        "unfocused bar dims to a different color"
    );
}

#[test]
fn sidebar_selection_bar_spans_expanded_detail_line() {
    use crate::sidebar::RowKind;
    let rect = Rect {
        x: 0,
        y: 0,
        cols: 24,
        rows: 8,
    };
    // A worktree with disk metadata expands the cursor row to two lines.
    let mut wt = row(RowKind::Worktree, "home");
    wt.disk_bytes = Some(1024);
    let model = FrameModel {
        sidebar_rows: vec![row(RowKind::Workspace, "app"), wt],
        sidebar_selected: 1,
        sidebar_focused: true,
        ..Default::default()
    };
    let frame = build_sidebar(&model, rect, model.sidebar_scroll);
    let cursor = frame.rows.iter().find(|p| p.visible_index == 1).unwrap();
    assert_eq!(cursor.height, 2, "detail line expands the cursor row");

    let mut s = Surface::new(24, 8);
    draw_sidebar(&mut s, rect, &model);
    let cells = s.screen_cells();
    // The bar paints col 0 of both rows of the placement.
    assert_eq!(cells[cursor.y][0].str(), "\u{2590}", "bar on name line");
    assert_eq!(
        cells[cursor.y + 1][0].str(),
        "\u{2590}",
        "bar on detail line"
    );
}

#[test]
fn statusbar_items_includes_badges_so_they_are_navigable() {
    // The core fix: badges (notifications/agent/…) must be in the same
    // ordered item list nav steps, after the config widgets — otherwise the
    // cursor can never reach them (the original bug).
    let model = FrameModel {
        bars: superzej_core::config::BarsConfig {
            bottom_right: vec!["loc".into()],
            ..Default::default()
        },
        loc: Some(superzej_core::loc::LocReport::total_only(1234)),
        panel: crate::panel::PanelData {
            alert_notifications: 2,
            ..Default::default()
        },
        agent_activity: Some(AgentActivity::default()),
        zoomed: true,
        ..Default::default()
    };
    let ids: Vec<BarItemId> = statusbar_items(&model)
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    assert_eq!(ids[0], BarItemId::Widget("loc".into()), "widgets first");
    assert!(ids.contains(&BarItemId::Badge(BarBadge::Notifications)));
    assert!(ids.contains(&BarItemId::Badge(BarBadge::Agent)));
    assert!(ids.contains(&BarItemId::Badge(BarBadge::Zoom)));
}

#[test]
fn statusbar_notify_chip_reflects_dnd_and_mode() {
    // No DND, no mode ⇒ no notify chip.
    let plain = FrameModel::default();
    let text = |m: &FrameModel| -> String {
        statusbar_items(m)
            .into_iter()
            .flat_map(|(_, segs)| segs)
            .map(|s| s.text)
            .collect()
    };
    assert!(!text(&plain).contains("dnd"));

    // Active mode ⇒ a mode chip.
    let moded = FrameModel {
        notify_mode: "focus".into(),
        ..Default::default()
    };
    assert!(text(&moded).contains("focus"));

    // DND wins and shows the mode alongside it.
    let dnd = FrameModel {
        notify_dnd: true,
        notify_mode: "focus".into(),
        ..Default::default()
    };
    let t = text(&dnd);
    assert!(t.contains("dnd"));
    assert!(t.contains("focus"));
}

#[test]
fn masthead_fit_sheds_items_when_narrow() {
    // Navigation enumerates exactly what draw shows, so the width-degraded
    // drop must shrink the navigable set too (no cursor on a hidden item).
    let model = FrameModel {
        bars: superzej_core::config::BarsConfig {
            top_right: vec!["cpu".into(), "mem".into(), "swap".into(), "date".into()],
            ..Default::default()
        },
        stats: superzej_metrics::StatsSnapshot {
            cpu_pct: Some(99),
            mem_gib: Some((4.0, 16.0)),
            swap_gib: Some((1.0, 8.0)),
            ..Default::default()
        },
        ..Default::default()
    };
    let wide = crate::masthead::masthead_layout(&model, 200, None).right_spans;
    let narrow = crate::masthead::masthead_layout(&model, 24, None).right_spans;
    assert_eq!(wide.len(), 4);
    assert!(
        narrow.len() < wide.len(),
        "narrow={} wide={}",
        narrow.len(),
        wide.len()
    );
    // cpu/mem survive longest (softest stats shed first).
    assert!(
        narrow
            .iter()
            .any(|(id, _, _)| *id == BarItemId::Widget("cpu".into()))
    );
}

#[test]
fn masthead_left_keeps_active_chip_and_never_overlaps_when_narrow() {
    // The top bar degrades like the bottom bar: the active app-tab chip stays,
    // the breadcrumb elides with `…`, and the right stats cluster never runs
    // into the left content (no overlap, nothing clipped mid-glyph).
    let model = FrameModel {
        bars: superzej_core::config::BarsConfig {
            top_left: vec!["brand".into(), "clock".into()],
            top_right: vec!["cpu".into(), "mem".into(), "date".into()],
            ..Default::default()
        },
        app_tabs: vec!["work".into(), "chat".into(), "observe".into()],
        active_app: 1,
        stats: superzej_metrics::StatsSnapshot {
            cpu_pct: Some(50),
            mem_gib: Some((4.0, 16.0)),
            ..Default::default()
        },
        ..Default::default()
    };
    for cols in [40usize, 60, 96, 160] {
        let lay = crate::masthead::masthead_layout(&model, cols, None);
        // The active chip ("chat") is always present.
        let left_text: String = lay.left.iter().map(|s| s.text.as_str()).collect();
        assert!(
            left_text.contains("chat"),
            "active chip missing at cols={cols}: {left_text:?}"
        );
        // Left + right + the split gutter fit within the width — no overlap.
        let lw = crate::seg::seg_width(&lay.left);
        let rw = crate::seg::seg_width(&lay.right);
        assert!(
            lw + rw + usize::from(rw > 0) <= cols,
            "overlap at cols={cols}: left={lw} right={rw}"
        );
    }
}

#[test]
fn masthead_item_spans_match_painted_right_cluster() {
    // Hit-test Rects must land exactly on the painted stat cells: the cluster is
    // right-aligned, so the first span sits at cols - right_width.
    let model = FrameModel {
        bars: superzej_core::config::BarsConfig {
            top_right: vec!["cpu".into(), "mem".into()],
            ..Default::default()
        },
        stats: superzej_metrics::StatsSnapshot {
            cpu_pct: Some(50),
            mem_gib: Some((4.0, 16.0)),
            ..Default::default()
        },
        ..Default::default()
    };
    let chrome = layout::compute(160, 10, false, false);
    let spans = crate::masthead::masthead_item_spans(&model, &chrome);
    assert_eq!(spans.len(), 2);
    // Spans are in display order and don't overlap.
    assert!(spans[0].1.x + spans[0].1.cols <= spans[1].1.x);
    // The cluster hugs the right edge (within the 1-col trailing margin).
    let cols = chrome.masthead.cols;
    let last = &spans[1].1;
    assert!(last.x + last.cols <= cols);
    assert!(last.x + last.cols >= cols - 2);
}

#[test]
fn workspace_slot_digits_skip_unswitchable_and_count_in_order() {
    let rect = Rect {
        x: 0,
        y: 0,
        cols: 24,
        rows: 10,
    };
    use crate::sidebar::RowKind;
    // a: switchable → slot 1. b: live-fallback (no path) → no slot.
    // c: switchable → slot 2 (the counter skips b, matching
    // sidebar_workspace_order's filter_map).
    let mut a = row(RowKind::Workspace, "alpha");
    a.worktree_path = Some("/repo/alpha".into());
    let b = row(RowKind::Workspace, "bravo"); // worktree_path: None
    let mut c = row(RowKind::Workspace, "charlie");
    c.worktree_path = Some("/repo/charlie".into());
    let model = FrameModel {
        sidebar_rows: vec![a, b, c],
        // Digits are revealed only while the sidebar is focused.
        sidebar_focused: true,
        ..Default::default()
    };
    let mut s = Surface::new(24, 10);
    draw_sidebar(&mut s, rect, &model);
    let text = s.screen_chars_to_string();
    assert!(
        text.contains("1 \u{25be} alpha"),
        "alpha is slot 1: {text:?}"
    );
    // bravo gets no digit (unswitchable) — only its bare caret + label.
    assert!(text.contains("\u{25be} bravo"), "bravo present: {text:?}");
    assert!(
        !text.contains("2 \u{25be} bravo") && !text.contains("1 \u{25be} bravo"),
        "bravo has no slot digit: {text:?}"
    );
    assert!(
        text.contains("2 \u{25be} charlie"),
        "charlie is slot 2 (bravo skipped): {text:?}"
    );
}

#[test]
fn quick_jump_digits_revealed_only_while_sidebar_focused() {
    let rect = Rect {
        x: 0,
        y: 0,
        cols: 24,
        rows: 6,
    };
    use crate::sidebar::{RowKind, RowTarget};
    // A switchable workspace carries a `worktree_path`; a worktree with a
    // Tab target gets an Alt+N jump digit.
    let mut ws = row(RowKind::Workspace, "app");
    ws.worktree_path = Some("/repo/app".into());
    let mut wt = row(RowKind::Worktree, "home");
    wt.tab_target = Some(RowTarget::Tab(0, 0));
    let model = FrameModel {
        sidebar_rows: vec![ws, wt],
        sidebar_selected: 0,
        sidebar_focused: true,
        ..Default::default()
    };
    let mut s = Surface::new(24, 6);
    draw_sidebar(&mut s, rect, &model);
    let focused = s.screen_chars_to_string();
    // Focused: workspace shows its Ctrl+N digit, worktree its Alt+N digit.
    assert!(
        focused.contains("1 \u{25be} app"),
        "workspace digit while focused: {focused:?}"
    );
    assert!(
        focused.contains("1 "),
        "worktree digit while focused: {focused:?}"
    );

    // Unfocused: the resting view is decluttered — no digits at all.
    let mut unfocused = model.clone();
    unfocused.sidebar_focused = false;
    let mut s2 = Surface::new(24, 6);
    draw_sidebar(&mut s2, rect, &unfocused);
    let text2 = s2.screen_chars_to_string();
    assert!(
        text2.contains("\u{25be} app") && !text2.contains("1 \u{25be} app"),
        "no workspace digit when unfocused: {text2:?}"
    );
}

#[test]
fn worktree_rows_show_alt_jump_digits_in_order_when_focused() {
    use crate::sidebar::{RowKind, RowTarget};
    let rect = Rect {
        x: 0,
        y: 0,
        cols: 24,
        rows: 8,
    };
    let mut ws = row(RowKind::Workspace, "app");
    ws.worktree_path = Some("/repo/app".into());
    let mut a = row(RowKind::Worktree, "alpha");
    a.tab_target = Some(RowTarget::Tab(0, 0));
    let mut b = row(RowKind::Worktree, "bravo");
    b.tab_target = Some(RowTarget::Tab(1, 0));
    let model = FrameModel {
        sidebar_rows: vec![ws, a, b],
        // Select the workspace so neither worktree expands (keeps rows 1-line).
        sidebar_selected: 0,
        sidebar_focused: true,
        ..Default::default()
    };
    let mut s = Surface::new(24, 8);
    draw_sidebar(&mut s, rect, &model);
    let text = s.screen_chars_to_string();
    assert!(
        text.contains("1 ") && text.contains("alpha"),
        "alpha is worktree slot 1: {text:?}"
    );
    assert!(
        text.contains("2 ") && text.contains("bravo"),
        "bravo is worktree slot 2: {text:?}"
    );
}

#[test]
fn sidebar_renders_glyphs_caret_and_dirty() {
    use crate::sidebar::{ActivityState, GitGlyphs, RowKind};
    let rect = Rect {
        x: 0,
        y: 0,
        cols: 30,
        rows: 8,
    };
    let mut ws = row(RowKind::Workspace, "app");
    ws.collapsed = false;
    let mut wt = row(RowKind::Worktree, "feat");
    wt.git = Some(GitGlyphs {
        dirty: true,
        ahead: 2,
        behind: 1,
    });
    // The agent/app indicator was dropped from the row entirely: a set
    // `agent` must render no trailing glyph.
    wt.agent = Some("claude".into());
    wt.activity = ActivityState::Active;
    let model = FrameModel {
        sidebar_rows: vec![ws, wt],
        sidebar_selected: 0,
        sidebar_focused: true,
        ..Default::default()
    };
    let mut s = Surface::new(30, 8);
    draw_sidebar(&mut s, rect, &model);
    let text = s.screen_chars_to_string();
    assert!(text.contains('\u{25be}'), "expanded caret ▾: {text:?}"); // expanded workspace
    assert!(text.contains("feat"));
    assert!(text.contains('\u{2191}'), "ahead glyph ↑: {text:?}");
    assert!(text.contains('\u{2193}'), "behind glyph ↓: {text:?}");
    // No agent/app glyph on the worktree row: "claude" would render as a
    // trailing letter-default 'C', which must not appear on the "feat" line.
    let feat_line = text
        .lines()
        .find(|l| l.contains("feat"))
        .expect("feat row present");
    assert!(
        !feat_line.contains('C'),
        "agent/app indicator must be gone: {feat_line:?}"
    );
}

#[test]
fn sidebar_renders_badges_for_pr_unread_and_alert() {
    use crate::sidebar::RowKind;
    let rect = Rect {
        x: 0,
        y: 0,
        cols: 40,
        rows: 6,
    };
    let mut ws = row(RowKind::Workspace, "app");
    ws.collapsed = false;
    let mut wt = row(RowKind::Worktree, "feat");
    wt.pr_count = Some(2);
    wt.unread_count = 3;
    wt.alert_count = 1;
    let model = FrameModel {
        sidebar_rows: vec![ws, wt],
        // Select the worktree so its two-tier detail line (PR/unread)
        // expands; the alert badge is always-on on the primary line.
        sidebar_selected: 1,
        ..Default::default()
    };
    let mut s = Surface::new(40, 6);
    draw_sidebar(&mut s, rect, &model);
    let text = s.screen_chars_to_string();
    assert!(text.contains('\u{2b21}'), "PR badge glyph ⬡: {text:?}");
    assert!(text.contains('\u{2709}'), "unread badge glyph ✉: {text:?}");
    assert!(text.contains('\u{26a0}'), "alert badge glyph ⚠: {text:?}");
    // Counts render alongside the glyphs.
    assert!(text.contains('2') && text.contains('3') && text.contains('1'));
}

#[test]
fn sidebar_renders_disk_size_badge() {
    use crate::sidebar::RowKind;
    let rect = Rect {
        x: 0,
        y: 0,
        cols: 40,
        rows: 6,
    };
    let mut ws = row(RowKind::Workspace, "app");
    ws.collapsed = false;
    let mut wt = row(RowKind::Worktree, "feat");
    wt.disk_bytes = Some(70 * 1024 * 1024 * 1024); // 70G
    wt.target_bytes = Some(60 * 1024 * 1024 * 1024);
    let model = FrameModel {
        sidebar_rows: vec![ws, wt],
        // The disk size rides the expanded detail line of the selected row.
        sidebar_selected: 1,
        ..Default::default()
    };
    let mut s = Surface::new(40, 6);
    draw_sidebar(&mut s, rect, &model);
    let text = s.screen_chars_to_string();
    assert!(
        text.contains("70G"),
        "size badge shows human size: {text:?}"
    );
}

#[test]
fn masthead_disk_widget_shows_free_pct_with_inverted_colors() {
    let mut model = FrameModel::default();
    // High free → normal (dim); low free → red; mid → amber. Defaults:
    // warn 15, critical 5.
    model.stats.disk_free_pct = Some(72);
    let hi = masthead_widget("disk", &model).expect("disk widget present");
    assert!(hi.text.contains("72%"), "shows free %: {:?}", hi.text);
    assert_eq!(hi.fg, col(S::Dim), "ample free → dim");

    model.stats.disk_free_pct = Some(12);
    assert_eq!(
        masthead_widget("disk", &model).unwrap().fg,
        theme_color(theme::AMBER),
        "low free → amber"
    );

    model.stats.disk_free_pct = Some(3);
    assert_eq!(
        masthead_widget("disk", &model).unwrap().fg,
        theme_color(theme::RED),
        "critically low free → red"
    );

    // Absent until sampled.
    model.stats.disk_free_pct = None;
    assert!(masthead_widget("disk", &model).is_none());
}

#[test]
fn bottombar_disk_widget_shows_active_worktree_size() {
    let mut model = FrameModel::default();
    assert!(
        bottombar_widget("disk", &model).is_none(),
        "hidden when size unknown"
    );
    model.active_worktree_disk = Some(7 * 1024 * 1024 * 1024); // 7G
    let wdg = bottombar_widget("disk", &model).expect("disk widget present");
    assert_eq!(wdg.text, "7GB");
}

#[test]
fn statusbar_disk_badge_trips_on_low_free_space() {
    let chrome = layout::compute(160, 10, false, false);
    // Default thresholds: warn 15%, critical 10% free.
    let mk = |free_pct: Option<u8>| -> String {
        let mut model = FrameModel::default();
        model.stats.disk_free_pct = free_pct;
        let mut s = Surface::new(160, 10);
        draw_statusbar(&mut s, chrome.statusbar, &model);
        s.screen_chars_to_string()
    };
    // Ample free → silent (clean is quiet).
    assert!(!mk(Some(72)).contains('\u{26c1}'), "silent when ample free");
    // At/below the warn line → the ⛁ chip appears.
    assert!(mk(Some(12)).contains('\u{26c1}'), "trips at low free");
    // Critically low → still shows (color asserted elsewhere).
    assert!(mk(Some(3)).contains('\u{26c1}'), "trips at critical free");
    // Not yet sampled → silent.
    assert!(!mk(None).contains('\u{26c1}'), "silent until sampled");
}

#[test]
fn sidebar_omits_zero_badges() {
    use crate::sidebar::RowKind;
    let rect = Rect {
        x: 0,
        y: 0,
        cols: 40,
        rows: 6,
    };
    let mut ws = row(RowKind::Workspace, "app");
    ws.collapsed = false;
    let mut wt = row(RowKind::Worktree, "feat");
    // All zero/none: no badges should render.
    wt.pr_count = Some(0);
    wt.unread_count = 0;
    wt.alert_count = 0;
    let model = FrameModel {
        sidebar_rows: vec![ws, wt],
        ..Default::default()
    };
    let mut s = Surface::new(40, 6);
    draw_sidebar(&mut s, rect, &model);
    let text = s.screen_chars_to_string();
    assert!(
        !text.contains('\u{2b21}'),
        "no PR badge when zero: {text:?}"
    );
    assert!(
        !text.contains('\u{2709}'),
        "no unread badge when zero: {text:?}"
    );
    assert!(
        !text.contains('\u{26a0}'),
        "no alert badge when zero: {text:?}"
    );
}

#[test]
fn dir_workspace_renders_folder_glyph() {
    use crate::sidebar::RowKind;
    let rect = Rect {
        x: 0,
        y: 0,
        cols: 30,
        rows: 4,
    };
    let mut repo_ws = row(RowKind::Workspace, "repo-ws");
    repo_ws.dir = false;
    let mut dir_ws = row(RowKind::Workspace, "scratch");
    dir_ws.dir = true;
    let model = FrameModel {
        sidebar_rows: vec![repo_ws, dir_ws],
        ..Default::default()
    };
    let mut s = Surface::new(30, 4);
    draw_sidebar(&mut s, rect, &model);
    let text = s.screen_chars_to_string();
    // The dir workspace carries the folder glyph; the repo one does not.
    assert!(text.contains('\u{1f4c1}'), "dir folder glyph 📁: {text:?}");
    assert!(text.contains("scratch") && text.contains("repo-ws"));
}

/// A tall sidebar with one workspace + several worktrees; `n` rows total.
fn many_rows(n: usize) -> Vec<crate::sidebar::SidebarRow> {
    use crate::sidebar::RowKind;
    let mut rows = vec![row(RowKind::Workspace, "app")];
    for i in 0..n {
        rows.push(row(RowKind::Worktree, &format!("wt{i}")));
    }
    rows
}

#[test]
fn nav_hints_footer_shown_only_when_focused_with_spare_room() {
    // The navigation-tips footer rides the empty tail of the column, so it must
    // appear only when (a) the sidebar is focused and (b) the laid-out rows
    // leave genuine blank space below them — never pushing a row or scrolling.
    let tall = Rect {
        x: 0,
        y: 0,
        cols: 30,
        rows: 40,
    };
    let rows = many_rows(3); // a short list under a very tall column → lots of tail

    // Focused + spare room → footer present, anchored to the tail, above metrics.
    let focused = FrameModel {
        sidebar_rows: rows.clone(),
        sidebar_selected: 0,
        sidebar_focused: true,
        ..Default::default()
    };
    let frame = build_sidebar(&focused, tall, focused.sidebar_scroll);
    let hints = frame
        .hints
        .expect("focused tall column reveals the hints footer");
    // It sits below the last rendered row (fills the blank tail, no overlap).
    let last_row_bottom = frame.rows.iter().map(|p| p.y + p.height).max().unwrap_or(0);
    assert!(
        hints.y >= last_row_bottom,
        "footer clears the rows: {hints:?}"
    );
    assert_eq!(
        hints.y + hints.rows,
        tall.y + tall.rows,
        "footer anchors to the bottom"
    );

    // Unfocused → no footer (resting view stays uncluttered).
    let unfocused = FrameModel {
        sidebar_focused: false,
        ..focused.clone()
    };
    assert!(
        build_sidebar(&unfocused, tall, unfocused.sidebar_scroll)
            .hints
            .is_none(),
        "an unfocused sidebar shows no hints"
    );

    // Focused but the list fills the column → no blank tail → no footer.
    let short = Rect {
        x: 0,
        y: 0,
        cols: 30,
        rows: 8,
    };
    let packed = FrameModel {
        sidebar_rows: many_rows(20),
        sidebar_focused: true,
        ..Default::default()
    };
    assert!(
        build_sidebar(&packed, short, packed.sidebar_scroll)
            .hints
            .is_none(),
        "a full list leaves no room for hints"
    );
}

#[test]
fn build_sidebar_and_click_hit_test_round_trip() {
    // Every rendered row's [y, y+height) maps back to its own visible index
    // via `sidebar_hits` — the contract that keeps clicks aligned with paint
    // even when the cursor row expands to two lines.
    let rect = Rect {
        x: 0,
        y: 0,
        cols: 40,
        rows: 12,
    };
    let mut rows = many_rows(4);
    // Give the selected worktree (visible index 2) secondary metadata so it
    // expands to a 2-row placement.
    rows[2].disk_bytes = Some(1024);
    let model = FrameModel {
        sidebar_rows: rows,
        sidebar_selected: 2,
        sidebar_focused: true,
        ..Default::default()
    };
    let frame = build_sidebar(&model, rect, model.sidebar_scroll);
    let cursor_row = frame.rows.iter().find(|p| p.visible_index == 2).unwrap();
    assert_eq!(cursor_row.height, 2, "selected row with detail expands");
    assert!(cursor_row.cursor_bar, "focused cursor draws the bar");

    let hits = sidebar_hits(&model, rect);
    for p in &frame.rows {
        for dy in 0..p.height {
            let found = hits
                .iter()
                .find(|(_, y, h)| (p.y + dy) >= *y && (p.y + dy) < *y + *h)
                .map(|(i, _, _)| *i);
            assert_eq!(
                found,
                Some(p.visible_index),
                "click on row {} line {dy} resolves to itself",
                p.visible_index
            );
        }
    }
}

#[test]
fn build_sidebar_scrolls_to_keep_cursor_visible() {
    // More rows than fit: the selected row must always be laid out, no
    // matter how far down it is (the old renderer left it unreachable).
    let rect = Rect {
        x: 0,
        y: 0,
        cols: 30,
        rows: 6, // header + blank leaves ~4 list rows
    };
    let rows = many_rows(20);
    for sel in [0usize, 5, 12, 20] {
        let model = FrameModel {
            sidebar_rows: rows.clone(),
            sidebar_selected: sel,
            ..Default::default()
        };
        let frame = build_sidebar(&model, rect, model.sidebar_scroll);
        assert!(
            frame.rows.iter().any(|p| p.visible_index == sel),
            "selected row {sel} is rendered (scroll={})",
            frame.scroll
        );
    }
}

#[test]
fn clamp_sidebar_scroll_keeps_cursor_in_window() {
    // Uniform 1-row heights, a 4-row window.
    let heights = vec![1usize; 10];
    assert_eq!(clamp_sidebar_scroll(&heights, 0, 4, 0), 0);
    assert_eq!(clamp_sidebar_scroll(&heights, 6, 4, 0), 3);
    assert_eq!(clamp_sidebar_scroll(&heights, 2, 4, 8), 2);
    assert_eq!(clamp_sidebar_scroll(&[], 0, 4, 0), 0);
    assert_eq!(clamp_sidebar_scroll(&heights, 0, 0, 0), 0);
}

#[test]
fn rail_mode_renders_dots_not_full_rows() {
    use crate::sidebar::{ActivityState, RowKind};
    let rect = Rect {
        x: 0,
        y: 0,
        cols: 4,
        rows: 8,
    };
    let mut wt = row(RowKind::Worktree, "feature");
    wt.activity = ActivityState::Active;
    let model = FrameModel {
        sidebar_rows: vec![row(RowKind::Workspace, "app"), wt],
        sidebar_rail: true,
        ..Default::default()
    };
    let mut s = Surface::new(4, 8);
    draw_sidebar(&mut s, rect, &model);
    let text = s.screen_chars_to_string();
    // The rail shows the worktree's initial but not the full label or the
    // "WORKSPACES" header.
    assert!(text.contains('f'), "rail worktree initial: {text:?}");
    assert!(
        !text.contains("WORKSPACES"),
        "rail omits the header: {text:?}"
    );
    assert!(
        !text.contains("feature"),
        "rail omits full labels: {text:?}"
    );
}

#[test]
fn clear_frame_removes_stale_cells_from_logical_surface() {
    let mut s = Surface::new(20, 3);
    draw_text(&mut s, 0, 0, "STALE", col(S::Text), col(S::Bg1), 20);
    assert!(s.screen_chars_to_string().contains("STALE"));

    clear_frame(&mut s);
    let text = s.screen_chars_to_string();
    assert!(!text.contains("STALE"), "logical clear removes old cells");
}
#[test]
fn plugin_view_is_host_rendered_with_semantic_roles() {
    use superzej_core::plugin_api::{Span, StyleRole, View};

    let mut s = Surface::new(20, 1);
    let view = View::line([
        Span::styled("ok", StyleRole::Accent),
        Span::styled(" warn", StyleRole::Warning),
    ]);
    draw_plugin_view(
        &mut s,
        Rect {
            x: 0,
            y: 0,
            cols: 20,
            rows: 1,
        },
        &view,
        theme::TEAL,
    );

    let text = s.screen_chars_to_string();
    assert!(text.contains("ok warn"), "{text:?}");
}

#[test]
fn center_tabs_show_worktree_label_and_chips() {
    let mut s = Surface::new(80, 2);
    let model = FrameModel {
        worktree: "washu/home".into(),
        tabs: vec!["1".into(), "2".into()],
        active_tab: 1,
        ..Default::default()
    };
    let strip = Rect {
        x: 0,
        y: 1,
        cols: 80,
        rows: 1,
    };
    draw_center_tabs(&mut s, strip, &model);
    let row = &lines(&s)[1];
    // The slug prefix renders uppercased, the leaf in accent, the chips
    // as padded pills after the label.
    assert!(row.contains("WASHU \u{25b8} home"), "{row:?}");
    let leaf_at = row.find(" home").unwrap();
    assert!(row[leaf_at..].contains(" 1 "), "{row:?}");
    assert!(row[leaf_at..].contains(" 2 "), "{row:?}");
    // Hit-test agrees with the rendered chip positions: the spans say
    // where chips draw, and a hit inside the first span returns tab 0.
    let spans = strip_chip_spans(&model, strip);
    assert_eq!(spans.len(), 2);
    assert_eq!(center_tab_hit(&model, strip, spans[0].0), Some(0));
    assert_eq!(center_tab_hit(&model, strip, spans[1].0 + 1), Some(1));
    assert_eq!(center_tab_hit(&model, strip, 0), None);
    // And the drawn cell at the first span really is the chip text.
    let chip0: String = row.chars().skip(spans[0].0).take(spans[0].1).collect();
    assert_eq!(chip0, " 1 ");
}

#[test]
fn center_tabs_render_pin_chips_right_aligned() {
    let mut s = Surface::new(80, 1);
    let model = FrameModel {
        tabs: vec!["1".into()],
        active_tab: 0,
        pins: vec![
            crate::pins::PinChip {
                index: 1,
                label: "mail".into(),
                glyph: crate::pins::PinHealth::Running.glyph(),
            },
            crate::pins::PinChip {
                index: 2,
                label: "logs".into(),
                glyph: crate::pins::PinHealth::Stopped.glyph(),
            },
        ],
        ..Default::default()
    };
    let strip = Rect {
        x: 0,
        y: 0,
        cols: 80,
        rows: 1,
    };
    draw_center_tabs(&mut s, strip, &model);
    let row = &lines(&s)[0];
    assert!(row.contains("mail"), "chip label present: {row:?}");
    assert!(row.contains("logs"));
    let spans = strip_chip_spans(&model, strip);
    assert_eq!(spans.len(), 1, "tab chip still present");
    // The pins are right of the tab chip.
    let mail_at = row.find("mail").unwrap();
    assert!(mail_at > spans[0].0, "pins render to the right of tabs");
}

#[test]
fn center_tabs_env_cluster_right_aligned_and_never_overlapped() {
    // A remote, sandboxed worktree: the env cluster is `(podman) [sprite]`.
    let mut s = Surface::new(80, 1);
    let model = FrameModel {
        worktree: "washu/home".into(),
        tabs: vec!["1".into(), "2".into()],
        active_tab: 0,
        active_sandbox_backend: "podman".into(),
        active_placement_kind: Some("sprite".into()),
        ..Default::default()
    };
    let strip = Rect {
        x: 0,
        y: 0,
        cols: 80,
        rows: 1,
    };
    draw_center_tabs(&mut s, strip, &model);
    let row = &lines(&s)[0];
    // The bug being fixed: the tab pill used to paint over the chip. Both
    // chips render intact, and the sandbox chip reads before the placement.
    assert!(row.contains("(podman)"), "backend chip intact: {row:?}");
    let backend_at = row.find("(podman)").unwrap();
    let kind_at = row.find("[sprite]").expect("placement chip intact");
    assert!(backend_at < kind_at, "reads (backend) then [kind]: {row:?}");
    // The cluster is right-aligned: it sits past all tab chips (compare in
    // char columns — the slug separator `▸` is multi-byte). The chip's char
    // column is its byte offset minus the extra bytes the `▸` contributes.
    let extra = "WASHU \u{25b8}".len() - "WASHU \u{25b8}".chars().count();
    let backend_col = backend_at - extra;
    let spans = strip_chip_spans(&model, strip);
    assert_eq!(spans.len(), 2, "both tab chips present: {row:?}");
    let last_tab_end = spans[1].0 + spans[1].1;
    assert!(
        backend_col >= last_tab_end,
        "env cluster is right of the tabs (no overlap): tab end {last_tab_end}, chip at col {backend_col}: {row:?}"
    );
}

#[test]
fn stats_cluster_drops_date_then_gpu_when_tight() {
    let parts: Vec<(String, usize)> = [
        ("cpu", 7),
        ("mem", 11),
        ("gpu", 7),
        ("net", 14),
        ("date", 10),
        ("clock", 5),
    ]
    .into_iter()
    .map(|(id, w)| (id.to_string(), w))
    .collect();
    // Plenty of room: everything survives.
    let all = fit_stats_cluster(&parts, 200);
    assert_eq!(all.len(), 6);
    // Tight: date goes first…
    let full = cluster_width(&parts, &all);
    let no_date = fit_stats_cluster(&parts, full - 1);
    assert!(!no_date.iter().any(|&i| parts[i].0 == "date"));
    assert!(no_date.iter().any(|&i| parts[i].0 == "gpu"));
    // …then gpu.
    let tighter = cluster_width(&parts, &no_date);
    let no_gpu = fit_stats_cluster(&parts, tighter - 1);
    assert!(!no_gpu.iter().any(|&i| parts[i].0 == "gpu"));
    assert!(no_gpu.iter().any(|&i| parts[i].0 == "clock"));
}

#[test]
fn masthead_brand_breakpoints() {
    assert_eq!(masthead_brand_cols(160), BRAND_FULL_COLS);
    assert_eq!(masthead_brand_cols(96), BRAND_FULL_COLS);
    assert_eq!(masthead_brand_cols(95), BRAND_COMPACT_COLS);
    assert_eq!(masthead_brand_cols(60), BRAND_COMPACT_COLS);
    assert_eq!(masthead_brand_cols(59), 0);
}

#[test]
fn masthead_stats_use_quiet_separators_and_threshold_colors() {
    let chrome = layout::compute(160, 10, false, false);
    let model = FrameModel {
        stats: superzej_metrics::StatsSnapshot {
            cpu_pct: Some(95),
            mem_gib: Some((10.0, 64.0)),
            ..Default::default()
        },
        bars: superzej_core::config::BarsConfig {
            top_right: vec!["cpu".into(), "mem".into()],
            ..Default::default()
        },
        ..Default::default()
    };
    let mut s = Surface::new(160, 10);
    draw_masthead(&mut s, &chrome, &model);
    let row = &lines(&s)[0];
    assert!(row.contains(" \u{00b7} "), "dot separator: {row:?}");
    assert!(
        !row.contains('\u{2502}'),
        "no heavy bar separators: {row:?}"
    );
    // 95% CPU renders in the critical (red) color.
    let pct_col = row.find("95%").unwrap();
    let pct_chars = row[..pct_col].chars().count();
    let cells = s.screen_cells();
    assert_eq!(
        cells[0][pct_chars].attrs().foreground(),
        theme_color(theme::RED),
        "critical cpu reads in red"
    );
    drop(cells);
    assert_eq!(stat_level(79), Level::Normal);
    assert_eq!(stat_level(80), Level::Warn);
    assert_eq!(stat_level(92), Level::Crit);
    assert_eq!(ratio_level(54.4, 64.0), Level::Warn);
    assert_eq!(ratio_level(10.0, 64.0), Level::Normal);
    assert_eq!(ratio_level(63.0, 64.0), Level::Crit);
    assert_eq!(ratio_level(1.0, 0.0), Level::Normal);
}

#[test]
fn strip_draws_header_label_and_glyph() {
    let mut s = Surface::new(40, 6);
    let strip = Rect {
        x: 0,
        y: 0,
        cols: 40,
        rows: 6,
    };
    let emu = AlacrittyEmulator::new(5, 40, 100);
    let cells = vec![StripCell {
        pane: 1,
        rect: strip,
        label: "syslog".into(),
        glyph: crate::pins::PinHealth::Running.glyph(),
        focused: true,
    }];
    draw_strip(&mut s, strip, &cells, theme::TEAL, |id| {
        (id == 1).then_some(&emu as &dyn PaneEmulator)
    });
    let header = &lines(&s)[0];
    assert!(header.contains("syslog"), "header label: {header:?}");
    assert!(header.contains(crate::pins::PinHealth::Running.glyph()));
}

#[test]
fn center_tab_bar_sits_below_the_divider() {
    let chrome = layout::compute(160, 10, true, true);
    let mut s = Surface::new(160, 10);
    let model = FrameModel {
        worktree: "repo/home".into(),
        tabs: vec!["1".into(), "2".into()],
        active_tab: 0,
        ..Default::default()
    };

    draw_chrome(&mut s, &chrome, &model, &crate::panel::PanelUi::default());

    let brand_cols = masthead_brand_cols(160);
    let l = lines(&s);
    // The masthead carries only brand + stats; the worktree label and
    // chips live on the center tab bar below the divider.
    assert!(
        !l[0].contains("REPO"),
        "masthead carries no nav labels: {:?}",
        l[0]
    );
    let tabs_row = &l[chrome.center_tabs.y];
    assert!(
        tabs_row.contains("REPO \u{25b8} home"),
        "worktree label on the center tab bar: {tabs_row:?}"
    );
    // The divider rule caps the columns above the tab bar.
    assert!(
        l[chrome.divider.y].contains("\u{2500}\u{2500}\u{2500}"),
        "divider rule: {:?}",
        l[chrome.divider.y]
    );
    // The text brand occupies the masthead's brand slot.
    let brand_zone: String = l[0].chars().take(brand_cols).collect();
    assert!(
        brand_zone.contains("superzej"),
        "text brand on the masthead: {:?}",
        l[0]
    );
}

#[test]
fn full_frame_tab_chip_lands_on_the_center_tab_bar() {
    let cols = 160usize;
    let rows = 10usize;
    let chrome = layout::compute(cols, rows, true, true);
    let mut emu = AlacrittyEmulator::new(chrome.center.rows as u16, chrome.center.cols as u16, 0);
    emu.advance(b"CENTER");
    let model = FrameModel {
        worktree: "repo/home".into(),
        tabs: vec!["1".into()],
        active_tab: 0,
        ..Default::default()
    };
    let center = crate::center::CenterTree::Leaf(1);
    let mut s = Surface::new(cols, rows);

    render_tab(
        &mut s,
        &chrome,
        &center,
        1,
        &model,
        &crate::panel::PanelUi::default(),
        |id| (id == 1).then_some(&emu as &dyn PaneEmulator),
        &|_| String::new(),
        &|_| None,
    );

    let l = lines(&s);
    let spans = strip_chip_spans(&model, chrome.center_tabs);
    assert_eq!(spans.len(), 1);
    let tabs_row = &l[chrome.center_tabs.y];
    let chip: String = tabs_row.chars().skip(spans[0].0).take(spans[0].1).collect();
    assert_eq!(chip, " 1 ", "tab chip on the center tab bar: {tabs_row:?}");
    assert!(
        tabs_row.contains("REPO \u{25b8} home"),
        "worktree label beside the chips: {tabs_row:?}"
    );
}

#[test]
fn render_tab_paints_every_visible_pane() {
    use crate::center::{Branch, CenterTree, Dir};
    let cols = 160usize;
    let rows = 40usize;
    let chrome = layout::compute(cols, rows, false, false); // full-width center

    // Two side-by-side panes (ids 1 and 2).
    let center = CenterTree::Split {
        dir: Dir::Row,
        children: vec![
            Branch {
                weight: 1.0,
                child: CenterTree::Leaf(1),
            },
            Branch {
                weight: 1.0,
                child: CenterTree::Leaf(2),
            },
        ],
    };
    let half = (chrome.center.cols / 2) as u16;
    let mut left = AlacrittyEmulator::new(chrome.center.rows as u16, half, 0);
    left.advance(b"LEFTPANE");
    let mut right = AlacrittyEmulator::new(chrome.center.rows as u16, half, 0);
    right.advance(b"RIGHTPANE");

    let model = FrameModel {
        tabs: vec!["repo/home".into()],
        ..Default::default()
    };
    let mut s = Surface::new(cols, rows);
    render_tab(
        &mut s,
        &chrome,
        &center,
        1,
        &model,
        &crate::panel::PanelUi::default(),
        |id| match id {
            1 => Some(&left as &dyn PaneEmulator),
            2 => Some(&right as &dyn PaneEmulator),
            _ => None,
        },
        &|id| format!("pane-{id}"),
        &|_| None,
    );
    let text = s.screen_chars_to_string();
    assert!(text.contains("LEFTPANE"), "left pane painted");
    assert!(text.contains("RIGHTPANE"), "right pane painted");
    // Card titles ride the top border of each pane frame.
    assert!(text.contains(" pane-1 "), "embedded card title: {text:?}");
    assert!(text.contains(" pane-2 "));
}

#[test]
fn render_tab_shows_splash_when_no_live_panes() {
    let cols = 160usize;
    let rows = 40usize;
    let chrome = layout::compute(cols, rows, true, true);
    let model = FrameModel {
        worktree: "repo/home".into(),
        tabs: vec!["1".into()],
        ..Default::default()
    };
    let center = crate::center::CenterTree::Leaf(1);
    let mut s = Surface::new(cols, rows);
    render_tab(
        &mut s,
        &chrome,
        &center,
        1,
        &model,
        &crate::panel::PanelUi::default(),
        |_| None,
        &|_| String::new(),
        &|_| None,
    );
    let l = lines(&s);
    // The splash wordmark lands inside the center band, with chrome intact.
    let mid = &l[chrome.center.y + chrome.center.rows / 2 - 1];
    let band: String = l[chrome.center.y..chrome.center.y + chrome.center.rows]
        .iter()
        .map(|r| {
            r.chars()
                .skip(chrome.center.x)
                .take(chrome.center.cols)
                .collect::<String>()
        })
        .collect();
    assert!(
        band.contains("Ctrl-Space"),
        "splash hints in center: {mid:?}"
    );
    assert!(band.chars().any(|c| "▀▄█".contains(c)), "splash wordmark");
    assert!(l.join("\n").contains("WORKSPACES"), "chrome still drawn");
    // No card rings drawn around the empty center.
    assert!(!band.contains('\u{256d}'), "no pane card on empty center");
}

#[test]
fn full_frame_places_chrome_and_center_pane() {
    let cols = 160usize;
    let rows = 40usize;
    let chrome = layout::compute(cols, rows, true, true);

    let mut emu = AlacrittyEmulator::new(chrome.center.rows as u16, chrome.center.cols as u16, 0);
    emu.advance(b"CENTER-CONTENT");

    let model = FrameModel {
        tabs: vec!["repo/home".into()],
        active_tab: 0,
        sidebar_rows: vec![
            row(crate::sidebar::RowKind::Workspace, "repo"),
            row(crate::sidebar::RowKind::Worktree, "feat"),
        ],
        panel: crate::panel::PanelData {
            branch: "feat".into(),
            pr: Some(crate::panel::PrSummary {
                number: 42,
                title: "a pr".into(),
                state: "OPEN".into(),
                url: "https://example/42".into(),
                is_draft: false,
                review_decision: None,
            }),
            ..Default::default()
        },
        status: "Cmd-K  Alt-w new  Alt-o switch".into(),
        bars: superzej_core::config::BarsConfig {
            bottom_left: vec!["status".into()],
            ..Default::default()
        },
        ..Default::default()
    };

    let mut s = Surface::new(cols, rows);
    let center = crate::center::CenterTree::Leaf(1);
    // Pr section open (Work tab) so the #42 PR summary is on screen.
    let panel_ui = crate::panel::PanelUi {
        tab: crate::panel::PanelTab::Work,
        open: crate::panel::Section::Pr,
        ..Default::default()
    };
    render_tab(
        &mut s,
        &chrome,
        &center,
        1,
        &model,
        &panel_ui,
        |id| (id == 1).then_some(&emu as &dyn PaneEmulator),
        &|_| String::new(),
        &|_| None,
    );
    let l = lines(&s);

    // Masthead: the text brand on row 0; the tab chip rides the center
    // tab bar; the accordion sections fill the panel column; the
    // statusbar (last row) carries the status widget.
    assert!(l[0].contains("superzej"), "brand: {:?}", l[0]);
    let tabs_row = &l[chrome.center_tabs.y];
    assert!(
        tabs_row.contains("repo/home") || tabs_row.contains(" repo/home "),
        "tab chip on the center tab bar: {tabs_row:?}"
    );
    let panel_rect = chrome.panel.unwrap();
    let panel_col: String = l
        .iter()
        .map(|row| {
            row.chars()
                .skip(panel_rect.x)
                .take(panel_rect.cols)
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    // Default tab = Work (Pr section open with #42 PR data); tab bar is visible.
    assert!(
        panel_col.contains("git") && panel_col.contains("work") && panel_col.contains("system"),
        "tab bar in panel column: {panel_col:?}"
    );
    // Work tab sections are visible.
    assert!(
        panel_col.contains("pr") && panel_col.contains("issues"),
        "accordion sections fill the panel column: {panel_col:?}"
    );
    assert!(l[rows - 1].contains("Cmd-K"), "status: {:?}", l[rows - 1]);
    // Sidebar title and center content all present.
    let all = l.join("\n");
    assert!(all.contains("WORKSPACES"));
    assert!(all.contains("CENTER-CONTENT"));
    assert!(all.contains("#42"));
}

#[test]
fn statusbar_keyhints_stop_at_last_whole_binding() {
    // Three bindings; "a alpha" (7) + "   b bravo" (10) overflow a tight bar.
    let model = FrameModel {
        keyhints: vec![
            ("a".into(), "alpha".into()),
            ("b".into(), "bravo".into()),
            ("c".into(), "charlie".into()),
        ],
        bars: superzej_core::config::BarsConfig {
            bottom_left: vec!["keyhints".into()],
            bottom_right: vec![],
            ..Default::default()
        },
        ..Default::default()
    };
    // Budget = 16 - 2 = 14: " " (1) + "a alpha" (7) fits at 8; the next
    // binding would land at 18, so it is dropped whole.
    let mut s = Surface::new(16, 1);
    draw_statusbar(
        &mut s,
        Rect {
            x: 0,
            y: 0,
            cols: 16,
            rows: 1,
        },
        &model,
    );
    let text = s.screen_chars_to_string();
    assert!(text.contains("alpha"), "first binding shown: {text:?}");
    // No mid-binding cut: the dropped binding is fully absent, not "b…".
    assert!(
        !text.contains('\u{2026}'),
        "no ellipsis truncation: {text:?}"
    );
    assert!(
        !text.contains("bravo"),
        "overflowing binding dropped: {text:?}"
    );
    assert!(
        !text.contains("charlie"),
        "overflowing binding dropped: {text:?}"
    );

    // With ample width every binding is present, untouched.
    let mut wide = Surface::new(60, 1);
    draw_statusbar(
        &mut wide,
        Rect {
            x: 0,
            y: 0,
            cols: 60,
            rows: 1,
        },
        &model,
    );
    let wtext = wide.screen_chars_to_string();
    assert!(
        wtext.contains("alpha") && wtext.contains("bravo") && wtext.contains("charlie"),
        "all bindings fit when wide: {wtext:?}"
    );
}

#[test]
fn statusbar_tests_widget_renders_pass_fail_rollup() {
    use crate::panel::{PanelData, TestsLite};
    let bars = || superzej_core::config::BarsConfig {
        bottom_left: vec![],
        bottom_right: vec!["tests".into()],
        ..Default::default()
    };
    let rect = Rect {
        x: 0,
        y: 0,
        cols: 40,
        rows: 1,
    };

    // All-pass run: only the ✓ count, no ✗.
    let pass = FrameModel {
        panel: PanelData {
            tests: Some(TestsLite {
                passed: 12,
                ..Default::default()
            }),
            ..Default::default()
        },
        bars: bars(),
        ..Default::default()
    };
    let mut s = Surface::new(40, 1);
    draw_statusbar(&mut s, rect, &pass);
    let text = s.screen_chars_to_string();
    assert!(text.contains("\u{2713}12"), "pass count shown: {text:?}");
    assert!(
        !text.contains('\u{2717}'),
        "no fail glyph all-pass: {text:?}"
    );

    // Mixed run: ✓ and ✗ both shown.
    let mixed = FrameModel {
        panel: PanelData {
            tests: Some(TestsLite {
                passed: 8,
                failed: 3,
                ..Default::default()
            }),
            ..Default::default()
        },
        bars: bars(),
        ..Default::default()
    };
    let mut s2 = Surface::new(40, 1);
    draw_statusbar(&mut s2, rect, &mixed);
    let t2 = s2.screen_chars_to_string();
    assert!(
        t2.contains("\u{2713}8") && t2.contains("\u{2717}3"),
        "pass+fail counts shown: {t2:?}"
    );

    // No run yet (default counts) → widget hidden.
    let empty = FrameModel {
        panel: PanelData {
            tests: Some(TestsLite::default()),
            ..Default::default()
        },
        bars: bars(),
        ..Default::default()
    };
    let mut s3 = Surface::new(40, 1);
    draw_statusbar(&mut s3, rect, &empty);
    let t3 = s3.screen_chars_to_string();
    assert!(
        !t3.contains('\u{2713}') && !t3.contains('\u{2717}'),
        "widget hidden with no counts: {t3:?}"
    );
}

/// A minimal panel model with one unstaged change.
fn panel_model() -> FrameModel {
    use crate::panel::{ChangeRow, PanelData};
    FrameModel {
        panel: PanelData {
            branch: "feat".into(),
            changes: vec![ChangeRow {
                status: "M".into(),
                dir: "src/".into(),
                name: "main.rs".into(),
                path: "src/main.rs".into(),
                added: 3,
                deleted: 1,
                ..Default::default() // stage: Unstaged, incoming: false
            }],
            ..Default::default()
        },
        panel_focused: true,
        ..Default::default()
    }
}

#[test]
fn panel_renders_accordion_sections_and_open_content() {
    use crate::panel::PanelUi;
    let rect = Rect {
        x: 0,
        y: 0,
        cols: 44,
        rows: 30,
    };
    let model = panel_model();
    let ui = PanelUi::default(); // tab = Git
    let mut s = Surface::new(44, 30);
    draw_panel(&mut s, rect, &model, &ui);
    let text = s.screen_chars_to_string();
    // Active-tab sections (Git: changes, commits, branches, stash, files)
    // are on screen; other-tab sections are hidden.
    for sec in ui.tab_sections() {
        assert!(
            text.contains(sec.label()),
            "{} missing: {text:?}",
            sec.label()
        );
    }
    // Tab bar labels are always visible.
    assert!(text.contains("git"), "tab bar: {text:?}");
    assert!(text.contains("work"), "tab bar: {text:?}");
    assert!(text.contains("system"), "tab bar: {text:?}");
    assert!(text.contains("feat"), "branch header: {text:?}");
    assert!(text.contains("main.rs"), "open section content: {text:?}");
    // Help hints moved to the bottom bar: section mode offers the open
    // affordance (Enter to drill into rows), row mode the section's actions.
    assert!(
        panel_help_pairs(&PanelUi::default())
            .iter()
            .any(|(_, l)| l == "open")
    );
    let row_mode = PanelUi {
        row_mode: true,
        ..Default::default()
    };
    assert!(
        panel_help_pairs(&row_mode)
            .iter()
            .any(|(_, l)| l == "stage")
    );
    // During an active merge conflict the flow hint leads and replaces the
    // generic "m flow menu" entry in the table.
    let merge_flow = PanelUi {
        row_mode: true,
        git: crate::panel::gitui::GitUi {
            flow: crate::panel::gitui::GitFlow::Merge(crate::panel::gitui::SequencerUi {
                onto: "main".to_string(),
                conflict: true,
            }),
            ..Default::default()
        },
        ..Default::default()
    };
    let mf_pairs = panel_help_pairs(&merge_flow);
    assert_eq!(
        mf_pairs[0],
        ("m".to_string(), "merge continue/abort".to_string())
    );
    // The generic "m flow menu" entry is suppressed (deduplicated by chord).
    assert!(!mf_pairs.iter().any(|(_, l)| l == "flow menu"));
    // Degenerate rects never panic or paint.
    let mut tiny = Surface::new(44, 30);
    draw_panel(
        &mut tiny,
        Rect {
            x: 0,
            y: 0,
            cols: 0,
            rows: 0,
        },
        &model,
        &PanelUi::default(),
    );
}

#[test]
fn panel_hits_expose_all_sections_at_distinct_rows() {
    use crate::panel::{PanelHit, PanelUi};
    let rect = Rect {
        x: 0,
        y: 3,
        cols: 44,
        rows: 30,
    };
    let model = panel_model();
    let hits = panel_hits(&model, &PanelUi::default(), rect);
    let section_rows: Vec<usize> = hits
        .iter()
        .filter(|(_, h)| matches!(h, PanelHit::OpenSection(_)))
        .map(|(y, _)| *y)
        .collect();
    // Default tab = Git → 5 sections shown (Changes, Commits, Branches, Stash, Files).
    let default_ui = PanelUi::default();
    assert_eq!(
        section_rows.len(),
        default_ui.tab_sections().len(),
        "hits: {hits:?}"
    );
    let mut dedup = section_rows.clone();
    dedup.dedup();
    assert_eq!(dedup, section_rows, "section rows are distinct + ordered");
    for y in &section_rows {
        assert!(*y >= rect.y && *y < rect.y + rect.rows, "y in rect: {y}");
    }
}

#[test]
fn checks_render_inside_the_open_git_section() {
    use crate::panel::{CheckLine, CheckState, PanelUi, PrSummary, Section};
    let rect = Rect {
        x: 0,
        y: 0,
        cols: 44,
        rows: 30,
    };
    let mut model = panel_model();
    model.panel.pr = Some(PrSummary {
        number: 42,
        title: "a pr".into(),
        state: "OPEN".into(),
        url: "https://example/42".into(),
        is_draft: false,
        review_decision: None,
    });
    model.panel.checks = vec![
        CheckLine {
            name: "build".into(),
            state: CheckState::Pass,
            duration_secs: None,
            details_url: None,
        },
        CheckLine {
            name: "lint".into(),
            state: CheckState::Fail,
            duration_secs: None,
            details_url: None,
        },
    ];
    let ui = PanelUi {
        tab: crate::panel::PanelTab::Work,
        open: Section::Pr,
        ..Default::default()
    };
    let mut s = Surface::new(44, 30);
    draw_panel(&mut s, rect, &model, &ui);
    let text = s.screen_chars_to_string();
    assert!(text.contains("CHECKS"), "{text:?}");
    assert!(text.contains("build"), "{text:?}");
    assert!(text.contains("lint"), "{text:?}");
    assert!(text.contains("#42"), "{text:?}");
}
