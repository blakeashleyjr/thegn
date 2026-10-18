use super::{DiskModel, DiskView};
use crate::{
    caps::test_override,
    compositor::Rect,
    monitor::{
        board::{Board, TabView},
        layout::PanelId,
    },
};
use std::sync::Arc;
use termwiz::surface::Surface;
use thegn_core::termcaps::{ColorDepth, UnicodeLevel};
use thegn_metrics::collect::{
    CollectorId, CollectorSet,
    frame::MonitorFrame,
    health::{Absent, Health},
    instances::InstanceId,
    model::{
        diskstats::{DevKind, DiskDevice},
        filesystems::{FilesystemKind, FilesystemRow},
    },
};

fn frame_fixture() -> MonitorFrame {
    let mut frame = MonitorFrame::default();
    frame.health[CollectorId::Diskstats.index()] = Health::Ok;
    frame.health[CollectorId::Filesystems.index()] = Health::Ok;
    frame.health[CollectorId::Psi.index()] = Health::Ok;
    frame.health[CollectorId::CgroupSlice.index()] = Health::Ok;
    frame.parts.diskstats.aggregate.read_bps = Some(48_000_000.0);
    frame.parts.diskstats.aggregate.write_bps = Some(9_000_000.0);
    frame.parts.diskstats.devices = (0..8)
        .map(|index| DiskDevice {
            id: InstanceId(index),
            name: Arc::from(format!("nvme{index}n1")),
            label: None,
            kind: DevKind::Disk,
            physical: true,
            parent: None,
            read_bps: Some(6_000_000.0),
            write_bps: Some(1_125_000.0),
            read_iops: Some(150.0),
            write_iops: Some(48.0),
            util_pct: Some(61.0),
            in_flight: 2,
        })
        .collect();
    frame.parts.filesystems.rows = (0..8)
        .map(|index| FilesystemRow {
            mount_point: format!("/data{index}"),
            fstype: "btrfs".into(),
            source: format!("nvme{index}n1"),
            superblock_dev: format!("259:{index}"),
            kind: FilesystemKind::Local,
            total_bytes: Some(1_000_000_000_000),
            free_bytes: Some(240_000_000_000),
            avail_bytes: Some(220_000_000_000),
            total_inodes: None,
            free_inodes: None,
            avail_inodes: None,
            btrfs_unallocated: Some(0),
            read_only: false,
        })
        .collect();
    frame.parts.psi.io.present = true;
    frame.parts.psi.io.file.some = Some(thegn_metrics::collect::parse::psi::PsiLine {
        avg10: 4800,
        avg60: 3900,
        avg300: 3000,
        total_us: 50_000,
    });
    frame.parts.psi.io.file.full = Some(thegn_metrics::collect::parse::psi::PsiLine {
        avg10: 4000,
        avg60: 3500,
        avg300: 2800,
        total_us: 40_000,
    });
    frame.parts.cgroup_slice.found = true;
    frame.parts.cgroup_slice.sample.io = Some(thegn_metrics::collect::model::cgroup::IoTotals {
        rbytes: 900_000_000,
        wbytes: 80_000_000,
        rios: 1200,
        wios: 240,
    });
    frame
}

fn render(
    model: &DiskModel,
    cols: usize,
    rows: usize,
) -> (Surface, Vec<crate::monitor::layout::Placed>, CollectorSet) {
    let view = DiskView(model);
    let board = Board::default();
    let rect = Rect::full(cols, rows);
    let placed = board.placed(&view, rect);
    let demand = board.demand(&view, rect, 1000).collectors;
    let mut surface = Surface::new(cols, rows);
    board.paint(&view, &mut surface, rect, 0, &[]);
    (surface, placed, demand)
}

#[test]
fn absorbs_disk_frame_and_reuses_warm_buffers() {
    let frame = frame_fixture();
    let mut model = DiskModel::default();
    model.absorb_frame(&frame);
    assert_eq!(model.devices.len(), 8);
    assert_eq!(model.filesystems.len(), 8);
    assert_eq!(model.read_bps, Some(48_000_000.0));
    let capacity = model.capacities();
    for _ in 0..1000 {
        model.absorb_frame(&frame);
    }
    assert_eq!(model.capacities(), capacity);
    assert_eq!(model.read_history.len(), 240);
}

#[test]
fn snapshots_cover_target_sizes_capabilities_and_static_panel_demand() {
    let mut model = DiskModel::default();
    model.absorb_frame(&frame_fixture());
    for (cols, rows) in [(100, 30), (160, 45), (240, 60)] {
        for (unicode, depth) in [
            (UnicodeLevel::Full, ColorDepth::Truecolor),
            (UnicodeLevel::Full, ColorDepth::Ansi16),
            (UnicodeLevel::Ascii, ColorDepth::None),
        ] {
            test_override::with_color(depth, || {
                test_override::with_unicode(unicode, || {
                    let (surface, placed, demand) = render(&model, cols, rows);
                    let text = surface.screen_chars_to_string();
                    for label in [
                        "I/O",
                        "I/O pressure",
                        "Devices",
                        "Filesystems",
                        "thegn.slice I/O",
                    ] {
                        assert!(
                            text.contains(label),
                            "missing {label} at {cols}x{rows}: {text}"
                        );
                    }
                    let view = DiskView(&model);
                    let union = placed.iter().fold(CollectorSet::empty(), |mut all, panel| {
                        for id in view.demand(panel.id).iter() {
                            all.insert(id);
                        }
                        all
                    });
                    assert_eq!(demand, union);
                    if (cols, rows) == (240, 60) {
                        // V-DSK-1b adds the device tree, btrfs dossier, and per-row histories.
                        assert!(fill_ratio(&surface, Rect::full(cols, rows)) >= 0.15);
                    }
                })
            });
        }
    }
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

#[test]
fn absent_collectors_render_absent_marks_and_do_not_change_demand() {
    let mut frame = frame_fixture();
    for collector in [
        CollectorId::Diskstats,
        CollectorId::Filesystems,
        CollectorId::Psi,
        CollectorId::CgroupSlice,
    ] {
        frame.health[collector.index()] = Health::Absent(Absent::NotOnThisOs);
    }
    let mut model = DiskModel::default();
    model.absorb_frame(&frame);
    let view = DiskView(&model);
    let mut present = DiskModel::default();
    present.absorb_frame(&frame_fixture());
    let present_view = DiskView(&present);
    for id in [
        PanelId("disk.io"),
        PanelId("disk.pressure"),
        PanelId("disk.devices"),
        PanelId("disk.filesystems"),
        PanelId("disk.slice"),
    ] {
        assert!(!view.demand(id).is_empty());
        assert_eq!(view.demand(id), present_view.demand(id));
    }
    let (surface, _, _) = render(&model, 160, 45);
    let text = surface.screen_chars_to_string();
    assert!(text.contains(crate::caps::active_glyphs().emdash));
    assert!(!text.contains(crate::caps::active_glyphs().arrow_up));
}
