use super::view::{GpuModel, GpuView};
use crate::{
    caps,
    compositor::Rect,
    monitor::board::{Board, TabView as _},
};
use termwiz::surface::Surface;
use thegn_core::termcaps::{ColorDepth, UnicodeLevel};
use thegn_metrics::collect::{
    CollectorId,
    frame::MonitorFrame,
    health::{Absent, Health},
    model::{
        amd_smu::{AmdSmuCard, AmdSmuStatus},
        gpu::{GpuDevice, GpuKind, RuntimeStatus},
        gpu_engines::{DpmLevels, GpuEngineRow, GpuSlot},
    },
    parse::amd_gpu_metrics::GpuMetrics,
};

fn fixture(tick: u32, kind: GpuKind, status: RuntimeStatus) -> MonitorFrame {
    let mut frame = MonitorFrame::default();
    frame.parts.gpu_devices.devices.push(GpuDevice {
        slot: "0000:04:00.0".to_owned(),
        card: 0,
        driver: Some("amdgpu".to_owned()),
        pci_id: Some("1002:15bf".to_owned()),
        kind,
        family: Some("Radeon 890M".to_owned()),
        ip_version: None,
        runtime_status: status,
        vram_total: Some(512 * 1024 * 1024),
        gtt_total: Some(46 * 1024 * 1024 * 1024),
        boot_vga: Some(true),
    });
    frame.parts.gpu_engines.devices.push(GpuEngineRow {
        slot: GpuSlot::new("0000:04:00.0").expect("valid fixture slot"),
        runtime_status: status,
        gfx_busy_pct: Some((25 + tick % 55) as u8),
        vcn_busy_pct: Some((tick % 13) as u8),
        vram_used: Some(300 * 1024 * 1024 + u64::from(tick) * 1024 * 1024),
        vram_total: Some(512 * 1024 * 1024),
        gtt_used: Some(3 * 1024 * 1024 * 1024 + u64::from(tick) * 2 * 1024 * 1024),
        gtt_total: Some(46 * 1024 * 1024 * 1024),
        sclk: DpmLevels {
            levels_mhz: [600, 764, 2900, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            len: 3,
            current_mhz: (status == RuntimeStatus::Active).then_some(764),
        },
        mclk: DpmLevels {
            levels_mhz: [500, 2400, 2800, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            len: 3,
            current_mhz: (status == RuntimeStatus::Active).then_some(2400),
        },
    });
    let mut metrics = GpuMetrics::default();
    metrics.power_socket = Some(17_300 + tick * 100);
    metrics.temperature_gfx = Some(8_000 + (tick % 7) as u16 * 100);
    frame.parts.amd_smu.cards.push(AmdSmuCard {
        card: 0,
        suspended: status == RuntimeStatus::Suspended,
        decoded: AmdSmuStatus::Decoded(
            thegn_metrics::collect::parse::amd_gpu_metrics::Decoded::Full,
        ),
        metrics,
    });
    frame.health[CollectorId::GpuDevices.index()] = Health::Ok;
    frame.health[CollectorId::GpuEngines.index()] = Health::Ok;
    frame.health[CollectorId::AmdSmu.index()] = Health::Ok;
    frame
}

fn text(surface: &Surface) -> String {
    surface.screen_chars_to_string()
}

fn fill_ratio(surface: &Surface, area: Rect) -> f32 {
    let (cols, rows) = surface.dimensions();
    let x0 = area.x.min(cols);
    let y0 = area.y.min(rows);
    let x1 = area.x.saturating_add(area.cols).min(cols);
    let y1 = area.y.saturating_add(area.rows).min(rows);
    let area_cells = x1.saturating_sub(x0).saturating_mul(y1.saturating_sub(y0));
    if area_cells == 0 {
        return 0.0;
    }
    let occupied: usize = surface
        .screen_lines()
        .iter()
        .enumerate()
        .filter(|(y, _)| *y >= y0 && *y < y1)
        .flat_map(|(_, line)| line.visible_cells())
        .filter(|cell| {
            let x = cell.cell_index();
            x < x1
                && x.saturating_add(cell.width()) > x0
                && !cell.str().is_empty()
                && cell.str() != " "
                && cell.str() != "\u{2800}"
        })
        .map(|cell| {
            let x = cell.cell_index();
            cell.width()
                .min(x1.saturating_sub(x))
                .min(x.saturating_add(cell.width()).saturating_sub(x0))
        })
        .sum();
    occupied as f32 / area_cells as f32
}

#[test]
fn gpu_view_snapshots_cover_size_and_capability_matrix() {
    let sizes = [(100, 30), (160, 45), (240, 60)];
    let levels = [UnicodeLevel::Full, UnicodeLevel::Basic, UnicodeLevel::Ascii];
    let depths = [ColorDepth::Truecolor, ColorDepth::Ansi16, ColorDepth::None];
    let mut model = GpuModel::default();
    for tick in 0_u32..480 {
        model.absorb_frame(&fixture(tick, GpuKind::Apu, RuntimeStatus::Active));
    }
    let view = GpuView(&model);
    let board = Board::default();
    for (cols, rows) in sizes {
        for level in levels {
            for depth in depths {
                let mut surface = Surface::new(cols, rows);
                caps::test_override::with_unicode(level, || {
                    caps::test_override::with_color(depth, || {
                        board.paint(&view, &mut surface, Rect::full(cols, rows), 0, &[]);
                    });
                });
                let rendered = text(&surface);
                assert!(rendered.contains("GPU"));
                assert!(rendered.contains("Radeon 890M"));
                assert!(rendered.contains("gfx"));
                assert!(rendered.contains("shared"));
                assert!(rendered.contains("764 MHz [2/3]"));
                if cols == 240 {
                    // density follow-up (batch 49): measured 0.30
                    assert!(fill_ratio(&surface, Rect::full(cols, rows)) >= 0.30);
                }
                if level == UnicodeLevel::Ascii {
                    assert!(rendered.is_ascii());
                }
                let expected = board.placed(&view, Rect::full(cols, rows)).iter().fold(
                    thegn_metrics::collect::CollectorSet::empty(),
                    |mut set, placed| {
                        for id in view.demand(placed.id).iter() {
                            set.insert(id);
                        }
                        set
                    },
                );
                assert_eq!(
                    board.demand(&view, Rect::full(cols, rows), 1000).collectors,
                    expected
                );
            }
        }
    }
}

#[test]
fn apu_and_dgpu_memory_labels_are_distinct_and_demand_is_panel_static() {
    let mut model = GpuModel::default();
    model.absorb_frame(&fixture(1, GpuKind::Apu, RuntimeStatus::Active));
    let view = GpuView(&model);
    let placed = Board::default().placed(&view, Rect::full(160, 45));
    let mut surface = Surface::new(160, 45);
    Board::default().paint(&view, &mut surface, Rect::full(160, 45), 0, &[]);
    assert!(text(&surface).contains("shared"));
    let engine_panel = placed
        .iter()
        .find(|panel| panel.id.0 == "gpu.engines")
        .expect("engine panel placed");
    let engine_demand = view.demand(engine_panel.id);
    let mut expected = thegn_metrics::collect::CollectorSet::empty();
    expected.insert(CollectorId::GpuEngines);
    assert_eq!(engine_demand, expected);
    let device_panel = placed
        .iter()
        .find(|panel| panel.id.0 == "gpu.devices")
        .expect("device panel placed");
    let device_demand = view.demand(device_panel.id);
    expected = thegn_metrics::collect::CollectorSet::empty();
    expected.insert(CollectorId::GpuDevices);
    assert_eq!(device_demand, expected);
    let memory_panel = placed
        .iter()
        .find(|panel| panel.id.0 == "gpu.memory")
        .expect("memory panel placed");
    let memory_demand = view.demand(memory_panel.id);
    expected = thegn_metrics::collect::CollectorSet::empty();
    expected.insert(CollectorId::GpuDevices);
    expected.insert(CollectorId::GpuEngines);
    assert_eq!(memory_demand, expected);
    let clocks_panel = placed
        .iter()
        .find(|panel| panel.id.0 == "gpu.clocks")
        .expect("clocks panel placed");
    let clocks_demand = view.demand(clocks_panel.id);
    expected = thegn_metrics::collect::CollectorSet::empty();
    expected.insert(CollectorId::GpuEngines);
    expected.insert(CollectorId::AmdSmu);
    assert_eq!(clocks_demand, expected);
    assert!(!placed.is_empty());

    model.absorb_frame(&fixture(2, GpuKind::Dgpu, RuntimeStatus::Active));
    let view = GpuView(&model);
    let mut dedicated = Surface::new(160, 45);
    Board::default().paint(&view, &mut dedicated, Rect::full(160, 45), 0, &[]);
    assert!(text(&dedicated).contains("VRAM"));
}

#[test]
fn suspended_gpu_never_renders_zero_busy_or_clock_and_missing_is_absent() {
    let mut model = GpuModel::default();
    model.absorb_frame(&fixture(1, GpuKind::Dgpu, RuntimeStatus::Suspended));
    let view = GpuView(&model);
    let mut surface = Surface::new(160, 45);
    Board::default().paint(&view, &mut surface, Rect::full(160, 45), 0, &[]);
    let rendered = text(&surface);
    assert!(rendered.contains("suspended"));
    assert!(rendered.contains("not polled"));
    assert!(!rendered.contains("gfx 0%"));
    assert!(!rendered.contains("sclk 0 MHz"));
    assert!(rendered.contains(caps::active_glyphs().emdash));

    let mut no_marker = fixture(2, GpuKind::Dgpu, RuntimeStatus::Active);
    no_marker.parts.gpu_engines.devices[0].sclk.current_mhz = None;
    model.absorb_frame(&no_marker);
    let view = GpuView(&model);
    let mut surface = Surface::new(160, 45);
    Board::default().paint(&view, &mut surface, Rect::full(160, 45), 0, &[]);
    assert!(text(&surface).contains(&format!("sclk {}", caps::active_glyphs().emdash)));
}

#[test]
fn busy_history_is_slot_keyed_bounded_and_reuses_capacity_after_warmup() {
    let mut model = GpuModel::default();
    model.absorb_frame(&fixture(0, GpuKind::Apu, RuntimeStatus::Active));
    let device_capacity = model.devices.devices.capacity();
    let engine_capacity = model.engines.devices.capacity();
    let smu_capacity = model.smu.cards.capacity();
    let history_capacity = model.histories.capacity();
    for tick in 0_u32..1_000 {
        model.absorb_frame(&fixture(tick, GpuKind::Apu, RuntimeStatus::Active));
    }
    assert_eq!(model.histories.len(), 1);
    assert_eq!(model.histories[0].len, 480);
    let capacity = model.histories.capacity();
    assert_eq!(model.devices.devices.capacity(), device_capacity);
    assert_eq!(model.engines.devices.capacity(), engine_capacity);
    assert_eq!(model.smu.cards.capacity(), smu_capacity);
    assert_eq!(capacity, history_capacity);
    let first = model.histories[0].values
        [(model.histories[0].next + HISTORY_CAP_TEST - 1) % HISTORY_CAP_TEST];
    model.absorb_frame(&fixture(1_001, GpuKind::Apu, RuntimeStatus::Active));
    assert_eq!(model.histories.capacity(), capacity);
    let last = model.histories[0].values
        [(model.histories[0].next + HISTORY_CAP_TEST - 1) % HISTORY_CAP_TEST];
    assert_ne!(first, last);

    let mut frame = fixture(1_002, GpuKind::Apu, RuntimeStatus::Active);
    frame.parts.gpu_engines.devices[0].slot =
        GpuSlot::new("0000:05:00.0").expect("valid second slot");
    model.absorb_frame(&frame);
    assert_eq!(model.histories.len(), 2);
    assert_eq!(model.histories[0].slot, "0000:04:00.0");
    assert_eq!(model.histories[1].slot, "0000:05:00.0");
    frame.parts.gpu_engines.devices[0].slot =
        GpuSlot::new("0000:06:00.0").expect("valid replacement slot");
    model.absorb_frame(&frame);
    assert_eq!(model.histories.len(), 2);
    assert_eq!(model.histories[0].slot, "0000:06:00.0");
}

#[test]
fn absent_collector_renders_unknown_mark() {
    let mut frame = fixture(1, GpuKind::Dgpu, RuntimeStatus::Active);
    frame.health[CollectorId::GpuEngines.index()] = Health::Absent(Absent::NoPermission);
    let mut model = GpuModel::default();
    model.absorb_frame(&frame);
    let mut surface = Surface::new(160, 45);
    Board::default().paint(&GpuView(&model), &mut surface, Rect::full(160, 45), 0, &[]);
    assert!(text(&surface).contains("engine counters unavailable"));
}

const HISTORY_CAP_TEST: usize = 480;
