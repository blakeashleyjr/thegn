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
    let view = ContainersView::new(&model, &inventory);
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
                    }
                    if cols == 240 {
                        let fill = fill_ratio(&surface, area);
                        // density follow-up (batch 52): estimated, primary sets from the printed value
                        assert!(
                            fill >= 0.30,
                            "fill ratio {fill:.3} at {cols}x{rows} {unicode:?}/{depth:?}"
                        );
                    }
                })
            });
        }
    }
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
    let old_capacity = model.slots[old_slot].cpu_history.values.len();
    model.absorb_frame(&frame(121, &["old-c", "new-d", "old-a"]));
    let capacities = (
        model.slots.capacity(),
        model.order.capacity(),
        model
            .slots
            .iter()
            .map(|slot| slot.cpu_history.values.len())
            .collect::<Vec<_>>(),
    );
    for tick in 0..1_000 {
        let ids = if tick.is_multiple_of(2) {
            ["new-d", "old-a", "old-c"]
        } else {
            ["old-c", "new-d", "old-a"]
        };
        model.absorb_frame(&frame(tick + 122, &ids));
        assert_eq!(
            (
                model.slots.capacity(),
                model.order.capacity(),
                model
                    .slots
                    .iter()
                    .map(|slot| slot.cpu_history.values.len())
                    .collect::<Vec<_>>()
            ),
            capacities
        );
    }
    let slot = model
        .slots
        .iter()
        .find(|slot| slot.id.as_deref() == Some("old-a"))
        .unwrap();
    let mut ordered = [f32::NAN; super::HISTORY_CAP];
    let len = slot.cpu_history.ordered(&mut ordered);
    assert_eq!(len, super::HISTORY_CAP);
    assert_ne!(
        ordered[len - 1],
        ordered[len - 2],
        "newest sample must reach the final ring position"
    );
    assert_eq!(old_capacity, super::HISTORY_CAP);

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
fn absent_health_and_unknown_container_metrics_keep_absent_marks() {
    let mut model = ContainersModel::default();
    let mut absent = MonitorFrame::default();
    absent.health[CollectorId::ContainersStats.index()] =
        Health::Absent(thegn_metrics::collect::health::Absent::NoHardware);
    model.absorb_frame(&absent);
    let view = ContainersView::new(&model, &[]);
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
    let view = ContainersView::new(&model, &[]);
    let mut surface = Surface::new(100, 30);
    Board::default().paint(&view, &mut surface, Rect::full(100, 30), 0, &[]);
    let output = text(&surface);
    assert!(output.contains("scope-unknow"));
    assert!(output.contains(crate::caps::active_glyphs().emdash));
}
