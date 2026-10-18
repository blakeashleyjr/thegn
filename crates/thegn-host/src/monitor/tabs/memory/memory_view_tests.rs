use super::{MemoryModel, MemoryView};
use crate::telemetry::{ScaleMode, TelemetryHistory, Window};
use crate::{
    caps,
    compositor::Rect,
    monitor::board::{Board, TabView as _},
};
use termwiz::surface::Surface;
use thegn_core::{
    mem_health::{self, MemHealthInput, Verdict},
    termcaps::{ColorDepth, UnicodeLevel},
};
use thegn_metrics::StatsSnapshot;
use thegn_metrics::collect::{
    CollectorId,
    frame::MonitorFrame,
    parse::{meminfo, psi, vmstat},
};

fn fixture(pressure: bool) -> MonitorFrame {
    let mut frame = MonitorFrame::default();
    meminfo::parse(
        b"MemTotal: 8388608 kB\nMemAvailable: 4194304 kB\nMemFree: 524288 kB\nBuffers: 262144 kB\nCached: 2097152 kB\nShmem: 131072 kB\nSlab: 131072 kB\nAnonPages: 4194304 kB\nActive(file): 1048576 kB\nInactive(file): 1048576 kB\nDirty: 1024 kB\nWriteback: 512 kB\nSwapTotal: 2097152 kB\nSwapFree: 1048576 kB\nZswap: 32768 kB\nHugePages_Total: 4\nHugePages_Free: 2\n",
        &mut frame.parts.meminfo.raw,
    );
    vmstat::parse(
        if pressure {
            b"pswpin 400\npgmajfault 9000\npgsteal_direct 1500\npgsteal_kswapd 500\npgscan_direct 3000\npgscan_kswapd 1000\n" as &[u8]
        } else {
            b"pswpin 0\npgmajfault 0\npgsteal_direct 0\npgsteal_kswapd 0\npgscan_direct 0\npgscan_kswapd 0\n"
        },
        &mut frame.parts.vmstat.raw,
    );
    frame.parts.vmstat.rates[thegn_metrics::collect::model::vmstat::VmRate::Pswpin as usize] =
        Some(if pressure { 1.0 } else { 0.0 });
    frame.parts.vmstat.rates[thegn_metrics::collect::model::vmstat::VmRate::Pgmajfault as usize] =
        Some(if pressure { 250.0 } else { 0.0 });
    frame.parts.vmstat.rates[thegn_metrics::collect::model::vmstat::VmRate::StealDirect as usize] =
        Some(if pressure { 1200.0 } else { 0.0 });
    frame.parts.vmstat.rates[thegn_metrics::collect::model::vmstat::VmRate::StealKswapd as usize] =
        Some(if pressure { 5.0 } else { 0.0 });
    frame.parts.vmstat.rates[thegn_metrics::collect::model::vmstat::VmRate::ScanDirect as usize] =
        Some(20.0);
    frame.parts.vmstat.rates[thegn_metrics::collect::model::vmstat::VmRate::ScanKswapd as usize] =
        Some(30.0);
    frame.parts.vmstat.rates[thegn_metrics::collect::model::vmstat::VmRate::Pswpin as usize] =
        Some(if pressure { 1.0 } else { 0.0 });
    frame.parts.vmstat.rates[thegn_metrics::collect::model::vmstat::VmRate::RefaultAnon as usize] =
        Some(0.0);
    frame.parts.vmstat.rates[thegn_metrics::collect::model::vmstat::VmRate::RefaultFile as usize] =
        Some(0.0);
    frame.parts.vmstat.rates[thegn_metrics::collect::model::vmstat::VmRate::Allocstall as usize] =
        Some(0.0);
    frame.parts.psi.memory.present = true;
    frame.parts.psi.memory.file = psi::parse(if pressure {
        b"some avg10=6.00 avg60=4.00 avg300=2.00 total=100\nfull avg10=11.00 avg60=8.00 avg300=3.00 total=20\n"
    } else {
        b"some avg10=0.00 avg60=0.00 avg300=0.00 total=0\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=0\n"
    });
    frame.sampled.insert(CollectorId::Meminfo);
    frame.sampled.insert(CollectorId::Vmstat);
    frame.sampled.insert(CollectorId::Psi);
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
fn absorb_frame_matches_mem_health_for_healthy_and_pressure_inputs() {
    let frame = fixture(false);
    let mut healthy = MemoryModel::default();
    healthy.absorb_frame(&frame);
    assert_eq!(healthy.health.verdict, Verdict::Ok);
    assert_eq!(
        healthy.health,
        mem_health::evaluate(&MemHealthInput {
            mem_total_bytes: Some(8 * 1024 * 1024 * 1024),
            mem_available_bytes: Some(4 * 1024 * 1024 * 1024),
            swap_total_bytes: Some(2 * 1024 * 1024 * 1024),
            swap_free_bytes: Some(1024 * 1024 * 1024),
            swap_in_per_sec: Some(0.0),
            psi_some_avg10: Some(0.0),
            psi_full_avg10: Some(0.0),
            refault_pages_per_sec: Some(0.0),
            majflt_per_sec: Some(0.0),
            direct_reclaim_pages_per_sec: Some(0.0),
            allocstall_per_sec: Some(0.0),
            secs_since_oom_kill: None,
            slice_used_frac_of_high: None,
            os_level: None,
        })
    );
    let mut pressure = MemoryModel::default();
    pressure.absorb_frame(&fixture(true));
    assert_eq!(pressure.reclaim, Some(1200.0));
    assert_eq!(pressure.steal, Some(5.0));
    assert_eq!(pressure.scan, Some(50.0));
    assert_eq!(pressure.health.verdict, Verdict::Thrashing);
    assert!(
        pressure
            .health
            .reasons
            .contains(thegn_core::mem_health::Reason::PsiFull)
    );
    assert_eq!(
        pressure.health,
        mem_health::evaluate(&MemHealthInput {
            mem_total_bytes: Some(8 * 1024 * 1024 * 1024),
            mem_available_bytes: Some(4 * 1024 * 1024 * 1024),
            swap_total_bytes: Some(2 * 1024 * 1024 * 1024),
            swap_free_bytes: Some(1024 * 1024 * 1024),
            swap_in_per_sec: Some(1.0),
            psi_some_avg10: Some(6.0),
            psi_full_avg10: Some(11.0),
            refault_pages_per_sec: Some(0.0),
            majflt_per_sec: Some(250.0),
            direct_reclaim_pages_per_sec: Some(1200.0),
            allocstall_per_sec: Some(0.0),
            secs_since_oom_kill: None,
            slice_used_frac_of_high: None,
            os_level: None,
        })
    );
}

#[test]
fn absent_swap_is_hidden_and_limit_panel_demands_cgroup_slice() {
    let mut model = MemoryModel::default();
    model.absorb_frame(&fixture(false));
    model.swap_total = Some(0);
    let view = MemoryView(&model);
    let placed = Board::default().placed(&view, Rect::full(160, 45));
    assert!(!placed.iter().any(|panel| panel.id.0 == "memory.swap"));
    assert!(placed.iter().any(|panel| panel.id.0 == "memory.limit"));
    let demand = Board::default()
        .demand(&view, Rect::full(160, 45), 1000)
        .collectors;
    assert!(demand.contains(CollectorId::CgroupSlice));
    model.swap_total = Some(1);
    let mut cgroup_frame = fixture(false);
    cgroup_frame.parts.cgroup_slice.found = true;
    cgroup_frame.parts.cgroup_slice.sample.mem_current = Some(1024);
    model.absorb_frame(&cgroup_frame);
    let view = MemoryView(&model);
    let demand = Board::default()
        .demand(&view, Rect::full(160, 45), 1000)
        .collectors;
    assert!(demand.contains(CollectorId::CgroupSlice));
}

#[test]
fn composition_keeps_available_distinct_from_free() {
    let mut model = MemoryModel::default();
    model.absorb_frame(&fixture(false));
    assert_eq!(model.available, Some(4 * 1024 * 1024 * 1024));
    assert_eq!(model.free, Some(512 * 1024 * 1024));
    assert_eq!(model.used, Some(4 * 1024 * 1024 * 1024));
    let view = MemoryView(&model);
    let mut surface = Surface::new(160, 45);
    Board::default().paint(&view, &mut surface, Rect::full(160, 45), 0, &[]);
    let rendered = text(&surface);
    let available = crate::monitor::tabs::memory::view::fmt_bytes(Some(model.available.unwrap()));
    let free = crate::monitor::tabs::memory::view::fmt_bytes(Some(model.free.unwrap()));
    let available_line = rendered
        .lines()
        .find(|line| line.contains("available"))
        .unwrap();
    let free_line = rendered.lines().find(|line| line.contains("free")).unwrap();
    assert!(available_line.contains(&available), "{rendered}");
    assert!(free_line.contains(&free), "{rendered}");
    assert!(!free_line.contains(&available), "{rendered}");

    let mut without_free = MonitorFrame::default();
    meminfo::parse(
        b"MemTotal: 8388608 kB\nMemAvailable: 4194304 kB\n",
        &mut without_free.parts.meminfo.raw,
    );
    let mut missing_free_model = MemoryModel::default();
    missing_free_model.absorb_frame(&without_free);
    let mut missing_free_surface = Surface::new(160, 45);
    Board::default().paint(
        &MemoryView(&missing_free_model),
        &mut missing_free_surface,
        Rect::full(160, 45),
        0,
        &[],
    );
    let missing_free = text(&missing_free_surface);
    let free_line = missing_free.lines().find(|line| line.contains("free"));
    assert!(
        free_line.is_none_or(|line| {
            line.contains(caps::active_glyphs().emdash) && !line.contains(&available)
        }),
        "{missing_free}"
    );
}

#[test]
fn board_snapshots_cover_size_and_capability_matrix() {
    let sizes = [(100, 30), (160, 45), (240, 60)];
    let levels = [UnicodeLevel::Full, UnicodeLevel::Basic, UnicodeLevel::Ascii];
    let depths = [ColorDepth::Truecolor, ColorDepth::Ansi16, ColorDepth::None];
    for (cols, rows) in sizes {
        for level in levels {
            for depth in depths {
                let mut model = MemoryModel::default();
                for sample in 0_u32..120 {
                    let mut frame = fixture(true);
                    frame.parts.vmstat.rates
                        [thegn_metrics::collect::model::vmstat::VmRate::StealDirect as usize] =
                        Some(1000.0 + (sample % 20) as f64 * 50.0);
                    if let Some(mut some) = frame.parts.psi.memory.file.some {
                        some.avg10 = 100 + (sample % 20) * 25;
                        frame.parts.psi.memory.file.some = Some(some);
                    }
                    if let Some(mut full) = frame.parts.psi.memory.file.full {
                        full.avg10 = 1000 + (sample % 15) * 40;
                        frame.parts.psi.memory.file.full = Some(full);
                    }
                    frame.parts.cgroup_slice.sample.mem_current =
                        Some(1024 + u64::from(sample % 20) * 128);
                    frame.parts.cgroup_slice.sample.mem_max =
                        Some(thegn_metrics::collect::model::cgroup::Limit::Value(8192));
                    model.absorb_frame(&frame);
                }
                let mut telemetry = TelemetryHistory::default();
                for sample in 0_u32..120 {
                    telemetry.push(
                        &StatsSnapshot {
                            mem_gib: Some((4.0 + (sample % 31) as f32 / 10.0, 8.0)),
                            swap_gib: Some((0.5 + (sample % 17) as f32 / 10.0, 2.0)),
                            ..Default::default()
                        },
                        1000 + u64::from(sample) * 1000,
                    );
                }
                model.absorb_history(&telemetry, Window::EVERYTHING, ScaleMode::Window, 120_000);
                model.swap_total = Some(2 * 1024 * 1024 * 1024);
                model.swap_free = Some(1024 * 1024 * 1024);
                model.zswap = Some(32 * 1024 * 1024);
                let view = MemoryView(&model);
                let mut surface = Surface::new(cols, rows);
                let board = Board::default();
                caps::test_override::with_unicode(level, || {
                    caps::test_override::with_color(depth, || {
                        board.paint(&view, &mut surface, Rect::full(cols, rows), 0, &[]);
                    });
                });
                let rendered = text(&surface);
                assert!(rendered.contains("Memory"));
                assert!(rendered.contains("health"));
                if cols == 240 {
                    let ratio = fill_ratio(&surface, Rect::full(cols, rows));
                    // density follow-up (batch 49): estimated floor 0.30.
                    if level == UnicodeLevel::Ascii {
                        assert!(ratio >= 0.30, "ASCII fill ratio {ratio:.3}");
                    } else {
                        assert!(ratio >= 0.30, "Unicode fill ratio {ratio:.3}");
                    }
                    assert!(rendered.contains("used / available history"));
                    assert!(rendered.contains("PSI history"));
                    assert!(rendered.contains("direct reclaim history"));
                    assert!(rendered.contains("direct reclaim"));
                    assert!(rendered.contains("kswapd steal"));
                    assert!(rendered.contains("refaults"));
                    assert!(rendered.contains("allocstall"));
                    assert!(rendered.contains("Container limit"));
                    assert!(rendered.contains("thegn.slice usage history"));
                    assert!(rendered.contains("swap history"));
                }
                if level == UnicodeLevel::Ascii {
                    assert!(rendered.is_ascii());
                }
                let placed = board.placed(&view, Rect::full(cols, rows));
                let solution = board.solve(&view, Rect::full(cols, rows));
                if cols == 100 {
                    assert!(!solution.dropped.is_empty());
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
                assert_eq!(
                    board.demand(&view, Rect::full(cols, rows), 1000).collectors,
                    expected
                );
            }
        }
    }
}

#[test]
fn slice_limit_history_stays_bounded_after_warmup() {
    let mut model = MemoryModel::default();
    for sample in 0_u32..1_000 {
        let mut frame = MonitorFrame::default();
        frame.parts.cgroup_slice.sample.mem_current = Some(1024 + u64::from(sample % 37) * 64);
        frame.parts.cgroup_slice.sample.mem_max =
            Some(thegn_metrics::collect::model::cgroup::Limit::Value(8192));
        model.absorb_frame(&frame);
    }
    assert_eq!(model.limit_history.len, 120);
}

#[test]
fn slice_limit_uses_the_lower_numeric_ceiling() {
    use thegn_metrics::collect::model::cgroup::Limit;

    assert_eq!(
        super::min_numeric_limit(Some(Limit::Value(8192)), Some(Limit::Value(4096))),
        Some(4096)
    );
}

#[test]
fn limit_panel_uses_absent_mark_and_unlimited_label() {
    use thegn_metrics::collect::model::cgroup::Limit;

    let mut model = MemoryModel::default();
    let mut frame = MonitorFrame::default();
    model.absorb_frame(&frame);
    let view = MemoryView(&model);
    let board = Board::default();
    let limit = board
        .placed(&view, Rect::full(160, 45))
        .into_iter()
        .find(|panel| panel.id.0 == "memory.limit")
        .expect("limit panel is placed");
    let mut missing = Surface::new(limit.rect.cols, limit.rect.rows);
    view.paint(
        &mut missing,
        &limit,
        Rect::full(limit.rect.cols, limit.rect.rows),
        0,
        &[],
    );
    assert!(text(&missing).contains(caps::active_glyphs().emdash));

    frame.parts.cgroup_slice.effective_limit = Some(Limit::Max);
    frame.parts.cgroup_slice.own_current = Some(1024);
    frame.parts.cgroup_slice.sample.mem_current = Some(2048);
    frame.parts.cgroup_slice.sample.mem_high = Some(Limit::Max);
    frame.parts.cgroup_slice.sample.mem_max = Some(Limit::Max);
    model.absorb_frame(&frame);
    let view = MemoryView(&model);
    let mut unlimited = Surface::new(limit.rect.cols, limit.rect.rows);
    view.paint(
        &mut unlimited,
        &limit,
        Rect::full(limit.rect.cols, limit.rect.rows),
        0,
        &[],
    );
    assert!(text(&unlimited).contains("no limit"));
    assert!(!text(&unlimited).contains(caps::active_glyphs().emdash));

    let mut ascii = Surface::new(limit.rect.cols, limit.rect.rows);
    caps::test_override::with_unicode(UnicodeLevel::Ascii, || {
        view.paint(
            &mut ascii,
            &limit,
            Rect::full(limit.rect.cols, limit.rect.rows),
            0,
            &[],
        );
    });
    let ascii_text = text(&ascii);
    assert!(ascii_text.is_ascii());
    assert!(ascii_text.contains("thegn.slice 2.0K - no limit"));
}

#[test]
fn missing_values_render_the_capability_absent_mark() {
    let mut model = MemoryModel::default();
    model.absorb_frame(&MonitorFrame::default());
    let view = MemoryView(&model);
    let mut surface = Surface::new(100, 30);
    Board::default().paint(&view, &mut surface, Rect::full(100, 30), 0, &[]);
    assert!(text(&surface).contains(caps::active_glyphs().emdash));
    let rendered = text(&surface);
    let zero_bytes = crate::monitor::kit::edge_fmt(thegn_core::numfmt::NumFmt::default()).fmt(
        0.0,
        thegn_core::numfmt::Unit::Bytes,
        12,
    );
    assert!(!rendered.contains(zero_bytes.trim()));
}

#[test]
fn a_positive_oom_kill_rate_triggers_the_oom_verdict() {
    let mut frame = fixture(false);
    frame.at_ms = 12_345;
    frame.parts.vmstat.rates[thegn_metrics::collect::model::vmstat::VmRate::OomKill as usize] =
        Some(1.0);
    let mut model = MemoryModel::default();
    model.absorb_frame(&frame);
    assert_eq!(model.health.verdict, Verdict::Oom);
    frame.at_ms += 60_001;
    frame.parts.vmstat.rates[thegn_metrics::collect::model::vmstat::VmRate::OomKill as usize] =
        Some(0.0);
    model.absorb_frame(&frame);
    assert_ne!(model.health.verdict, Verdict::Oom);
}

#[test]
fn rate_sums_preserve_a_single_reported_counter() {
    assert_eq!(super::sum_rates(Some(3.0), None), Some(3.0));
    assert_eq!(super::sum_rates(None, Some(4.0)), Some(4.0));
    assert_eq!(super::sum_rates(None, None), None);
}

#[test]
fn memory_history_keeps_only_the_latest_bounded_window() {
    let mut history = super::MemoryHistory::default();
    for sample in 0_u16..125 {
        history.push(f32::from(sample));
    }
    let mut recent = [0.0_f32; 120];
    let (len, peak) = history.copy_recent(&mut recent);
    assert_eq!(len, 120);
    assert_eq!(peak, 124.0);
    assert_eq!(recent[0], 5.0);
    assert_eq!(recent[119], 124.0);
}

#[test]
fn short_history_is_right_aligned_without_upsampling() {
    assert_eq!(super::right_aligned_window(3, 10), (0, 3, 7));
    assert_eq!(super::right_aligned_window(12, 10), (2, 10, 0));
}

#[test]
fn short_memory_histories_are_right_aligned_by_the_painters() {
    let mut model = MemoryModel::default();
    for sample in 0_u64..10 {
        let mut frame = fixture(true);
        if let Some(mut some) = frame.parts.psi.memory.file.some {
            some.avg10 = 100 + sample as u32 * 100;
            frame.parts.psi.memory.file.some = Some(some);
        }
        model.absorb_frame(&frame);
    }
    let mut telemetry = TelemetryHistory::default();
    for sample in 0_u64..10 {
        telemetry.push(
            &StatsSnapshot {
                mem_gib: Some((4.0 + sample as f32 / 10.0, 8.0)),
                swap_gib: Some((0.5 + sample as f32 / 10.0, 2.0)),
                ..Default::default()
            },
            (sample + 1) * 1000,
        );
    }
    model.absorb_history(&telemetry, Window::EVERYTHING, ScaleMode::Window, 10_000);
    model.swap_total = Some(2 * 1024 * 1024 * 1024);
    model.swap_free = Some(1024 * 1024 * 1024);
    let view = MemoryView(&model);
    let area = Rect::full(240, 60);
    let board = Board::default();
    let placed = board.placed(&view, area);
    let mut surface = Surface::new(area.cols, area.rows);
    board.paint(&view, &mut surface, area, 0, &[]);
    let lines: Vec<Vec<char>> = text(&surface)
        .lines()
        .map(|line| line.chars().collect())
        .collect();

    for (panel_id, label) in [
        ("memory.composition", "used / available history"),
        ("memory.pressure", "PSI history"),
        ("memory.swap", "swap history"),
    ] {
        let panel = placed
            .iter()
            .find(|panel| panel.id.0 == panel_id)
            .expect("history panel is placed");
        let label_row = lines
            .iter()
            .position(|line| line.iter().collect::<String>().contains(label))
            .expect("history label is rendered");
        let mut chart = lines[label_row + 1].clone();
        chart.resize(area.cols, ' ');
        let left = panel.rect.x + 1;
        let right = panel.rect.x + panel.rect.cols - 2;
        assert!(
            chart[left..left + 3]
                .iter()
                .all(|ch| *ch == ' ' || *ch == '\u{2800}')
        );
        assert_ne!(
            chart[right], ' ',
            "{label} chart should reach the right edge"
        );
        assert_ne!(
            chart[right], '\u{2800}',
            "{label} chart should reach the right edge"
        );
    }
}
