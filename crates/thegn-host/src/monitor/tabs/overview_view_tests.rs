use super::{OverviewModel, OverviewView};
use crate::telemetry::{ScaleMode, Window};
use crate::{
    caps::test_override,
    compositor::Rect,
    monitor::{
        board::{Board, TabView},
        layout::PanelId,
    },
    sections::Section,
};
use termwiz::surface::Surface;
use thegn_core::termcaps::{ColorDepth, UnicodeLevel};
use thegn_metrics::{
    StatsSnapshot,
    collect::{
        CollectorId, CollectorSet,
        frame::{MonitorFrame, Parts},
        model::{
            cpu_stat::{CoreSample, CpuStatPart, ModeShare},
            diskstats::{DiskTotals, DiskstatsPart},
            netdev::{IfaceSample, NetdevPart},
            psi::{PsiPart, PsiResource},
            thermal_zone::{ThermalZone, ThermalZonePart},
        },
        parse::{
            meminfo,
            psi::{PsiFile, PsiLine},
        },
    },
};

fn fixture() -> OverviewModel {
    let mut model = OverviewModel {
        stats: StatsSnapshot {
            cpu_pct: Some(42),
            cpu_cores: vec![20, 70, 40, 90],
            load_avg: Some((1.0, 0.8, 0.5)),
            mem_gib: Some((8.0, 32.0)),
            net_bps: Some((4096, 2048)),
            disk_free_pct: Some(62),
            disk_io: Some((1024, 512)),
            cpu_temp_c: Some(54.0),
            ..Default::default()
        },
        ..Default::default()
    };
    let mut history = crate::telemetry::TelemetryHistory::default();
    for second in 0..=60_u64 {
        history.push(&model.stats, second * 1000);
    }
    model.absorb_history(&history, Window::from_secs(60), ScaleMode::Window, 60_000);
    let mut raw = meminfo::MeminfoRaw::default();
    meminfo::parse(
        b"MemTotal: 32768000 kB\nMemAvailable: 16384000 kB\n",
        &mut raw,
    );
    let mut health =
        [thegn_metrics::collect::health::Health::Warming; thegn_metrics::collect::COLLECTORS];
    health[CollectorId::PowerSupply.index()] = thegn_metrics::collect::health::Health::Absent(
        thegn_metrics::collect::health::Absent::NoHardware,
    );
    let frame = MonitorFrame {
        health,
        parts: Parts {
            cpu_stat: CpuStatPart {
                cores: vec![CoreSample {
                    id: 0,
                    online: true,
                    share: Some(ModeShare {
                        busy: 420,
                        ..Default::default()
                    }),
                }],
                procs_running: Some(3),
                ..Default::default()
            },
            meminfo: thegn_metrics::collect::model::meminfo::MeminfoPart {
                raw,
                ..Default::default()
            },
            psi: PsiPart {
                memory: PsiResource {
                    present: true,
                    file: PsiFile {
                        some: Some(PsiLine {
                            avg10: 25,
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                ..Default::default()
            },
            netdev: NetdevPart {
                ifaces: vec![IfaceSample::default()],
                ..Default::default()
            },
            diskstats: DiskstatsPart {
                aggregate: DiskTotals {
                    read_bps: Some(1024.0),
                    write_bps: Some(512.0),
                    ..Default::default()
                },
                ..Default::default()
            },
            thermal_zone: ThermalZonePart {
                zones: vec![ThermalZone {
                    key: "thermal_zone0".into(),
                    zone_type: "cpu".into(),
                    device: String::new(),
                    policy: None,
                    mode: None,
                    temp_c: Some(54.0),
                    trips: Vec::new(),
                    alias: false,
                    alarm_events: 0,
                }],
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    };
    model.absorb_frame(&frame);
    model
}

#[test]
fn empty_overview_demands_its_live_source_collectors() {
    let model = OverviewModel::default();
    let view = OverviewView(&model);
    let demand = Board::default().demand(&view, Rect::full(160, 45), 1000);
    assert!(demand.collectors.contains(CollectorId::CpuStat));
    assert!(demand.collectors.contains(CollectorId::Meminfo));
}

#[test]
fn cpu_core_shares_are_rendered_as_percentages() {
    let model = fixture();
    let text = OverviewView(&model).panel_content(0).1.join(" ");
    assert!(
        text.contains("42%"),
        "per-mille 420 must display as 42%: {text}"
    );
    assert!(
        !text.contains("420%"),
        "raw per-mille leaked into output: {text}"
    );
}

fn fill_ratio(surface: &Surface, area: Rect) -> f32 {
    let (cols, rows) = surface.dimensions();
    let mut occupied = 0usize;
    for (y, line) in surface.screen_lines().iter().enumerate() {
        if y < area.y || y >= area.y.saturating_add(area.rows) {
            continue;
        }
        for cell in line.visible_cells() {
            let x = cell.cell_index();
            if x >= area.x
                && x < area.x.saturating_add(area.cols)
                && !cell.str().is_empty()
                && cell.str() != " "
                // A blank braille cell (U+2800) is visually empty.
                && cell.str() != "\u{2800}"
            {
                occupied += cell.width();
            }
        }
    }
    let area_cells = cols
        .min(area.x.saturating_add(area.cols))
        .saturating_sub(area.x)
        .saturating_mul(
            rows.min(area.y.saturating_add(area.rows))
                .saturating_sub(area.y),
        );
    occupied as f32 / area_cells.max(1) as f32
}

fn paint(
    model: &OverviewModel,
    cols: usize,
    rows: usize,
) -> (Surface, Vec<crate::monitor::layout::Placed>, CollectorSet) {
    let view = OverviewView(model);
    let board = Board::default();
    let area = Rect::full(cols, rows);
    let placed = board.placed(&view, area);
    let demand = board.demand(&view, area, 1000).collectors;
    let mut surface = Surface::new(cols, rows);
    board.paint(&view, &mut surface, area, 0, &[] as &[Section]);
    (surface, placed, demand)
}

#[test]
fn overview_renders_all_target_sizes_and_terminal_levels() {
    let model = fixture();
    for (cols, rows) in [(100, 30), (160, 45), (240, 60)] {
        for (level, depth) in [
            (UnicodeLevel::Full, ColorDepth::Truecolor),
            (UnicodeLevel::Full, ColorDepth::Ansi16),
            (UnicodeLevel::Ascii, ColorDepth::None),
        ] {
            test_override::with_color(depth, || {
                test_override::with_unicode(level, || {
                    let (surface, placed, demand) = paint(&model, cols, rows);
                    let text = surface.screen_chars_to_string();
                    for label in ["CPU", "Memory", "Network", "Disk", "Thermal"] {
                        assert!(
                            text.contains(label),
                            "missing {label} at {cols}x{rows}: {text}"
                        );
                    }
                    assert!(
                        !placed
                            .iter()
                            .any(|panel| panel.id == PanelId("overview.power"))
                    );
                    let union = placed.iter().fold(CollectorSet::empty(), |mut all, panel| {
                        for id in OverviewView(&model).demand(panel.id).iter() {
                            all.insert(id);
                        }
                        all
                    });
                    assert_eq!(demand, union);
                    if (cols, rows) == (240, 60) {
                        assert!(fill_ratio(&surface, Rect::full(cols, rows)) >= 0.55);
                    }
                });
            });
        }
    }
}

#[test]
fn shrinking_the_body_drops_lower_priority_panels_before_cpu() {
    let model = fixture();
    let mut observed_drop = false;
    for rows in 5..30 {
        let view = OverviewView(&model);
        let solution = Board::default().solve(&view, Rect::full(100, rows));
        if !solution.dropped.is_empty() {
            observed_drop = true;
            assert!(
                solution
                    .placed
                    .iter()
                    .any(|panel| panel.id == PanelId("overview.cpu"))
            );
            assert!(!solution.dropped.contains(&PanelId("overview.cpu")));
        }
    }
    assert!(observed_drop, "fixture never reached a constrained layout");
}

#[test]
fn overview_power_panel_appears_with_a_battery_reading() {
    let mut model = fixture();
    model.stats.battery = Some((73, false));
    let (_, placed, demand) = paint(&model, 240, 60);
    assert!(
        placed
            .iter()
            .any(|panel| panel.id == PanelId("overview.power"))
    );
    assert!(demand.contains(CollectorId::PowerSupply));
}
