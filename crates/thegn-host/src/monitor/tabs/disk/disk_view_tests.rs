use super::{DiskModel, DiskView};
use crate::{
    caps::test_override,
    compositor::Rect,
    monitor::{
        board::{Board, TabView},
        kit::grid::CellGrid,
        layout::PanelId,
    },
    telemetry::{ScaleMode, TelemetryHistory, Window},
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
    frame.parts.diskstats.physical = 8;
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
            read_bps: Some(3_000_000.0 + f64::from(index) * 1_500_000.0),
            write_bps: Some(350_000.0 + f64::from(index) * 220_000.0),
            read_iops: Some(150.0),
            write_iops: Some(48.0),
            util_pct: Some(25.0 + f64::from(index) * 7.0),
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
            total_bytes: Some(500_000_000_000 + index as u64 * 125_000_000_000),
            free_bytes: Some(40_000_000_000 + index as u64 * 19_000_000_000),
            avail_bytes: Some(35_000_000_000 + index as u64 * 17_000_000_000),
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
    let mut frame = frame_fixture();
    let mut model = DiskModel::default();
    model.absorb_frame(&frame);
    assert_eq!(model.devices.len(), 8);
    assert_eq!(model.filesystems.len(), 8);
    assert_eq!(model.read_bps, Some(48_000_000.0));
    assert_eq!(model.physical_count, 8);
    for row in &mut frame.parts.filesystems.rows {
        row.mount_point.push_str("/with/a/long/path/component");
        row.fstype.push_str("_filesystem_type");
        row.source.push_str("_physical_device_source");
        row.superblock_dev.push_str("_extended_superblock_id");
    }
    model.absorb_frame(&frame);
    let string_capacities = model
        .filesystems
        .iter()
        .map(|row| {
            (
                row.mount_point.capacity(),
                row.fstype.capacity(),
                row.source.capacity(),
                row.superblock_dev.capacity(),
            )
        })
        .collect::<Vec<_>>();
    for row in &mut frame.parts.filesystems.rows {
        row.mount_point.truncate(6);
        row.fstype.truncate(5);
        row.source.truncate(7);
        row.superblock_dev.truncate(5);
    }
    let capacity = model.capacities();
    for tick in 0..1000 {
        frame.parts.diskstats.aggregate.read_bps = Some(48_000_000.0 + f64::from(tick));
        frame.parts.diskstats.aggregate.write_bps = Some(9_000_000.0 + f64::from(tick));
        if let Some(io) = &mut frame.parts.cgroup_slice.sample.io {
            io.rbytes = 900_000_000 + u64::from(tick);
            io.wbytes = 80_000_000 + u64::from(tick) * 2;
        }
        for (index, device) in frame.parts.diskstats.devices.iter_mut().enumerate() {
            device.util_pct = Some(f64::from((tick + index as u32 * 7) % 100));
        }
        model.absorb_frame(&frame);
    }
    assert_eq!(model.capacities(), capacity);
    assert_eq!(
        model
            .filesystems
            .iter()
            .map(|row| (
                row.mount_point.capacity(),
                row.fstype.capacity(),
                row.source.capacity(),
                row.superblock_dev.capacity(),
            ))
            .collect::<Vec<_>>(),
        string_capacities
    );
    assert_eq!(model.read_history.len, 240);
    assert_eq!(model.write_history.len, 240);
    assert_eq!(model.psi_some_history.len, 240);
    assert_eq!(model.psi_full_history.len, 240);
    assert_eq!(model.device_history.len(), 8);
    assert!(
        model
            .device_history
            .iter()
            .all(|history| history.util.len == 240)
    );
    assert!(
        model
            .device_history
            .iter()
            .all(|history| { history.read.len == 240 && history.write.len == 240 })
    );
    let read_next = model.read_history.next;
    let write_next = model.write_history.next;
    frame.parts.diskstats.aggregate.read_bps = None;
    model.absorb_frame(&frame);
    assert_eq!(model.read_history.next, read_next);
    assert_ne!(model.write_history.next, write_next);
}

