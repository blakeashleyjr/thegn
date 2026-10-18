use super::view::{GpuModel, GpuView, HISTORY_CAP};
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
        gfx_busy_pct: Some(if status == RuntimeStatus::Suspended {
            0
        } else {
            (25 + tick % 55) as u8
        }),
        vcn_busy_pct: Some(if status == RuntimeStatus::Suspended {
            0
        } else {
            (tick % 13) as u8
        }),
        vram_used: Some(300 * 1024 * 1024 + u64::from(tick % 150) * 1024 * 1024),
        vram_total: Some(512 * 1024 * 1024),
        gtt_used: Some(3 * 1024 * 1024 * 1024 + u64::from(tick % 150) * 2 * 1024 * 1024),
        gtt_total: Some(46 * 1024 * 1024 * 1024),
        sclk: DpmLevels {
            levels_mhz: [600, 764, 2900, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            len: 3,
            current_mhz: (status == RuntimeStatus::Active).then_some(match tick % 3 {
                0 => 600,
                1 => 764,
                _ => 2900,
            }),
        },
        mclk: DpmLevels {
            levels_mhz: [500, 2400, 2800, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            len: 3,
            current_mhz: (status == RuntimeStatus::Active).then_some(2400),
        },
    });
    let metrics = GpuMetrics {
        layout_verified: true,
        activity_gfx: Some((20 + tick % 70) as u16),
        activity_mm: Some((10 + tick % 60) as u16),
        activity_vcn: Some((tick % 80) as u16),
        activity_umc: Some((30 + tick % 50) as u16),
        activity_ipu: std::array::from_fn(|index| Some(((tick + index as u32 * 7) % 100) as u16)),
        power_socket: Some(12_000 + (tick % 60) * 150),
        power_current_stapm_limit: Some(28),
        temperature_gfx: Some(8_000 + (tick % 7) as u16 * 100),
        ..GpuMetrics::default()
    };
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

fn two_device_fixture(tick: u32) -> MonitorFrame {
    let mut frame = fixture(tick, GpuKind::Dgpu, RuntimeStatus::Active);
    let mut device = frame.parts.gpu_devices.devices[0].clone();
    device.slot = "0000:05:00.0".to_owned();
    device.card = 1;
    device.family = Some("Second GPU".to_owned());
    frame.parts.gpu_devices.devices.push(device);

    let mut engine = frame.parts.gpu_engines.devices[0];
    engine.slot = GpuSlot::new("0000:05:00.0").expect("valid second slot");
    engine.vram_used = Some(100 * 1024 * 1024);
    engine.gtt_used = Some(8 * 1024 * 1024 * 1024);
    frame.parts.gpu_engines.devices.push(engine);
    frame.parts.amd_smu.cards.push(AmdSmuCard {
        card: 1,
        suspended: false,
        decoded: AmdSmuStatus::Decoded(
            thegn_metrics::collect::parse::amd_gpu_metrics::Decoded::Full,
        ),
        metrics: GpuMetrics {
            layout_verified: true,
            power_socket: Some(8_000 + (tick % 20) * 100),
            power_current_stapm_limit: Some(20),
            ..GpuMetrics::default()
        },
    });
    frame
}

#[test]
fn power_history_normalizes_milliwatts_against_watt_limit() {
    let mut model = GpuModel::default();
    let mut sample = fixture(0, GpuKind::Apu, RuntimeStatus::Active);
    sample.parts.amd_smu.cards[0].metrics.power_socket = Some(17_300);
    sample.parts.amd_smu.cards[0]
        .metrics
        .power_current_stapm_limit = Some(28);
    model.absorb_frame(&sample);

    let history = &model.histories[0];
    let latest = history.power[(history.power_next + HISTORY_CAP - 1) % HISTORY_CAP];
    assert!((latest - 0.62).abs() < 0.01);
}

fn text(surface: &Surface) -> String {
    surface.screen_chars_to_string()
}

