use crate::{
    caps,
    compositor::Rect,
    detail::StatusCtx,
    monitor::{MonitorOverlay, MonitorPrefs, MonitorTab, board::TabView},
    telemetry::TelemetryHistory,
};
use termwiz::{input::KeyCode, surface::Surface};
use thegn_metrics::{
    StatsSnapshot,
    collect::{CollectorId, frame::MonitorFrame},
};

fn overlay_for(tab: MonitorTab) -> MonitorOverlay {
    let history = TelemetryHistory::default();
    let model = crate::chrome::FrameModel {
        stats: StatsSnapshot {
            gpu_pct: Some(30),
            gpu_mem_mib: Some((512, 1024)),
            ..StatsSnapshot::default()
        },
        ..crate::chrome::FrameModel::default()
    };
    let ctx = StatusCtx::new_for_test_on(&history, Rect::full(240, 60));
    MonitorOverlay::open(tab, MonitorPrefs::default(), &model, &ctx)
}

fn overlay() -> MonitorOverlay {
    overlay_for(MonitorTab::Gpu)
}

fn frame() -> MonitorFrame {
    let mut frame = MonitorFrame::default();
    frame
        .parts
        .gpu_devices
        .devices
        .push(thegn_metrics::collect::model::gpu::GpuDevice {
            slot: "0000:04:00.0".to_owned(),
            card: 0,
            driver: Some("amdgpu".to_owned()),
            pci_id: None,
            kind: thegn_metrics::collect::model::gpu::GpuKind::Apu,
            family: Some("Radeon fixture".to_owned()),
            ip_version: None,
            runtime_status: thegn_metrics::collect::model::gpu::RuntimeStatus::Active,
            vram_total: Some(1024),
            gtt_total: Some(4096),
            boot_vga: Some(true),
        });
    frame.parts.gpu_engines.devices.push(
        thegn_metrics::collect::model::gpu_engines::GpuEngineRow {
            slot: thegn_metrics::collect::model::gpu_engines::GpuSlot::new("0000:04:00.0")
                .expect("valid fixture slot"),
            runtime_status: thegn_metrics::collect::model::gpu::RuntimeStatus::Active,
            gfx_busy_pct: Some(37),
            vcn_busy_pct: Some(11),
            vram_used: Some(512),
            vram_total: Some(1024),
            gtt_used: Some(1024),
            gtt_total: Some(4096),
            sclk: thegn_metrics::collect::model::gpu_engines::DpmLevels {
                levels_mhz: [500, 1500, 3000, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
                len: 3,
                current_mhz: Some(1500),
            },
            mclk: Default::default(),
        },
    );
    frame.health[CollectorId::GpuDevices.index()] = thegn_metrics::collect::health::Health::Ok;
    frame.health[CollectorId::GpuEngines.index()] = thegn_metrics::collect::health::Health::Ok;
    frame
}

#[test]
fn gpu_overlay_selects_board_view_and_demands_placed_panel_sources() {
    let overlay = overlay();
    assert_eq!(overlay.tab(), MonitorTab::Gpu);
    let view = overlay.view();
    let placed = overlay.board.placed(&view, overlay.body_rect());
    assert!(!placed.is_empty());
    assert!(placed.iter().all(|panel| panel.id.0.starts_with("gpu.")));
    let rendered = {
        let mut surface = Surface::new(240, 60);
        overlay
            .board
            .paint(&view, &mut surface, overlay.body_rect(), 0, &[]);
        surface.screen_chars_to_string()
    };
    for title in ["GPU devices", "GPU engines", "GPU memory", "GPU clocks"] {
        assert!(rendered.contains(title), "missing {title}");
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
    assert_eq!(expected.iter().count(), 3);
}

#[test]
fn gpu_overlay_ingest_is_visible_only_and_pause_freezes_model() {
    let mut overlay = overlay();
    assert!(overlay.ingest_frame(&frame()));
    assert_eq!(overlay.gpu.engines.devices[0].gfx_busy_pct, Some(37));
    overlay.handle_key(&KeyCode::Char(' '), termwiz::input::Modifiers::NONE);
    let mut next = frame();
    next.parts.gpu_engines.devices[0].gfx_busy_pct = Some(82);
    assert!(!overlay.ingest_frame(&next));
    assert_eq!(overlay.gpu.engines.devices[0].gfx_busy_pct, Some(37));

    let mut other = overlay_for(MonitorTab::Procs);
    assert!(!other.ingest_frame(&frame()));
    assert!(other.gpu.engines.devices.is_empty());
}

#[test]
fn gpu_overlay_uses_legacy_view_when_engine_collector_is_absent() {
    let mut overlay = overlay();
    let mut frame = frame();
    frame.parts.gpu_engines.devices.clear();
    frame.health[CollectorId::GpuEngines.index()] = thegn_metrics::collect::health::Health::Absent(
        thegn_metrics::collect::health::Absent::NotImplemented,
    );
    assert!(overlay.ingest_frame(&frame));

    assert!(matches!(
        overlay.view(),
        crate::monitor::board::ActiveView::Legacy(_)
    ));
}

#[test]
fn memory_history_right_aligns_two_samples_in_unicode_and_ascii() {
    let mut overlay = overlay();
    let mut sample = frame();
    sample.parts.gpu_engines.devices[0].vram_used = Some(400);
    assert!(overlay.ingest_frame(&sample));
    sample.parts.gpu_engines.devices[0].vram_used = Some(600);
    assert!(overlay.ingest_frame(&sample));

    let view = overlay.view();
    let memory = overlay
        .board
        .placed(&view, overlay.body_rect())
        .into_iter()
        .find(|panel| panel.id.0 == "gpu.memory")
        .expect("memory panel placed");
    for level in [
        thegn_core::termcaps::UnicodeLevel::Full,
        thegn_core::termcaps::UnicodeLevel::Ascii,
    ] {
        let mut surface = Surface::new(240, 60);
        caps::test_override::with_unicode(level, || {
            overlay
                .board
                .paint(&view, &mut surface, overlay.body_rect(), 0, &[]);
        });
        let label_y = surface
            .screen_lines()
            .iter()
            .position(|line| {
                line.visible_cells()
                    .any(|cell| cell.str().contains("VRAM used / total 0-100%"))
            })
            .expect("VRAM history label rendered");
        let graph_y = label_y + 1;
        let inner_left = memory.rect.x + 1;
        let inner_right = memory.rect.x + memory.rect.cols - 2;
        let graph_cells = surface.screen_lines()[graph_y]
            .visible_cells()
            .map(|cell| (cell.cell_index(), cell.str().to_owned()))
            .collect::<Vec<_>>();
        let right_is_filled = graph_cells
            .iter()
            .find(|(x, _)| *x == inner_right)
            .is_some_and(|(_, value)| !value.is_empty() && value != " " && value != "\u{2800}");
        assert!(
            right_is_filled,
            "rightmost VRAM graph cell is blank: {level:?}"
        );
        let left_is_blank = graph_cells
            .iter()
            .find(|(x, _)| *x == inner_left)
            .is_none_or(|(_, value)| value.is_empty() || value == " " || value == "\u{2800}");
        assert!(
            left_is_blank,
            "short VRAM history was not right-aligned: {level:?}"
        );
    }
}