#[test]
fn snapshots_cover_target_sizes_capabilities_and_static_panel_demand() {
    let mut model = DiskModel::default();
    let mut frame = frame_fixture();
    for tick in 0_u16..240 {
        frame.parts.diskstats.aggregate.read_bps = Some(10_000_000.0 + f64::from(tick) * 100_000.0);
        frame.parts.diskstats.aggregate.write_bps =
            Some(2_000_000.0 + f64::from(239 - tick) * 25_000.0);
        if let Some(io) = &mut frame.parts.cgroup_slice.sample.io {
            io.rbytes = 900_000_000 + u64::from(tick) * 1_000_000;
            io.wbytes = 80_000_000 + u64::from(tick) * 250_000;
        }
        for (index, device) in frame.parts.diskstats.devices.iter_mut().enumerate() {
            device.read_bps = Some(
                1_000_000.0
                    + f64::from(index as u16) * 1_000_000.0
                    + f64::from(tick % 31) * 75_000.0,
            );
            device.write_bps = Some(
                250_000.0 + f64::from(index as u16) * 250_000.0 + f64::from(tick % 23) * 20_000.0,
            );
            device.util_pct = Some(f64::from((tick + index as u16 * 11) % 100));
        }
        if let Some(line) = &mut frame.parts.psi.io.file.some {
            line.avg10 = 1200 + u32::from((tick * 17) % 7600);
        }
        if let Some(line) = &mut frame.parts.psi.io.file.full {
            line.avg10 = 500 + u32::from((tick * 29) % 6500);
        }
        model.absorb_frame(&frame);
    }
    if let Some(line) = &mut frame.parts.psi.io.file.some {
        line.avg10 = 4800;
    }
    if let Some(line) = &mut frame.parts.psi.io.file.full {
        line.avg10 = 4000;
    }
    model.absorb_frame(&frame);
    for (cols, rows) in [(100, 30), (160, 45), (240, 60)] {
        for (unicode, depth) in [
            (UnicodeLevel::Full, ColorDepth::Truecolor),
            (UnicodeLevel::Full, ColorDepth::Ansi16),
            (UnicodeLevel::Ascii, ColorDepth::None),
        ] {
            test_override::with_color(depth, || {
                test_override::with_unicode(unicode, || {
                    let (surface, _placed, demand) = render(&model, cols, rows);
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
                    for golden in [
                        "some 48.00%",
                        "full 40.00%",
                        "nvme0n1",
                        "/data0 btrfs",
                        "totals read",
                    ] {
                        assert!(
                            text.contains(golden),
                            "missing golden {golden} at {cols}x{rows}: {text}"
                        );
                    }
                    if unicode == UnicodeLevel::Ascii {
                        assert!(text.is_ascii(), "non-ASCII output at {cols}x{rows}: {text}");
                    }
                    let mut expected = CollectorSet::empty();
                    for id in [
                        CollectorId::Diskstats,
                        CollectorId::Psi,
                        CollectorId::CgroupSlice,
                        CollectorId::Filesystems,
                    ] {
                        expected.insert(id);
                    }
                    assert_eq!(demand, expected);
                    if (cols, rows) == (240, 60) {
                        let ratio = fill_ratio(&surface, Rect::full(cols, rows));
                        match (unicode, depth) {
                            (UnicodeLevel::Full, ColorDepth::Truecolor) => {
                                // density follow-up (batch 49): measured 0.30
                                assert!(ratio >= 0.30, "fill ratio {ratio:.3}");
                            }
                            (UnicodeLevel::Full, ColorDepth::Ansi16) => {
                                // density follow-up (batch 49): measured 0.30
                                assert!(ratio >= 0.30, "fill ratio {ratio:.3}");
                            }
                            (UnicodeLevel::Ascii, ColorDepth::None) => {
                                // density follow-up (batch 49): measured 0.30
                                assert!(ratio >= 0.30, "fill ratio {ratio:.3}");
                            }
                            _ => unreachable!(),
                        }
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

#[test]
fn io_peak_scale_remains_visible_beside_the_history_graphs() {
    let mut model = DiskModel::default();
    let mut frame = frame_fixture();
    for tick in 0_u16..240 {
        frame.parts.diskstats.aggregate.read_bps = Some(10_000_000.0 + f64::from(tick) * 100_000.0);
        frame.parts.diskstats.aggregate.write_bps = Some(2_000_000.0 + f64::from(tick) * 25_000.0);
        model.absorb_frame(&frame);
    }
    for (cols, rows) in [(100, 30), (240, 60)] {
        let (surface, _, _) = render(&model, cols, rows);
        let text = surface.screen_chars_to_string();
        assert!(
            text.contains("peak"),
            "peak missing at {cols}x{rows}: {text}"
        );
    }
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

#[test]
fn diskstats_warming_and_no_permission_keep_their_reading_states() {
    let mut frame = frame_fixture();
    frame.health[CollectorId::Diskstats.index()] = Health::Warming;
    let mut model = DiskModel::default();
    model.absorb_frame(&frame);
    let (surface, placed, _) = render(&model, 160, 45);
    let text = surface.screen_chars_to_string();
    assert!(text.contains(crate::caps::active_glyphs().ellipsis));
    assert!(!text.contains("0 physical devices"));
    assert!(!text.contains("0B/s"));
    let devices = placed
        .iter()
        .find(|panel| panel.id == PanelId("disk.devices"))
        .expect("device panel placed");
    let device_content = text
        .lines()
        .nth(devices.rect.y + 1)
        .expect("device content row")
        .chars()
        .skip(devices.rect.x + 1)
        .take(devices.rect.cols.saturating_sub(2))
        .collect::<String>();
    assert!(device_content.contains(crate::caps::active_glyphs().ellipsis));

    frame.health[CollectorId::Diskstats.index()] = Health::Absent(Absent::NoPermission);
    model.absorb_frame(&frame);
    let (surface, _, _) = render(&model, 160, 45);
    assert!(
        surface
            .screen_chars_to_string()
            .contains(crate::caps::glyph(crate::caps::Glyph::Lock))
    );
}

#[test]
fn healthy_empty_pressure_and_missing_util_do_not_show_stale_loading_or_percent() {
    let mut frame = MonitorFrame::default();
    frame.health[CollectorId::Diskstats.index()] = Health::Ok;
    frame.health[CollectorId::Psi.index()] = Health::Ok;
    frame.parts.diskstats.devices = vec![DiskDevice {
        id: InstanceId(0),
        name: Arc::from("nvme0n1"),
        label: None,
        kind: DevKind::Disk,
        physical: true,
        parent: None,
        read_bps: None,
        write_bps: None,
        read_iops: None,
        write_iops: None,
        util_pct: None,
        in_flight: 0,
    }];
    let mut model = DiskModel::default();
    model.absorb_frame(&frame);
    let (surface, placed, _) = render(&model, 160, 45);
    let text = surface.screen_chars_to_string();
    let pressure = placed
        .iter()
        .find(|panel| panel.id == PanelId("disk.pressure"))
        .expect("pressure panel placed");
    let pressure_content = text
        .lines()
        .nth(pressure.rect.y + 1)
        .expect("pressure content row")
        .chars()
        .skip(pressure.rect.x + 1)
        .take(pressure.rect.cols.saturating_sub(2))
        .collect::<String>();
    assert!(pressure_content.contains(crate::caps::active_glyphs().emdash));
    assert!(!pressure_content.contains(crate::caps::active_glyphs().ellipsis));
    let device = placed
        .iter()
        .find(|panel| panel.id == PanelId("disk.devices"))
        .expect("device panel placed");
    let device_content = text
        .lines()
        .nth(device.rect.y + 1)
        .expect("device content row")
        .chars()
        .skip(device.rect.x + 1)
        .take(device.rect.cols.saturating_sub(2))
        .collect::<String>();
    assert!(device_content.contains(crate::caps::active_glyphs().emdash));
    assert!(!device_content.contains(&format!("{}%", crate::caps::active_glyphs().emdash)));
}

#[test]
fn per_device_histories_follow_identity_across_reordering_and_missing_sides() {
    let mut frame = frame_fixture();
    let mut model = DiskModel::default();
    model.absorb_frame(&frame);
    frame.parts.diskstats.devices.swap(0, 1);
    frame.parts.diskstats.devices[0].read_bps = Some(71_000.0);
    frame.parts.diskstats.devices[0].write_bps = None;
    frame.parts.diskstats.devices[1].read_bps = None;
    frame.parts.diskstats.devices[1].write_bps = Some(83_000.0);
    model.absorb_frame(&frame);

    let first = model
        .device_history
        .iter()
        .find(|history| history.id == InstanceId(1))
        .expect("device identity retained");
    let second = model
        .device_history
        .iter()
        .find(|history| history.id == InstanceId(0))
        .expect("device identity retained");
    let mut values = [0.0; super::HISTORY];
    let first_read_len = first.read.copy_ordered(&mut values);
    assert_eq!(values[first_read_len - 1], 71_000.0);
    let first_write_len = first.write.copy_ordered(&mut values);
    assert_eq!(values[first_write_len - 1], 570_000.0);
    let second_read_len = second.read.copy_ordered(&mut values);
    assert_eq!(values[second_read_len - 1], 3_000_000.0);
    let second_write_len = second.write.copy_ordered(&mut values);
    assert_eq!(values[second_write_len - 1], 83_000.0);

    let capacity = model.capacities();
    for tick in 0_u16..1000 {
        for device in &mut frame.parts.diskstats.devices {
            device.read_bps = Some(10_000.0 + f64::from(tick));
            device.write_bps = Some(20_000.0 + f64::from(tick));
        }
        model.absorb_frame(&frame);
    }
    assert_eq!(model.capacities(), capacity);
    assert!(model.device_history.iter().all(|history| {
        history.read.len == super::HISTORY
            && history.write.len == super::HISTORY
            && history.util.len == super::HISTORY
    }));
    let newest_history = model
        .device_history
        .iter()
        .find(|history| history.id == InstanceId(0))
        .expect("bounded history remains attached to device")
        .read;
    let newest = newest_history.values[(newest_history.next + super::HISTORY - 1) % super::HISTORY];
    assert_eq!(newest, 10_999.0);
}

#[test]
fn disk_history_absorbs_telemetry_series_with_warm_model_capacity() {
    let mut history = TelemetryHistory::default();
    for tick in 0_u64..120 {
        history.push(
            &thegn_metrics::StatsSnapshot {
                disk_io: Some((1_000_000 + tick * 3_000, 300_000 + tick * 700)),
                ..Default::default()
            },
            tick * 1000,
        );
    }
    let mut model = DiskModel::default();
    model.absorb_history(&history, Window::from_secs(120), ScaleMode::Window, 120_000);
    let capacity = model.capacities();
    let warm_rebuilds = model.telemetry_rebuilds;
    for _ in 0..1000 {
        model.absorb_history(&history, Window::from_secs(120), ScaleMode::Window, 120_000);
    }
    assert_eq!(model.capacities(), capacity);
    assert_eq!(model.telemetry_rebuilds, warm_rebuilds);
    assert_eq!(model.telemetry_read_history.len, super::HISTORY);
    assert_eq!(model.telemetry_write_history.len, super::HISTORY);
    assert_ne!(
        model.telemetry_read_history.values[0],
        model.telemetry_read_history.values[super::HISTORY - 1]
    );
    assert_ne!(
        model.telemetry_write_history.values[0],
        model.telemetry_write_history.values[super::HISTORY - 1]
    );
}

#[test]
fn mirrored_io_plot_places_newest_read_and_write_samples_at_the_right_edge() {
    let mut grid = CellGrid::new(12, 8);
    super::paint_mirrored_history(&mut grid, Rect::full(12, 8), &[1.0], &[1.0]);
    assert!((0..4).any(|row| { grid.get(11, row).is_some_and(|cell| cell.ch != ' ') }));
    assert!((4..8).any(|row| { grid.get(11, row).is_some_and(|cell| cell.ch != ' ') }));
    assert!((0..8).all(|row| { grid.get(0, row).is_some_and(|cell| cell.ch == ' ') }));
}