fn panel_text(surface: &Surface, rect: Rect) -> String {
    surface
        .screen_lines()
        .iter()
        .enumerate()
        .filter(|(y, _)| *y >= rect.y && *y < rect.y.saturating_add(rect.rows))
        .flat_map(|(_, line)| line.visible_cells())
        .filter(|cell| {
            cell.cell_index() >= rect.x && cell.cell_index() < rect.x.saturating_add(rect.cols)
        })
        .map(|cell| cell.str().to_owned())
        .collect()
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
                assert!(rendered.contains("GPU0 gfx 0-100%"));
                assert!(rendered.contains("vcn %"));
                assert!(rendered.contains("shared"));
                assert!(rendered.contains("MHz ["));
                if cols == 240 {
                    // density follow-up (batch 51): estimated, primary sets from the printed value
                    let ratio = fill_ratio(&surface, Rect::full(cols, rows));
                    assert!(
                        ratio >= 0.30,
                        "fill ratio {ratio:.3} at {cols}x{rows} {level:?}/{depth:?}"
                    );
                }
                if level == UnicodeLevel::Ascii {
                    assert!(rendered.is_ascii());
                }
                let mut expected = thegn_metrics::collect::CollectorSet::empty();
                expected.insert(CollectorId::GpuDevices);
                expected.insert(CollectorId::GpuEngines);
                expected.insert(CollectorId::AmdSmu);
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
    let board = Board::default();
    let placed = board.placed(&view, Rect::full(160, 45));
    let mut surface = Surface::new(160, 45);
    board.paint(&view, &mut surface, Rect::full(160, 45), 0, &[]);
    let engine_panel = placed
        .iter()
        .find(|panel| panel.id.0 == "gpu.engines")
        .expect("engine panel placed");
    let engine_demand = view.demand(engine_panel.id);
    let mut expected = thegn_metrics::collect::CollectorSet::empty();
    expected.insert(CollectorId::GpuEngines);
    expected.insert(CollectorId::AmdSmu);
    expected.insert(CollectorId::GpuDevices);
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
    let apu_memory = panel_text(&surface, memory_panel.rect);
    assert!(apu_memory.contains("shared"), "{apu_memory}");
    assert!(apu_memory.contains("vram"), "{apu_memory}");
    assert!(apu_memory.contains("gtt"), "{apu_memory}");
    assert!(apu_memory.contains("301"), "{apu_memory}");
    assert!(apu_memory.contains("512"), "{apu_memory}");
    assert!(apu_memory.contains("3.0"), "{apu_memory}");
    assert!(apu_memory.contains("46"), "{apu_memory}");
    let clocks_panel = placed
        .iter()
        .find(|panel| panel.id.0 == "gpu.clocks")
        .expect("clocks panel placed");
    let clocks_demand = view.demand(clocks_panel.id);
    expected = thegn_metrics::collect::CollectorSet::empty();
    expected.insert(CollectorId::GpuEngines);
    expected.insert(CollectorId::GpuDevices);
    expected.insert(CollectorId::AmdSmu);
    assert_eq!(clocks_demand, expected);
    assert!(!placed.is_empty());

    model.absorb_frame(&fixture(2, GpuKind::Dgpu, RuntimeStatus::Active));
    let view = GpuView(&model);
    let mut dedicated = Surface::new(160, 45);
    Board::default().paint(&view, &mut dedicated, Rect::full(160, 45), 0, &[]);
    let dedicated_memory = panel_text(&dedicated, memory_panel.rect);
    assert!(dedicated_memory.contains("VRAM"), "{dedicated_memory}");
    assert!(dedicated_memory.contains("GTT"), "{dedicated_memory}");
    assert!(!dedicated_memory.contains("shared"), "{dedicated_memory}");
}

