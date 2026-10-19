use crate::{
    compositor::Rect,
    detail::StatusCtx,
    monitor::{Confirm, MonitorOverlay, MonitorPrefs, MonitorTab},
    telemetry::TelemetryHistory,
};
use std::sync::Arc;
use termwiz::input::{KeyCode, Modifiers};
use thegn_metrics::collect::{CollectorId, frame::MonitorFrame, health::Health};
use thegn_metrics::{DiskInfo, DiskKind, StatsSnapshot};

fn overlay() -> MonitorOverlay {
    let history = TelemetryHistory::default();
    let model = crate::chrome::FrameModel {
        stats: StatsSnapshot {
            disks: Arc::from([DiskInfo {
                name: "nvme0n1".into(),
                mount: "/".into(),
                free_pct: 40,
                read_bps: 12_000_000,
                write_bps: 3_000_000,
                kind: DiskKind::Ssd,
                extra_mounts: 0,
            }]),
            ..Default::default()
        },
        ..Default::default()
    };
    let ctx = StatusCtx::new_for_test_on(&history, Rect::full(240, 60));
    MonitorOverlay::open(MonitorTab::Disk, MonitorPrefs::default(), &model, &ctx)
}

#[test]
fn disk_opens_as_board_view_with_the_worktree_panel_and_placed_demand() {
    use crate::monitor::board::TabView;

    let overlay = overlay();
    assert_eq!(overlay.tab(), MonitorTab::Disk);
    let view = overlay.view();
    let placed = overlay.board.placed(&view, overlay.body_rect());
    assert!(placed.iter().any(|panel| panel.id.0 == "disk.worktrees"));
    let mut surface = termwiz::surface::Surface::new(240, 60);
    overlay
        .board
        .paint(&view, &mut surface, overlay.body_rect(), 0, &[]);
    let text = surface.screen_chars_to_string();
    for title in [
        "I/O",
        "I/O pressure",
        "Devices",
        "Filesystems",
        "thegn.slice",
        "Worktrees",
    ] {
        assert!(text.contains(title), "missing {title}: {text}");
    }
    let expected = placed.iter().fold(
        thegn_metrics::collect::CollectorSet::empty(),
        |mut set, panel| {
            for id in view.demand(panel.id).iter() {
                set.insert(id);
            }
            set
        },
    );
    assert_eq!(overlay.demand(1000).collectors, expected);
}

#[test]
fn disk_ingest_is_visible_only_and_pause_freezes_its_model() {
    let mut overlay = overlay();
    let mut frame = MonitorFrame::default();
    frame.health[CollectorId::Diskstats.index()] = Health::Ok;
    frame.parts.diskstats.aggregate.read_bps = Some(8_000_000.0);
    assert!(overlay.ingest_frame(&frame));
    assert_eq!(overlay.disk.test_read_bps(), Some(8_000_000.0));

    overlay.tab = MonitorTab::Procs;
    frame.parts.diskstats.aggregate.read_bps = Some(55_000_000.0);
    assert!(!overlay.ingest_frame(&frame));
    assert_eq!(overlay.disk.test_read_bps(), Some(8_000_000.0));
    overlay.tab = MonitorTab::Disk;
    overlay.handle_key(&KeyCode::Char(' '), Modifiers::NONE);
    frame.parts.diskstats.aggregate.read_bps = Some(99_000_000.0);
    assert!(!overlay.ingest_frame(&frame));
    assert_eq!(overlay.disk.test_read_bps(), Some(8_000_000.0));
}

#[test]
fn disk_rebuild_uses_telemetry_history() {
    let mut history = TelemetryHistory::default();
    for (at_ms, read, write) in [(1000, 10_000_000, 2_000_000), (2000, 18_000_000, 4_000_000)] {
        history.push(
            &StatsSnapshot {
                disk_io: Some((read, write)),
                ..Default::default()
            },
            at_ms,
        );
    }
    let model = crate::chrome::FrameModel {
        stats: StatsSnapshot {
            disks: Arc::from([DiskInfo {
                name: "nvme0n1".into(),
                mount: "/".into(),
                free_pct: 40,
                read_bps: 18_000_000,
                write_bps: 4_000_000,
                kind: DiskKind::Ssd,
                extra_mounts: 0,
            }]),
            ..Default::default()
        },
        ..Default::default()
    };
    let mut ctx = StatusCtx::new_for_test_on(&history, Rect::full(240, 60));
    ctx.now_ms = 3000;
    let mut overlay = MonitorOverlay::open(MonitorTab::Disk, MonitorPrefs::default(), &model, &ctx);
    assert_eq!(overlay.disk.test_telemetry_state().0, 240);
    assert_eq!(overlay.disk.test_telemetry_state().1, 240);
    assert_eq!(overlay.disk.test_telemetry_state().2, Some(18_000_000.0));
    assert_eq!(overlay.disk.test_telemetry_state().3, Some(4_000_000.0));
    let values = overlay.disk.test_telemetry_read_values();
    let first_sample = values
        .iter()
        .position(|value| value.is_finite())
        .expect("history sample");
    let last_sample = values
        .iter()
        .rposition(|value| value.is_finite())
        .expect("history sample");
    assert!(values[..first_sample].iter().all(|value| value.is_nan()));
    assert!(
        values[last_sample + 1..]
            .iter()
            .all(|value| *value == values[last_sample])
    );
    let frozen = overlay.disk.test_telemetry_state().2;
    overlay.paused = true;
    ctx.now_ms = 4000;
    overlay.rebuild(&model, &ctx, false);
    assert_eq!(overlay.disk.test_telemetry_state().2, frozen);
}

