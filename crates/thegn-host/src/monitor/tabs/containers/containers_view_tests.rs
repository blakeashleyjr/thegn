use super::{ContainersModel, ContainersView};
use crate::{
    caps,
    compositor::Rect,
    monitor::board::{Board, TabView as _},
};
use std::sync::Arc;
use termwiz::surface::Surface;
use thegn_core::{
    sandbox::ContainerInfo,
    termcaps::{ColorDepth, UnicodeLevel},
};
use thegn_metrics::collect::{
    CollectorId, CollectorSet, frame::MonitorFrame, health::Health,
    model::containers_stats::ContainerStatsRow,
};

fn row(id: &str, tick: usize) -> ContainerStatsRow {
    ContainerStatsRow {
        id: Arc::from(id),
        cpu_pct: Some(((tick * 7) % 87) as f64 + 1.0),
        mem_current: Some(20_000_000 + (tick * 1_300_007) as u64),
        mem_peak: Some(80_000_000),
        pids: Some(2 + (tick % 17) as u64),
        io_read_bps: Some(3_000.0 + (tick * 97) as f64),
        io_write_bps: Some(1_000.0 + (tick * 53) as f64),
        gap: false,
    }
}

fn frame(tick: usize, ids: &[&str]) -> MonitorFrame {
    let mut frame = MonitorFrame::default();
    frame.sampled.insert(CollectorId::ContainersStats);
    frame.health[CollectorId::ContainersStats.index()] = Health::Ok;
    frame.parts.containers_stats.scopes = ids.len() as u32;
    frame.parts.containers_stats.rows = ids.iter().map(|id| row(id, tick)).collect();
    frame
}

fn inventory() -> Vec<ContainerInfo> {
    vec![
        ContainerInfo {
            name: "owned-one".into(),
            image: String::new(),
            status: "Up 1 minute".into(),
            ours: true,
            backend: "podman".into(),
            cpu: String::new(),
            mem: String::new(),
            net: String::new(),
            containment: String::new(),
            mounts: String::new(),
        },
        ContainerInfo {
            name: "foreign-one".into(),
            image: String::new(),
            status: "Exited".into(),
            ours: false,
            backend: "podman".into(),
            cpu: String::new(),
            mem: String::new(),
            net: String::new(),
            containment: String::new(),
            mounts: String::new(),
        },
    ]
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
    let total = x1.saturating_sub(x0).saturating_mul(y1.saturating_sub(y0));
    if total == 0 {
        return 0.0;
    }
    let used: usize = surface
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
        .map(|cell| cell.width())
        .sum();
    used as f32 / total as f32
}

#[test]
fn snapshots_cover_sizes_capabilities_and_static_demand() {
    let mut model = ContainersModel::default();
    let ids = [
        "scope-a123456789",
        "scope-b123456789",
        "scope-c123456789",
        "scope-d123456789",
        "scope-e123456789",
        "scope-f123456789",
    ];
    for tick in 0..120 {
        let mut current = ids.to_vec();
        current.rotate_left(tick % ids.len());
        model.absorb_frame(&frame(tick, &current));
    }
    let inventory = inventory();
    let view = ContainersView::new(&model, &inventory, None, 0);
    let mut ratios = Vec::new();
    for (cols, rows) in [(100, 30), (160, 45), (240, 60)] {
        for (unicode, depth) in [
            (UnicodeLevel::Full, ColorDepth::Truecolor),
            (UnicodeLevel::Full, ColorDepth::Ansi16),
            (UnicodeLevel::Ascii, ColorDepth::None),
        ] {
            let mut surface = Surface::new(cols, rows);
            caps::test_override::with_unicode(unicode, || {
                caps::test_override::with_color(depth, || {
                    let area = Rect::full(cols, rows);
                    let board = Board::default();
                    let placed = board.placed(&view, area);
                    board.paint(&view, &mut surface, area, 0, &[]);
                    let output = text(&surface);
                    assert!(output.contains("Estate"));
                    assert!(output.contains("Containers"));
                    assert!(output.contains("scope-a123456789"[..12].trim_end()));
                    let expected = placed.iter().fold(CollectorSet::empty(), |mut set, panel| {
                        for collector in view.demand(panel.id).iter() {
                            set.insert(collector);
                        }
                        set
                    });
                    assert_eq!(board.demand(&view, area, 1000).collectors, expected);
                    assert_eq!(
                        expected.iter().collect::<Vec<_>>(),
                        vec![CollectorId::ContainersStats]
                    );
                    if unicode == UnicodeLevel::Ascii {
                        assert!(output.is_ascii());
                        assert!(
                            output.contains("containers - 1 owned - 1 foreign"),
                            "ASCII inventory title missing: {output}"
                        );
                    }
                    ratios.push((cols, rows, unicode, depth, fill_ratio(&surface, area)));
                })
            });
        }
    }
    assert!(
        ratios
            .iter()
            .filter(|(cols, _, _, _, _)| *cols == 240)
            .all(|(_, _, _, _, ratio)| *ratio >= 0.28),
        "fill ratios by size/colour/unicode: {ratios:?}"
    );
}