#[test]
fn memory_uses_device_totals_without_an_engine_row() {
    let mut frame = fixture(1, GpuKind::Dgpu, RuntimeStatus::Active);
    frame.parts.gpu_engines.devices.clear();
    let mut model = GpuModel::default();
    model.absorb_frame(&frame);
    let view = GpuView(&model);
    let board = Board::default();
    let memory_panel = board
        .placed(&view, Rect::full(160, 45))
        .into_iter()
        .find(|panel| panel.id.0 == "gpu.memory")
        .expect("memory panel placed");
    let mut surface = Surface::new(160, 45);
    board.paint(&view, &mut surface, Rect::full(160, 45), 0, &[]);
    let memory = panel_text(&surface, memory_panel.rect);
    assert!(memory.contains("VRAM"), "{memory}");
    assert!(memory.contains("GTT"), "{memory}");
    assert!(memory.contains("512"), "{memory}");
    assert!(memory.contains("46"), "{memory}");
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
    assert!(!rendered.contains("vcn 0%"));
    assert!(!rendered.contains("sclk 0 MHz"));
    assert!(!rendered.contains("301"));
    assert!(rendered.contains(caps::active_glyphs().emdash));
    assert!(model.histories[0].activity.iter().all(|ring| ring.len == 0));

    let mut suspending_model = GpuModel::default();
    suspending_model.absorb_frame(&fixture(1, GpuKind::Dgpu, RuntimeStatus::Suspending));
    let mut suspending_surface = Surface::new(160, 45);
    Board::default().paint(
        &GpuView(&suspending_model),
        &mut suspending_surface,
        Rect::full(160, 45),
        0,
        &[],
    );
    let suspending = text(&suspending_surface);
    assert!(!suspending.contains("gfx 26%"));
    assert!(!suspending.contains("vcn 1%"));

    let mut no_marker = fixture(2, GpuKind::Dgpu, RuntimeStatus::Active);
    no_marker.parts.gpu_engines.devices[0].sclk.current_mhz = None;
    model.absorb_frame(&no_marker);
    let view = GpuView(&model);
    let mut surface = Surface::new(160, 45);
    Board::default().paint(&view, &mut surface, Rect::full(160, 45), 0, &[]);
    assert!(text(&surface).contains(&format!("sclk {}", caps::active_glyphs().emdash)));

    let mut outside_ladder = fixture(3, GpuKind::Dgpu, RuntimeStatus::Active);
    outside_ladder.parts.gpu_engines.devices[0].sclk.current_mhz = Some(1_234);
    model.absorb_frame(&outside_ladder);
    let mut surface = Surface::new(160, 45);
    Board::default().paint(&GpuView(&model), &mut surface, Rect::full(160, 45), 0, &[]);
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
    let frames = (0_u32..1_000)
        .map(|tick| fixture(tick, GpuKind::Apu, RuntimeStatus::Active))
        .collect::<Vec<_>>();
    let (_, counts) = crate::proc_workload_alloc::measure(|| {
        for frame in &frames {
            model.absorb_frame(frame);
        }
    });
    assert_eq!(
        counts.allocation_calls, 0,
        "1000 warm GPU absorbs allocated: {counts:?}"
    );
    assert_eq!(
        counts.realloc_calls, 0,
        "1000 warm GPU absorbs reallocated: {counts:?}"
    );
    assert_eq!(model.histories.len(), 1);
    assert_eq!(model.histories[0].len, 480);
    assert_eq!(model.histories[0].memory_len, 480);
    assert_eq!(model.histories[0].gtt_len, 480);
    assert_eq!(model.histories[0].clocks_len, 480);
    assert_eq!(model.histories[0].power_len, 480);
    assert!(
        model.histories[0]
            .activity
            .iter()
            .all(|ring| ring.len == HISTORY_CAP)
    );
    let capacity = model.histories.capacity();
    assert_eq!(model.devices.devices.capacity(), device_capacity);
    assert_eq!(model.engines.devices.capacity(), engine_capacity);
    assert_eq!(model.smu.cards.capacity(), smu_capacity);
    assert_eq!(capacity, history_capacity);
    let first =
        model.histories[0].values[(model.histories[0].next + HISTORY_CAP - 1) % HISTORY_CAP];
    model.absorb_frame(&fixture(1_001, GpuKind::Apu, RuntimeStatus::Active));
    assert_eq!(model.histories.capacity(), capacity);
    let last = model.histories[0].values[(model.histories[0].next + HISTORY_CAP - 1) % HISTORY_CAP];
    assert_ne!(first, last);

    let mut frame = fixture(1_002, GpuKind::Apu, RuntimeStatus::Active);
    frame.parts.gpu_engines.devices[0].slot =
        GpuSlot::new("0000:05:00.0").expect("valid second slot");
    frame.parts.gpu_engines.devices[0].runtime_status = RuntimeStatus::Suspended;
    model.absorb_frame(&frame);
    assert_eq!(model.histories.len(), 1);
    assert_eq!(model.histories[0].slot, "0000:05:00.0");
    assert_eq!(model.histories[0].len, 0);
    assert!(model.histories[0].activity.iter().all(|ring| ring.len == 0));
    frame.parts.gpu_engines.devices[0].slot =
        GpuSlot::new("0000:06:00.0").expect("valid replacement slot");
    model.absorb_frame(&frame);
    assert_eq!(model.histories.len(), 1);
    assert_eq!(model.histories[0].slot, "0000:06:00.0");
    assert_eq!(model.histories[0].len, 0);
}

