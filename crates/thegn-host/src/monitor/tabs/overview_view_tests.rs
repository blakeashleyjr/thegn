use super::{OverviewModel, OverviewView};
use crate::telemetry::{ScaleMode, TelemetryHistory, Window};
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

fn sparse_history() -> TelemetryHistory {
    let mut history = TelemetryHistory::default();
    for second in 40..60_u64 {
        let missing_interior_sample = (50..=51).contains(&second);
        let snap = StatsSnapshot {
            cpu_pct: (!missing_interior_sample).then_some((40 + second % 20) as u8),
            load_avg: (!missing_interior_sample).then_some((second as f32 / 10.0, 1.0, 0.5)),
            mem_gib: (!missing_interior_sample).then_some((4.0, 8.0)),
            net_bps: (!missing_interior_sample).then_some((second * 1024, 512)),
            disk_io: (!missing_interior_sample).then_some((second * 2048, second * 1024)),
            ..Default::default()
        };
        history.push(&snap, second * 1000);
    }
    history
}

fn graph_column_has_ink(surface: &Surface, x: usize) -> bool {
    surface.screen_lines().iter().any(|line| {
        line.visible_cells().any(|cell| {
            cell.cell_index() == x
                && !cell.str().is_empty()
                && cell.str() != " "
                && !cell.str().chars().all(|glyph| glyph as u32 == 0x2800)
        })
    })
}

fn graph_column_is_cell_blank(surface: &Surface, x: usize) -> bool {
    surface.screen_lines().iter().all(|line| {
        line.visible_cells()
            .any(|cell| cell.cell_index() == x && cell.str() == " ")
    })
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

#[test]
fn overview_history_skips_leading_gaps_holds_interior_gaps_and_reuses_capacity() {
    let history = sparse_history();
    let mut model = OverviewModel::default();
    model.absorb_history(&history, Window::from_secs(60), ScaleMode::Window, 60_000);

    // With 160 buckets over 60 s, the first sample at 40 s lands at
    // floor(40 * 160 / 60) = 106, leaving 54 buckets in each populated slot.
    for (panel, slots) in model.sparks.iter().enumerate().take(5) {
        let metric_count = if panel == 4 { 2 } else { 1 };
        for series in slots.iter().take(metric_count) {
            assert_eq!(series.len(), 54, "panel {panel} {series:?}");
            assert!(series.capacity() >= 160, "panel {panel} {series:?}");
        }
    }
    assert!(model.sparks[5][0].is_empty());
    assert!(model.sparks[6][0].is_empty());
    assert!(model.sparks[7].iter().all(|series| series.is_empty()));

    // The samples at 50 s and 51 s are absent; buckets 133 through 137
    // retain the 49 s reading for CPU, Memory, Network, and Disk.
    for (panel, series) in [(0, 0), (1, 0), (2, 0), (3, 0), (4, 0), (4, 1)] {
        let values = &model.sparks[panel][series];
        assert!(
            values[26].is_finite() && values[26] > 0.0,
            "panel {panel} series {series}: held reference must be a real 49 s reading"
        );
        assert!(
            values[27..32].iter().all(|value| *value == values[26]),
            "panel {panel} series {series}: must hold the 49 s reading across interior gaps"
        );
    }
    for (panel, series) in [(3, 0), (4, 0), (4, 1)] {
        assert!(
            model.sparks[panel][series][27..32]
                .iter()
                .all(|value| *value > 0.0),
            "panel {panel} series {series}: zero-gap metric must not dip during interior gaps"
        );
    }

    let capacities = model
        .sparks
        .iter()
        .map(|slots| [slots[0].capacity(), slots[1].capacity()])
        .collect::<Vec<_>>();
    for _ in 0..1000 {
        model.absorb_history(&history, Window::from_secs(60), ScaleMode::Window, 60_000);
    }
    assert_eq!(
        model
            .sparks
            .iter()
            .map(|slots| [slots[0].capacity(), slots[1].capacity()])
            .collect::<Vec<_>>(),
        capacities
    );

    let mut surface = Surface::new(120, 8);
    test_override::with_unicode(UnicodeLevel::Full, || {
        crate::monitor::graphs::paint_history_graph(
            &model.sparks[0][0],
            thegn_core::theme::RampId::Util,
            Rect::full(120, 8),
            &mut surface,
        );
    });
    assert!(
        (0..93).all(|x| graph_column_is_cell_blank(&surface, x)),
        "the 54 samples, right-aligned in 240 dots, leave the graph's first 93 columns blank"
    );
    assert!(
        graph_column_has_ink(&surface, 93),
        "the first Overview CPU sample must begin in the column after the 106 leading buckets"
    );
    assert!(
        graph_column_has_ink(&surface, 119),
        "the newest Overview CPU sample must reach the graph's last inner column"
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
                        let ratio = fill_ratio(&surface, Rect::full(cols, rows));
                        assert!(ratio >= 0.55, "fill ratio {ratio:.3}");
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

#[test]
fn fresh_overview_does_not_place_or_demand_power_before_discovery() {
    let model = OverviewModel::default();
    let (_, placed, demand) = paint(&model, 240, 60);

    assert!(
        !placed
            .iter()
            .any(|panel| panel.id == PanelId("overview.power"))
    );
    assert!(!demand.contains(CollectorId::PowerSupply));
    assert!(demand.contains(CollectorId::CpuStat));
}

#[test]
fn overview_power_panel_appears_once_discovery_finds_a_supply() {
    // Positive control: this already passes on main after a supply is discovered.
    let mut model = fixture();
    let mut health =
        [thegn_metrics::collect::health::Health::Warming; thegn_metrics::collect::COLLECTORS];
    health[CollectorId::PowerSupply.index()] = thegn_metrics::collect::health::Health::Ok;
    model.absorb_frame(&MonitorFrame {
        health,
        ..Default::default()
    });

    let (_, placed, demand) = paint(&model, 240, 60);
    assert!(
        placed
            .iter()
            .any(|panel| panel.id == PanelId("overview.power"))
    );
    assert!(demand.contains(CollectorId::PowerSupply));
}

#[test]
fn overview_power_panel_stays_placed_while_its_collector_is_failing() {
    // Positive control: this already passes on main for a failing collector.
    let mut model = fixture();
    let mut health =
        [thegn_metrics::collect::health::Health::Warming; thegn_metrics::collect::COLLECTORS];
    health[CollectorId::PowerSupply.index()] = thegn_metrics::collect::health::Health::Failing {
        since_tick: 1,
        backoff: 1,
        retry_at: 2,
    };
    model.absorb_frame(&MonitorFrame {
        health,
        ..Default::default()
    });

    let (_, placed, demand) = paint(&model, 240, 60);
    assert!(
        placed
            .iter()
            .any(|panel| panel.id == PanelId("overview.power"))
    );
    assert!(demand.contains(CollectorId::PowerSupply));
}