#[test]
fn warm_storage_is_bounded_and_membership_reuses_slots_without_history_leaks() {
    let mut model = ContainersModel::default();
    for tick in 0..120 {
        model.absorb_frame(&frame(tick, &["old-a", "old-b", "old-c"]));
    }
    let old_slot = model
        .slots
        .iter()
        .position(|slot| slot.id.as_deref() == Some("old-a"))
        .unwrap();
    assert_eq!(model.slots[old_slot].cpu_history.len, super::HISTORY_CAP);
    model.absorb_frame(&frame(121, &["old-c", "new-d", "old-a"]));
    let capacities = (model.slots.capacity(), model.order.capacity());
    for tick in 0_usize..1_000 {
        let ids = match (tick / 7) % 3 {
            0 => ["new-d", "old-a", "old-c"],
            1 => ["old-c", "new-d", "old-a"],
            _ => ["old-a", "new-e", "old-c"],
        };
        model.absorb_frame(&frame(tick + 122, &ids));
        assert_eq!((model.slots.capacity(), model.order.capacity()), capacities);
    }
    let slot = model
        .slots
        .iter()
        .find(|slot| slot.id.as_deref() == Some("old-a"))
        .unwrap();
    let mut ordered = [f32::NAN; super::HISTORY_CAP];
    let len = slot.cpu_history.ordered(&mut ordered);
    assert_eq!(len, super::HISTORY_CAP);
    assert_eq!(
        ordered[len - 1],
        row("old-a", 1_121).cpu_pct.unwrap() as f32
    );

    model.absorb_frame(&frame(2_000, &["old-c", "new-d"]));
    model.absorb_frame(&frame(2_001, &["old-c", "new-d", "old-a"]));
    let reappeared = model
        .slots
        .iter()
        .find(|slot| slot.id.as_deref() == Some("old-a"))
        .unwrap();
    assert_eq!(
        reappeared.cpu_history.len, 1,
        "a vanished id must not inherit a replaced slot's history"
    );
}

#[test]
fn totals_ignore_missing_metric_rows_and_table_pids_keep_their_column() {
    let mut mixed = frame(1, &["without-io", "with-io"]);
    mixed.parts.containers_stats.rows[0].io_read_bps = None;
    mixed.parts.containers_stats.rows[0].io_write_bps = None;
    let mut model = ContainersModel::default();
    model.absorb_frame(&mixed);
    let mut ordered = [f32::NAN; super::HISTORY_CAP];
    assert_eq!(model.read_total.ordered(&mut ordered), 1);
    assert!(ordered[0].is_finite());
    assert_eq!(
        ordered[0],
        mixed.parts.containers_stats.rows[1].io_read_bps.unwrap() as f32
    );

    let mut stats = frame(0, &["scope-pids-123456"]);
    stats.parts.containers_stats.rows[0].pids = Some(7);
    let mut model = ContainersModel::default();
    for tick in 0..120 {
        stats.parts.containers_stats.rows[0].cpu_pct = Some(tick as f64 + 1.0);
        model.absorb_frame(&stats);
    }
    let inventory = inventory();
    let view = ContainersView::new(&model, &inventory, None, 0);
    let mut surface = Surface::new(240, 60);
    let board = Board::default();
    let area = Rect::full(240, 60);
    let placed = board.placed(&view, area);
    board.paint(&view, &mut surface, area, 0, &[]);
    let table = placed
        .iter()
        .find(|panel| panel.id == super::TABLE)
        .unwrap();
    let row_y = table.rect.y + 2;
    let line = text(&surface).lines().nth(row_y).unwrap().to_owned();
    let pids_x = table.rect.x + 1 + super::TABLE_PIDS;
    assert!(
        line.chars()
            .skip(pids_x)
            .take(5)
            .collect::<String>()
            .contains('7'),
        "pids column must retain 7 in `{line}`"
    );
}