#[test]
fn aux_history_draws_each_device_and_activity_rings_require_verified_layout() {
    let mut model = GpuModel::default();
    for tick in 0_u32..32 {
        model.absorb_frame(&two_device_fixture(tick));
    }
    let view = GpuView(&model);
    let rect = Rect::full(100, 18);
    let body = Rect {
        x: 1,
        y: 1,
        cols: 98,
        rows: 16,
    };
    let mut memory_surface = Surface::new(100, 18);
    view.paint_aux_history(&mut memory_surface, rect, body, true);
    let memory = text(&memory_surface);
    assert!(memory.contains("GPU0 VRAM / total"), "{memory}");
    assert!(memory.contains("GPU1 VRAM / total"), "{memory}");
    assert!(memory.contains("GPU0 GTT / total"), "{memory}");
    assert!(memory.contains("GPU1 GTT / total"), "{memory}");
    let label_row = |label: &str| {
        memory_surface
            .screen_lines()
            .iter()
            .position(|line| {
                line.visible_cells()
                    .fold(String::new(), |mut row, cell| {
                        row.push_str(cell.str());
                        row
                    })
                    .contains(label)
            })
            .expect("history label rendered")
    };
    let vram_row = label_row("GPU0 VRAM / total");
    let gtt_row = label_row("GPU0 GTT / total");
    assert!(
        gtt_row >= vram_row + 3,
        "VRAM area graph has fewer than two rows: VRAM {vram_row}, GTT {gtt_row}"
    );

    let mut clocks_surface = Surface::new(100, 18);
    view.paint_aux_history(&mut clocks_surface, rect, body, false);
    let clocks = text(&clocks_surface);
    assert!(clocks.contains("GPU0 gfx clock / max"), "{clocks}");
    assert!(clocks.contains("GPU1 gfx clock / max"), "{clocks}");
    assert!(clocks.contains("GPU0 power / cap"), "{clocks}");
    assert!(clocks.contains("GPU1 power / cap"), "{clocks}");

    let mut unverified = two_device_fixture(33);
    unverified.parts.amd_smu.cards[0].metrics.layout_verified = false;
    unverified.parts.amd_smu.cards[0].metrics.activity_gfx = Some(88);
    let mut unverified_model = GpuModel::default();
    unverified_model.absorb_frame(&unverified);
    assert!(
        unverified_model.histories[0]
            .activity
            .iter()
            .all(|ring| ring.len == 0)
    );
    assert_eq!(unverified_model.histories[0].power_len, 1);
}

#[test]
fn busy_area_and_activity_rows_do_not_overlap_with_vcn_present() {
    let mut model = GpuModel::default();
    for tick in 0_u32..8 {
        model.absorb_frame(&fixture(tick, GpuKind::Apu, RuntimeStatus::Active));
    }
    let history = &model.histories[0];
    let activity_rows = history.activity.iter().filter(|ring| ring.len > 0).count();
    let graph_rows = 20_usize.saturating_sub(2 + activity_rows.min(18));
    let activity_start = 20 - 1 - activity_rows.min(18);
    let busy_graph_rows = 1..1 + graph_rows;
    let activity_label_rows = activity_start..19;
    assert!(
        activity_label_rows.all(|row| !busy_graph_rows.contains(&row)),
        "activity labels overlap busy graph rows {busy_graph_rows:?}"
    );

    let mut surface = Surface::new(40, 20);
    let rect = Rect::full(40, 20);
    GpuView(&model).paint_busy_history(&mut surface, rect, rect);
    let rendered = text(&surface);
    assert!(rendered.contains("GPU0 gfx 0-100%"), "{rendered}");
    assert!(rendered.contains("vcn %"), "{rendered}");
    let gfx_label_row = surface
        .screen_lines()
        .iter()
        .position(|line| {
            line.visible_cells()
                .fold(String::new(), |mut row, cell| {
                    row.push_str(cell.str());
                    row
                })
                .contains("GPU0 gfx 0-100%")
        })
        .expect("gfx activity label rendered");
    assert!(
        !busy_graph_rows.contains(&gfx_label_row),
        "gfx label row {gfx_label_row} overlaps busy graph {busy_graph_rows:?}"
    );
}