#[test]
fn x_confirms_the_selected_worktree_path() {
    let mut overlay = overlay();
    let selected = std::path::PathBuf::from("/tmp/worktree-selected");
    overlay.disk_rows = vec![
        crate::monitor::build::DiskWtRow {
            path: std::path::PathBuf::from("/tmp/worktree-first"),
            name: "first".into(),
            total_bytes: 10,
            target_bytes: 4,
            age_secs: Some(30),
            measured_at_secs: None,
        },
        crate::monitor::build::DiskWtRow {
            path: selected.clone(),
            name: "selected".into(),
            total_bytes: 20,
            target_bytes: 8,
            age_secs: Some(60),
            measured_at_secs: None,
        },
    ];
    overlay.sel = 1;
    overlay.handle_key(&KeyCode::Char('x'), Modifiers::NONE);
    assert!(matches!(
        overlay.confirm,
        Some(Confirm::Clean { path, .. }) if path == selected
    ));
}

fn line_text(line: crate::seg::Line) -> String {
    match line {
        crate::seg::Line::Segs(segs) => segs.iter().map(|seg| seg.text.as_str()).collect(),
        crate::seg::Line::Split { l, r } | crate::seg::Line::SplitMinLeft { l, r, .. } => l
            .iter()
            .chain(r.iter())
            .map(|seg| seg.text.as_str())
            .collect(),
        _ => String::new(),
    }
}

fn overlay_with_worktree_rows() -> (MonitorOverlay, std::path::PathBuf) {
    let mut overlay = overlay();
    let selected = std::path::PathBuf::from("/tmp/worktree-selected");
    overlay.disk_rows = vec![
        crate::monitor::build::DiskWtRow {
            path: std::path::PathBuf::from("/tmp/worktree-first"),
            name: "first".into(),
            total_bytes: 10,
            target_bytes: 4,
            age_secs: Some(30),
            measured_at_secs: None,
        },
        crate::monitor::build::DiskWtRow {
            path: selected.clone(),
            name: "selected".into(),
            total_bytes: 20,
            target_bytes: 8,
            age_secs: Some(60),
            measured_at_secs: None,
        },
    ];
    overlay.sel = 1;
    (overlay, selected)
}

#[test]
fn x_does_nothing_while_the_worktrees_panel_is_zoomed_away() {
    let (mut overlay, _) = overlay_with_worktree_rows();
    overlay.handle_key(&KeyCode::Char('f'), Modifiers::NONE);
    overlay.handle_key(&KeyCode::Char('z'), Modifiers::NONE);
    assert!(overlay.board.is_zoomed());
    assert!(!overlay.disk_worktrees_placed());

    overlay.handle_key(&KeyCode::Char('x'), Modifiers::NONE);

    assert!(overlay.confirm.is_none());
    assert!(
        overlay
            .status
            .as_deref()
            .is_some_and(|status| status.contains("worktrees panel hidden"))
    );
    assert!(!line_text(overlay.footer()).contains("x clean"));
}

#[test]
fn x_still_confirms_after_unzoom() {
    let (mut overlay, selected) = overlay_with_worktree_rows();
    overlay.handle_key(&KeyCode::Char('f'), Modifiers::NONE);
    overlay.handle_key(&KeyCode::Char('z'), Modifiers::NONE);
    overlay.handle_key(&KeyCode::Escape, Modifiers::NONE);
    assert!(overlay.disk_worktrees_placed());

    overlay.handle_key(&KeyCode::Char('x'), Modifiers::NONE);

    assert!(matches!(
        overlay.confirm,
        Some(Confirm::Clean { path, .. }) if path == selected
    ));
}