#[test]
fn history_right_edge_tracks_the_newest_sample_and_holds_an_interior_gap() {
    let ids = ["scope-cpu-123456"];
    let mut model = ContainersModel::default();
    for tick in 0..120 {
        model.absorb_frame(&frame(tick, &ids));
    }
    let inventory = inventory();
    let view = ContainersView::new(&model, &inventory, None, 0);
    let board = Board::default();
    let area = Rect::full(240, 60);
    let placed = board.placed(&view, area);
    let cpu = placed.iter().find(|panel| panel.id == super::CPU).unwrap();
    let sample_x = cpu.rect.x + cpu.rect.cols - 2;
    let graph_y = cpu.rect.y + cpu.rect.rows - 2;
    let mut surface = Surface::new(240, 60);
    caps::test_override::with_unicode(UnicodeLevel::Full, || {
        board.paint(&view, &mut surface, area, 0, &[])
    });
    let last_cell = |surface: &Surface| {
        text(surface)
            .lines()
            .nth(graph_y)
            .unwrap()
            .chars()
            .nth(sample_x)
            .unwrap()
    };
    let painted = last_cell(&surface);
    assert!(
        painted != ' ' && painted != '\u{2800}',
        "newest graph cell was {painted:?}"
    );

    let mut gap = frame(121, &ids);
    gap.parts.containers_stats.rows[0].gap = true;
    model.absorb_frame(&gap);
    let mut ordered = [f32::NAN; super::HISTORY_CAP];
    let len = model.cpu_total.ordered(&mut ordered);
    assert_eq!(ordered[len - 1], ordered[len - 2]);
    let gap_view = ContainersView::new(&model, &inventory, None, 0);
    let mut gap_surface = Surface::new(240, 60);
    caps::test_override::with_unicode(UnicodeLevel::Full, || {
        board.paint(&gap_view, &mut gap_surface, area, 0, &[])
    });
    let held = last_cell(&gap_surface);
    assert!(
        held != ' ' && held != '\u{2800}',
        "held gap cell was {held:?}"
    );
}

#[test]
fn leading_gap_does_not_create_a_history_sample() {
    let mut model = ContainersModel::default();
    let mut gap = frame(0, &["scope-leading-gap"]);
    gap.parts.containers_stats.rows[0].gap = true;
    model.absorb_frame(&gap);

    let slot = model
        .slots
        .iter()
        .find(|slot| slot.id.as_deref() == Some("scope-leading-gap"))
        .unwrap();
    assert_eq!(slot.cpu_history.len, 0);
    assert_eq!(model.cpu_total.len, 0);
}

#[test]
fn unrepresentable_float_metrics_stay_nan_and_nan_painters_stay_blank() {
    let mut invalid = frame(0, &["scope-invalid-floats"]);
    let reading = &mut invalid.parts.containers_stats.rows[0];
    reading.cpu_pct = Some(f64::MAX);
    reading.io_read_bps = Some(f64::MAX);
    reading.io_write_bps = Some(-1.0);

    let mut model = ContainersModel::default();
    model.absorb_frame(&invalid);
    let slot = model
        .slots
        .iter()
        .find(|slot| slot.id.as_deref() == Some("scope-invalid-floats"))
        .unwrap();
    assert!(slot.cpu.is_none(), "unrepresentable CPU must be unknown");
    assert!(slot.read.is_none(), "unrepresentable I/O must be unknown");
    let mut ordered = [f32::NAN; super::HISTORY_CAP];
    assert_eq!(slot.cpu_history.ordered(&mut ordered), 1);
    assert!(ordered[0].is_nan(), "CPU overflow must remain NaN");
    assert_eq!(slot.read_history.ordered(&mut ordered), 1);
    assert!(ordered[0].is_nan(), "I/O overflow must remain NaN");
    assert_eq!(slot.write_history.ordered(&mut ordered), 1);
    assert!(ordered[0].is_nan(), "negative I/O must remain NaN");
    assert_eq!(model.cpu_total.ordered(&mut ordered), 1);
    assert!(ordered[0].is_nan(), "total overflow must remain NaN");
    assert_eq!(model.read_total.ordered(&mut ordered), 1);
    assert!(ordered[0].is_nan(), "I/O total overflow must remain NaN");

    let mut surface = Surface::new(8, 4);
    let area = Rect::full(8, 4);
    super::paint_finite_history_runs(
        &[f32::NAN],
        thegn_core::theme::RampId::Util,
        area,
        true,
        &mut surface,
    );
    super::paint_finite_spark_runs(
        &[f32::NAN],
        thegn_core::theme::RampId::Util,
        area,
        &mut surface,
    );
    assert!(
        text(&surface)
            .lines()
            .all(|line| line.chars().all(|ch| ch == ' ')),
        "NaN history cells must remain blank"
    );
}

