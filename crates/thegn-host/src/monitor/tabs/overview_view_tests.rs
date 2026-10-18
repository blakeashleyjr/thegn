use super::{OverviewModel, OverviewView};
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
    collect::{CollectorId, CollectorSet},
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
    model.absorb_history(&history, 60_000);
    model
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
        for depth in [ColorDepth::Truecolor, ColorDepth::Ansi16, ColorDepth::None] {
            test_override::with_color(depth, || {
                test_override::with_unicode(UnicodeLevel::Ascii, || {
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
fn overview_power_panel_appears_with_a_battery_reading() {
    let mut model = fixture();
    model.stats.battery = Some((73, false));
    let (_, placed, demand) = paint(&model, 240, 60);
    assert!(
        placed
            .iter()
            .any(|panel| panel.id == PanelId("overview.power"))
    );
    assert!(!demand.contains(CollectorId::PowerSupply));
}