#[test]
fn activity_gap_holds_the_last_sample_and_suspended_frames_do_not_push() {
    let mut first = fixture(1, GpuKind::Apu, RuntimeStatus::Active);
    first.parts.amd_smu.cards[0].metrics.activity_gfx = Some(64);
    let mut model = GpuModel::default();
    model.absorb_frame(&first);

    let mut gap = fixture(2, GpuKind::Apu, RuntimeStatus::Active);
    gap.parts.amd_smu.cards[0].metrics.activity_gfx = Some(101);
    model.absorb_frame(&gap);
    let ring = &model.histories[0].activity[0];
    assert_eq!(ring.len, 2);
    let previous = ring.values[(ring.next + HISTORY_CAP - 2) % HISTORY_CAP];
    let held = ring.values[(ring.next + HISTORY_CAP - 1) % HISTORY_CAP];
    assert_eq!(previous, held);

    let suspended = fixture(3, GpuKind::Apu, RuntimeStatus::Suspended);
    model.absorb_frame(&suspended);
    assert_eq!(model.histories[0].activity[0].len, 2);
}

#[test]
fn absent_collector_renders_unknown_mark() {
    let mut frame = fixture(1, GpuKind::Dgpu, RuntimeStatus::Active);
    frame.health[CollectorId::GpuEngines.index()] = Health::Absent(Absent::NoPermission);
    let mut model = GpuModel::default();
    model.absorb_frame(&frame);
    let mut surface = Surface::new(160, 45);
    Board::default().paint(&GpuView(&model), &mut surface, Rect::full(160, 45), 0, &[]);
    let rendered = text(&surface);
    assert!(rendered.contains(caps::glyph(caps::Glyph::Lock)));
    assert!(rendered.contains("root-only"));
    assert!(rendered.contains("engine counters unavailable"));
}

#[test]
fn warming_gpu_view_uses_ellipsis_without_absent_mark() {
    let model = GpuModel::default();
    let mut surface = Surface::new(160, 45);
    Board::default().paint(&GpuView(&model), &mut surface, Rect::full(160, 45), 0, &[]);
    let rendered = text(&surface);
    assert!(rendered.contains(caps::active_glyphs().ellipsis));
    assert!(!rendered.contains(caps::active_glyphs().emdash));
}

#[test]
fn short_engine_history_reaches_the_right_edge() {
    let mut model = GpuModel::default();
    for tick in 0_u32..20 {
        model.absorb_frame(&fixture(tick, GpuKind::Apu, RuntimeStatus::Active));
    }
    let mut surface = Surface::new(40, 8);
    let rect = Rect::full(40, 8);
    GpuView(&model).paint_busy_history(&mut surface, rect, rect);
    let rightmost_is_drawn = surface
        .screen_lines()
        .iter()
        .enumerate()
        .filter(|(row, _)| (1..6).contains(row))
        .flat_map(|(_, line)| line.visible_cells())
        .filter(|cell| cell.cell_index() == 39)
        .any(|cell| !cell.str().is_empty() && cell.str() != " " && cell.str() != "\u{2800}");
    assert!(
        rightmost_is_drawn,
        "short graph did not reach its right edge"
    );
}

#[test]
fn activity_history_places_newest_sample_in_last_inner_column() {
    let mut sample = fixture(1, GpuKind::Apu, RuntimeStatus::Active);
    sample.parts.gpu_engines.devices[0].gfx_busy_pct = None;
    let metrics = &mut sample.parts.amd_smu.cards[0].metrics;
    metrics.activity_gfx = Some(75);
    metrics.activity_mm = None;
    metrics.activity_vcn = None;
    metrics.activity_umc = None;
    metrics.activity_ipu = [None; 8];
    let mut model = GpuModel::default();
    model.absorb_frame(&sample);

    let mut surface = Surface::new(40, 8);
    let rect = Rect::full(40, 8);
    GpuView(&model).paint_busy_history(&mut surface, rect, rect);
    let gfx_row = surface
        .screen_lines()
        .iter()
        .position(|line| {
            line.visible_cells()
                .fold(String::new(), |mut row, cell| {
                    row.push_str(cell.str());
                    row
                })
                .contains("GPU0 gfx 0-100%")
        })
        .expect("gfx activity label rendered");
    let newest_column = surface.screen_lines()[gfx_row]
        .visible_cells()
        .any(|cell| cell.cell_index() == 39 && cell.str() != " " && cell.str() != "\u{2800}");
    assert!(
        newest_column,
        "newest activity sample was not right-aligned"
    );
}