#[test]
fn absent_health_and_unknown_container_metrics_keep_absent_marks() {
    let mut model = ContainersModel::default();
    let mut absent = MonitorFrame::default();
    absent.sampled.insert(CollectorId::ContainersStats);
    absent.health[CollectorId::ContainersStats.index()] =
        Health::Absent(thegn_metrics::collect::health::Absent::NoHardware);
    model.absorb_frame(&absent);
    let view = ContainersView::new(&model, &[], None, 0);
    let mut surface = Surface::new(100, 30);
    Board::default().paint(&view, &mut surface, Rect::full(100, 30), 0, &[]);
    let output = text(&surface);
    assert!(output.contains(crate::caps::active_glyphs().emdash));
    assert!(!output.contains("0.0%"));

    let mut unknown = frame(1, &["scope-unknown123"]);
    let reading = &mut unknown.parts.containers_stats.rows[0];
    reading.cpu_pct = None;
    reading.mem_current = None;
    reading.pids = None;
    reading.io_read_bps = None;
    reading.io_write_bps = None;
    let mut model = ContainersModel::default();
    model.absorb_frame(&unknown);
    let view = ContainersView::new(&model, &[], None, 0);
    let mut surface = Surface::new(100, 30);
    Board::default().paint(&view, &mut surface, Rect::full(100, 30), 0, &[]);
    let output = text(&surface);
    assert!(output.contains("scope-unknow"));
    assert!(output.contains(crate::caps::active_glyphs().emdash));
}

#[test]
fn unsampled_frame_updates_health_without_erasing_histories() {
    let mut model = ContainersModel::default();
    model.absorb_frame(&frame(0, &["scope-kept"]));
    let mut ordered = [f32::NAN; super::HISTORY_CAP];
    assert_eq!(model.cpu_total.ordered(&mut ordered), 1);
    let previous = ordered[0];

    let mut unsampled = MonitorFrame::default();
    unsampled.health[CollectorId::ContainersStats.index()] =
        Health::Absent(thegn_metrics::collect::health::Absent::NotImplemented);
    model.absorb_frame(&unsampled);

    assert_eq!(model.cpu_total.ordered(&mut ordered), 1);
    assert_eq!(ordered[0], previous);
    assert_eq!(
        model.slots.iter().filter(|slot| slot.id.is_some()).count(),
        1
    );
    assert!(matches!(model.health, Health::Absent(_)));
}

#[test]
fn io_panel_mirrors_read_above_and_write_below_midline() {
    let mut model = ContainersModel::default();
    for tick in 0..12 {
        let mut sample = frame(tick, &["scope-io"]);
        sample.parts.containers_stats.rows[0].io_read_bps = Some(900.0);
        sample.parts.containers_stats.rows[0].io_write_bps = Some(200.0);
        model.absorb_frame(&sample);
    }
    let inventory = inventory();
    let view = ContainersView::new(&model, &inventory, None, 0);
    let area = Rect::full(240, 60);
    let board = Board::default();
    let placed = board.placed(&view, area);
    let io = placed.iter().find(|panel| panel.id == super::IO).unwrap();
    let mut surface = Surface::new(240, 60);
    caps::test_override::with_unicode(UnicodeLevel::Ascii, || {
        board.paint(&view, &mut surface, area, 0, &[])
    });
    let output = text(&surface);
    assert!(
        output.contains("read | scale"),
        "read half label missing: {output}"
    );
    assert!(
        output.contains("write"),
        "write half label missing: {output}"
    );
    let graph_top = io.rect.y + 2;
    let graph_rows = io.rect.rows.saturating_sub(4);
    assert!(graph_rows >= 4);
    let lines: Vec<_> = output.lines().collect();
    let has_ink = |line: &str| {
        line.chars()
            .skip(io.rect.x + 1)
            .take(io.rect.cols.saturating_sub(2))
            .any(|ch| ch != ' ')
    };
    assert!(
        has_ink(lines[graph_top + graph_rows / 2 - 1]),
        "read series must rise above the axis: {output}"
    );
    assert!(
        has_ink(lines[graph_top + graph_rows / 2]),
        "write series must extend below the axis: {output}"
    );
}

