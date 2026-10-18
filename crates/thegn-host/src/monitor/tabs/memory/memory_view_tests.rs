use super::view::{MemoryModel, MemoryView};
use crate::{caps, compositor::Rect, monitor::board::Board};
use termwiz::surface::Surface;
use thegn_core::{
    mem_health::Verdict,
    termcaps::{ColorDepth, UnicodeLevel},
};
use thegn_metrics::collect::{
    CollectorId,
    frame::MonitorFrame,
    parse::{meminfo, psi, vmstat},
};

fn fixture(pressure: bool) -> MonitorFrame {
    let mut frame = MonitorFrame::default();
    meminfo::parse(
        b"MemTotal: 8388608 kB\nMemAvailable: 1048576 kB\nMemFree: 524288 kB\nBuffers: 262144 kB\nCached: 2097152 kB\nShmem: 131072 kB\nSlab: 131072 kB\nAnonPages: 4194304 kB\nActive(file): 1048576 kB\nInactive(file): 1048576 kB\nDirty: 1024 kB\nWriteback: 512 kB\nSwapTotal: 2097152 kB\nSwapFree: 1048576 kB\nZswap: 32768 kB\nHugePages_Total: 4\nHugePages_Free: 2\n",
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
    frame.parts.vmstat.rates[thegn_metrics::collect::model::vmstat::VmRate::ScanDirect as usize] =
        Some(20.0);
    frame.parts.vmstat.rates[thegn_metrics::collect::model::vmstat::VmRate::ScanKswapd as usize] =
        Some(30.0);
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
    let mut healthy = MemoryModel::default();
    healthy.absorb_frame(&fixture(false));
    assert_eq!(healthy.health.verdict, Verdict::Ok);
    let mut pressure = MemoryModel::default();
    pressure.absorb_frame(&fixture(true));
    assert_eq!(pressure.health.verdict, Verdict::Thrashing);
    assert!(
        pressure
            .health
            .reasons
            .contains(thegn_core::mem_health::Reason::PsiFull)
    );
}

#[test]
fn absent_swap_and_cgroup_are_hidden_and_not_demanded() {
    let mut model = MemoryModel::default();
    model.absorb_frame(&fixture(false));
    model.swap_total = Some(0);
    model.slice_present = false;
    let view = MemoryView(&model);
    let placed = Board::default().placed(&view, Rect::full(160, 45));
    assert!(!placed.iter().any(|panel| panel.id.0 == "memory.swap"));
    assert!(!placed.iter().any(|panel| panel.id.0 == "memory.limit"));
    let demand = Board::default()
        .demand(&view, Rect::full(160, 45), 1000)
        .collectors;
    assert!(!demand.contains(CollectorId::CgroupSlice));
    model.swap_total = Some(1);
    model.slice_present = true;
    let view = MemoryView(&model);
    let demand = Board::default()
        .demand(&view, Rect::full(160, 45), 1000)
        .collectors;
    assert!(demand.contains(CollectorId::Meminfo));
}

#[test]
fn board_snapshots_cover_size_and_capability_matrix() {
    let sizes = [(100, 30), (160, 45), (240, 60)];
    let levels = [UnicodeLevel::Full, UnicodeLevel::Basic, UnicodeLevel::Ascii];
    let depths = [ColorDepth::Truecolor, ColorDepth::Ansi16];
    for (cols, rows) in sizes {
        for level in levels {
            for depth in depths {
                let mut model = MemoryModel::default();
                model.absorb_frame(&fixture(true));
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
                    assert!(fill_ratio(&surface, Rect::full(cols, rows)) >= 0.55);
                }
                if level == UnicodeLevel::Ascii {
                    assert!(rendered.chars().all(|character| character.is_ascii()));
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
fn missing_values_render_the_capability_absent_mark() {
    let mut model = MemoryModel::default();
    model.absorb_frame(&MonitorFrame::default());
    let view = MemoryView(&model);
    let mut surface = Surface::new(100, 30);
    Board::default().paint(&view, &mut surface, Rect::full(100, 30), 0, &[]);
    assert!(text(&surface).contains(caps::active_glyphs().emdash));
    assert!(!text(&surface).contains("0 B"));
}