#[test]
fn inventory_cursor_keeps_ownership_foreground_and_scrolls_selected_row_into_view() {
    use termwiz::cell::Intensity;

    let model = ContainersModel::default();
    let inventory = inventory();
    let view = ContainersView::new(&model, &inventory, None, 0);
    let area = Rect::full(240, 60);
    let board = Board::default();
    let placed = board.placed(&view, area);
    let panel = placed
        .iter()
        .find(|panel| panel.id == super::INVENTORY)
        .unwrap();
    let palette = crate::chrome::palette_snapshot();
    let selected_bg =
        crate::seg::Tok::Series(thegn_core::theme::SeriesRole::Selected).resolve(&palette);
    let ghost_fg = crate::seg::Tok::Slot(crate::chrome::S::Ghost).resolve(&palette);

    let mut before = Surface::new(240, 60);
    board.paint(&view, &mut before, area, 0, &[]);
    let owned_line = before.screen_lines()[panel.rect.y + 2].clone();
    let owned_cell = owned_line
        .visible_cells()
        .find(|cell| cell.str() == "o")
        .unwrap();
    assert_eq!(owned_cell.attrs().intensity(), Intensity::Bold);
    assert_eq!(owned_cell.attrs().background(), selected_bg);

    let view = ContainersView::new(&model, &inventory, None, 1);
    let mut after = Surface::new(240, 60);
    board.paint(&view, &mut after, area, 0, &[]);
    let owned_line = after.screen_lines()[panel.rect.y + 2].clone();
    let owned_cell = owned_line
        .visible_cells()
        .find(|cell| cell.str() == "o")
        .unwrap();
    assert_ne!(owned_cell.attrs().intensity(), Intensity::Bold);
    let foreign_line = after.screen_lines()[panel.rect.y + 3].clone();
    let foreign_cell = foreign_line
        .visible_cells()
        .find(|cell| cell.str() == "f")
        .unwrap();
    assert_eq!(foreign_cell.attrs().intensity(), Intensity::Bold);
    assert_eq!(foreign_cell.attrs().background(), selected_bg);
    assert_eq!(foreign_cell.attrs().foreground(), ghost_fg);
    let long_inventory: Vec<_> = (0..12)
        .map(|index| ContainerInfo {
            name: format!("container-row-{index}"),
            image: String::new(),
            status: "Up 1 minute".into(),
            ours: index.is_multiple_of(2),
            backend: "podman".into(),
            cpu: String::new(),
            mem: String::new(),
            net: String::new(),
            containment: String::new(),
            mounts: String::new(),
        })
        .collect();
    let last_name = long_inventory.last().unwrap().name.clone();
    let scrolled_view =
        ContainersView::new(&model, &long_inventory, None, long_inventory.len() - 1);
    let mut scrolled = Surface::new(100, 30);
    board.paint(&scrolled_view, &mut scrolled, Rect::full(100, 30), 0, &[]);
    assert!(
        text(&scrolled).contains(&last_name),
        "selected final inventory row was not scrolled into view: {}",
        text(&scrolled)
    );
}

#[test]
fn odd_index_history_run_keeps_its_braille_half_cell_alignment() {
    // The visible tail starts at original sample 1; the gap at original odd
    // sample 3 therefore leaves the following finite run at visible index 3.
    let values = [0.1, 0.2, 0.4, f32::NAN, 0.9];
    let area = Rect {
        x: 0,
        y: 0,
        cols: 2,
        rows: 4,
    };
    let mut actual = Surface::new(2, 4);
    let mut expected = Surface::new(2, 4);
    super::paint_finite_history_runs(
        &values,
        thegn_core::theme::RampId::Util,
        area,
        true,
        &mut actual,
    );
    crate::monitor::graphs::paint_history_graph(
        &values,
        thegn_core::theme::RampId::Util,
        area,
        &mut expected,
    );
    assert_eq!(
        text(&actual),
        text(&expected),
        "a finite run after the odd-index gap shifted half a braille cell"
    );
}
