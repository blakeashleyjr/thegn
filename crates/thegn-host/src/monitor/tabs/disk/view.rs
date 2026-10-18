#![allow(dead_code)] // wired into the overlay dispatch in batch 48 (see THE-889)

use crate::{
    compositor::Rect,
    monitor::{
        board::TabView,
        kit::{
            emit::Emitter,
            grid::{CellGrid, CellStyle},
            panel,
        },
        layout::{Node, PanelId, PanelSpec, Placed, Size, Variant, VariantKind},
    },
    sections::Section,
};
use termwiz::surface::Surface;
use thegn_core::numfmt::{NumFmt, Reading, Unit};
use thegn_core::theme::RampId;
use thegn_metrics::collect::{
    CollectorId, CollectorSet,
    frame::MonitorFrame,
    health::{Absent, Health},
    model::{
        cgroup::IoTotals, diskstats::DiskDevice, filesystems::FilesystemRow, psi::PsiResource,
    },
};

const IO: PanelId = PanelId("disk.io");
const PRESSURE: PanelId = PanelId("disk.pressure");
const DEVICES: PanelId = PanelId("disk.devices");
const FILESYSTEMS: PanelId = PanelId("disk.filesystems");
const SLICE: PanelId = PanelId("disk.slice");
const VARIANT: [Variant; 1] = [Variant {
    kind: VariantKind::Full,
    min: (20, 4),
    pref: (48, 10),
}];
const SPECS: [PanelSpec; 5] = [
    PanelSpec {
        id: IO,
        priority: 0,
        variants: &VARIANT,
    },
    PanelSpec {
        id: PRESSURE,
        priority: 1,
        variants: &VARIANT,
    },
    PanelSpec {
        id: DEVICES,
        priority: 2,
        variants: &VARIANT,
    },
    PanelSpec {
        id: FILESYSTEMS,
        priority: 3,
        variants: &VARIANT,
    },
    PanelSpec {
        id: SLICE,
        priority: 4,
        variants: &VARIANT,
    },
];
const HISTORY: usize = 240;
const MAX_DEVICES: usize = 32;

#[derive(Debug, Clone)]
struct HistoryRing {
    values: [f32; HISTORY],
    next: usize,
    len: usize,
}

impl Default for HistoryRing {
    fn default() -> Self {
        Self {
            values: [0.0; HISTORY],
            next: 0,
            len: 0,
        }
    }
}

impl HistoryRing {
    fn push(&mut self, value: f32) {
        self.values[self.next] = value;
        self.next = (self.next + 1) % HISTORY;
        self.len = self.len.saturating_add(1).min(HISTORY);
    }

    fn copy_normalized(&self, out: &mut [f32; HISTORY]) -> (usize, f32) {
        let start = (self.next + HISTORY - self.len) % HISTORY;
        let peak = self.values.iter().copied().fold(0.0_f32, f32::max);
        for (offset, slot) in out.iter_mut().take(self.len).enumerate() {
            let value = self.values[(start + offset) % HISTORY];
            *slot = if peak > 0.0 { value / peak } else { 0.0 };
        }
        (self.len, peak)
    }

    fn copy_ordered(&self, out: &mut [f32; HISTORY]) -> usize {
        let start = (self.next + HISTORY - self.len) % HISTORY;
        for (offset, slot) in out.iter_mut().take(self.len).enumerate() {
            *slot = self.values[(start + offset) % HISTORY];
        }
        self.len
    }
}

#[derive(Debug, Clone)]
struct DeviceHistory {
    id: thegn_metrics::collect::instances::InstanceId,
    util: HistoryRing,
}

#[derive(Debug, Default, Clone)]
pub(in crate::monitor) struct DiskModel {
    devices: Vec<DiskDevice>,
    filesystems: Vec<FilesystemRow>,
    io_psi: PsiResource,
    slice_io: Option<IoTotals>,
    read_bps: Option<f64>,
    write_bps: Option<f64>,
    read_history: HistoryRing,
    write_history: HistoryRing,
    device_history: Vec<DeviceHistory>,
    physical_count: u16,
    disk_health: Health,
    fs_health: Health,
    psi_health: Health,
    slice_health: Health,
}

impl DiskModel {
    pub(in crate::monitor) fn absorb_frame(&mut self, frame: &MonitorFrame) {
        self.devices.clear();
        self.devices.extend(
            frame
                .parts
                .diskstats
                .devices
                .iter()
                .take(MAX_DEVICES)
                .cloned(),
        );
        self.filesystems.reserve(
            frame
                .parts
                .filesystems
                .rows
                .len()
                .saturating_sub(self.filesystems.len()),
        );
        for (index, source) in frame.parts.filesystems.rows.iter().enumerate() {
            if let Some(destination) = self.filesystems.get_mut(index) {
                destination.mount_point.clone_from(&source.mount_point);
                destination.fstype.clone_from(&source.fstype);
                destination.source.clone_from(&source.source);
                destination
                    .superblock_dev
                    .clone_from(&source.superblock_dev);
                destination.kind = source.kind;
                destination.total_bytes = source.total_bytes;
                destination.free_bytes = source.free_bytes;
                destination.avail_bytes = source.avail_bytes;
                destination.total_inodes = source.total_inodes;
                destination.free_inodes = source.free_inodes;
                destination.avail_inodes = source.avail_inodes;
                destination.btrfs_unallocated = source.btrfs_unallocated;
                destination.read_only = source.read_only;
            } else {
                self.filesystems.push(source.clone());
            }
        }
        self.filesystems
            .truncate(frame.parts.filesystems.rows.len());
        self.io_psi.clone_from(&frame.parts.psi.io);
        self.slice_io = frame.parts.cgroup_slice.sample.io;
        self.physical_count = frame.parts.diskstats.physical;
        self.read_bps = frame.parts.diskstats.aggregate.read_bps;
        self.write_bps = frame.parts.diskstats.aggregate.write_bps;
        self.disk_health = frame.health[CollectorId::Diskstats.index()];
        self.fs_health = frame.health[CollectorId::Filesystems.index()];
        self.psi_health = frame.health[CollectorId::Psi.index()];
        self.slice_health = frame.health[CollectorId::CgroupSlice.index()];
        if let (Some(read), Some(write)) = (
            frame.parts.diskstats.aggregate.read_bps,
            frame.parts.diskstats.aggregate.write_bps,
        ) && read.is_finite()
            && write.is_finite()
            && read >= 0.0
            && write >= 0.0
        {
            self.read_history.push(read.min(f64::from(f32::MAX)) as f32);
            self.write_history
                .push(write.min(f64::from(f32::MAX)) as f32);
        }
        self.device_history
            .retain(|history| self.devices.iter().any(|device| device.id == history.id));
        if self.device_history.capacity() == 0 && !frame.parts.diskstats.devices.is_empty() {
            self.device_history.reserve_exact(MAX_DEVICES);
        }
        for device in self
            .devices
            .iter()
            .filter(|device| device.physical)
            .take(MAX_DEVICES)
        {
            if let Some(value) = device
                .util_pct
                .filter(|value| value.is_finite() && *value >= 0.0)
            {
                if let Some(history) = self
                    .device_history
                    .iter_mut()
                    .find(|history| history.id == device.id)
                {
                    history.util.push(value.min(100.0) as f32 / 100.0);
                } else if self.device_history.len() < MAX_DEVICES {
                    let mut history = DeviceHistory {
                        id: device.id,
                        util: HistoryRing::default(),
                    };
                    history.util.push(value.min(100.0) as f32 / 100.0);
                    self.device_history.push(history);
                }
            }
        }
    }

    #[cfg(test)]
    pub(in crate::monitor) fn capacities(&self) -> (usize, usize, usize, usize) {
        (
            self.devices.capacity(),
            self.filesystems.capacity(),
            self.device_history.capacity(),
            HISTORY,
        )
    }
}

pub(in crate::monitor) struct DiskView<'a>(pub &'a DiskModel);

impl TabView for DiskView<'_> {
    fn specs(&self) -> &'static [PanelSpec] {
        &SPECS
    }
    fn template(&self) -> Node {
        Node::Col(vec![
            (
                Size::Weight(3),
                Node::Row(vec![
                    (Size::Weight(2), Node::Panel(IO)),
                    (Size::Weight(1), Node::Panel(PRESSURE)),
                    (Size::Weight(1), Node::Panel(SLICE)),
                ]),
            ),
            (
                Size::Weight(2),
                Node::Row(vec![
                    (Size::Weight(1), Node::Panel(DEVICES)),
                    (Size::Weight(1), Node::Panel(FILESYSTEMS)),
                ]),
            ),
        ])
    }
    fn demand(&self, panel: PanelId) -> CollectorSet {
        let mut set = CollectorSet::empty();
        match panel {
            IO | DEVICES => set.insert(CollectorId::Diskstats),
            PRESSURE => set.insert(CollectorId::Psi),
            FILESYSTEMS => set.insert(CollectorId::Filesystems),
            SLICE => set.insert(CollectorId::CgroupSlice),
            _ => {}
        }
        set
    }
    fn paint(
        &self,
        surface: &mut Surface,
        placed: &Placed,
        rect: Rect,
        _scroll: usize,
        _sections: &[Section],
    ) {
        let mut grid = CellGrid::new(rect.cols, rect.rows);
        match placed.id {
            IO => paint_io(&mut grid, rect, self.0),
            PRESSURE => paint_pressure(&mut grid, rect, self.0),
            DEVICES => paint_devices(&mut grid, rect, self.0),
            FILESYSTEMS => paint_filesystems(&mut grid, rect, self.0),
            SLICE => paint_slice(&mut grid, rect, self.0),
            _ => return,
        }
        Emitter::default().emit(
            &grid,
            Rect::full(rect.cols, rect.rows),
            surface,
            rect.x,
            rect.y,
        );
        if placed.id == IO
            && !matches!(self.0.disk_health, Health::Absent(_))
            && self.0.read_history.len > 0
            && rect.rows > 3
        {
            let mut normalized = [0.0; HISTORY];
            let (len, _) = self.0.read_history.copy_normalized(&mut normalized);
            crate::monitor::graphs::paint_history_graph(
                &normalized[..len],
                RampId::Read,
                Rect {
                    x: rect.x + 1,
                    y: rect.y + 3,
                    cols: rect.cols.saturating_sub(2),
                    rows: rect.rows.saturating_sub(4) / 2,
                },
                surface,
            );
        }
        if placed.id == DEVICES && !matches!(self.0.disk_health, Health::Absent(_)) {
            for (row, device) in self
                .0
                .devices
                .iter()
                .take(rect.rows.saturating_sub(2))
                .enumerate()
            {
                if device.physical
                    && let Some(history) = self
                        .0
                        .device_history
                        .iter()
                        .find(|history| history.id == device.id)
                {
                    let mut normalized = [0.0; HISTORY];
                    let len = history.util.copy_ordered(&mut normalized);
                    crate::monitor::graphs::paint_sparkline(
                        &normalized[..len],
                        8,
                        RampId::Util,
                        Rect {
                            x: rect.x + rect.cols.saturating_sub(10),
                            y: rect.y + row + 1,
                            cols: 8,
                            rows: 1,
                        },
                        surface,
                    );
                }
            }
        }
        if placed.id == IO
            && !matches!(self.0.disk_health, Health::Absent(_))
            && self.0.write_history.len > 0
            && rect.rows > 4
        {
            let mut normalized = [0.0; HISTORY];
            let (len, _) = self.0.write_history.copy_normalized(&mut normalized);
            crate::monitor::graphs::paint_history_graph(
                &normalized[..len],
                RampId::Write,
                Rect {
                    x: rect.x + 1,
                    y: rect.y + 3 + rect.rows.saturating_sub(4) / 2,
                    cols: rect.cols.saturating_sub(2),
                    rows: rect.rows.saturating_sub(4) / 2,
                },
                surface,
            );
        }
    }
}

fn paint_io(grid: &mut CellGrid, rect: Rect, model: &DiskModel) {
    let mut body = panel::panel(grid, Rect::full(rect.cols, rect.rows), "I/O", None);
    let area = body.rect();
    if matches!(model.disk_health, Health::Absent(_)) {
        body.put_str(0, 0, absent_mark(model.disk_health), CellStyle::default());
        return;
    }
    let read = rate_reading(model.read_bps, model.disk_health, area.cols / 2);
    let write = rate_reading(model.write_bps, model.disk_health, area.cols / 2);
    body.put_str(
        0,
        0,
        &format!(
            "{}{read}  {}{write}",
            crate::caps::active_glyphs().arrow_up,
            crate::caps::active_glyphs().arrow_down
        ),
        CellStyle::default(),
    );
    if area.rows > 1 {
        let count = if model.devices.is_empty() {
            format!("{} physical devices", crate::caps::active_glyphs().ellipsis)
        } else {
            format!("{} physical devices", model.physical_count)
        };
        body.put_str(0, 1, &count, CellStyle::default());
    }
    if area.rows > 2 {
        body.put_str(
            0,
            2,
            &format!(
                "peak {}{}  {}{}",
                crate::caps::active_glyphs().arrow_up,
                history_peak(&model.read_history, model.disk_health, area.cols / 2),
                crate::caps::active_glyphs().arrow_down,
                history_peak(&model.write_history, model.disk_health, area.cols / 2)
            ),
            CellStyle::default(),
        );
    }
}

fn paint_pressure(grid: &mut CellGrid, rect: Rect, model: &DiskModel) {
    let mut body = panel::panel(grid, Rect::full(rect.cols, rect.rows), "I/O pressure", None);
    if matches!(model.psi_health, Health::Absent(_)) {
        body.put_str(0, 0, absent_mark(model.psi_health), CellStyle::default());
        return;
    }
    if !model.io_psi.present && model.io_psi.file.some.is_none() {
        let mark = if matches!(model.psi_health, Health::Warming) {
            crate::caps::active_glyphs().ellipsis
        } else {
            crate::caps::active_glyphs().emdash
        };
        body.put_str(0, 0, mark, CellStyle::default());
        return;
    }
    let some = model
        .io_psi
        .file
        .some
        .map(|line| format!("some {:.2}%", f64::from(line.avg10) / 100.0))
        .unwrap_or_else(|| format!("some {}", crate::caps::active_glyphs().emdash));
    let full = model
        .io_psi
        .file
        .full
        .map(|line| format!("full {:.2}%", f64::from(line.avg10) / 100.0))
        .unwrap_or_else(|| format!("full {}", crate::caps::active_glyphs().emdash));
    body.put_str(0, 0, &some, CellStyle::default());
    body.put_str(0, 1, &full, CellStyle::default());
}

fn paint_devices(grid: &mut CellGrid, rect: Rect, model: &DiskModel) {
    let mut body = panel::panel(grid, Rect::full(rect.cols, rect.rows), "Devices", None);
    if matches!(model.disk_health, Health::Absent(_)) {
        body.put_str(0, 0, absent_mark(model.disk_health), CellStyle::default());
        return;
    }
    for (row, device) in model.devices.iter().take(body.rect().rows).enumerate() {
        let text = format!(
            "{:<12} {:>8} {:>8} {:>5}{}",
            truncate(&device.name, 12),
            optional_rate(device.read_bps),
            optional_rate(device.write_bps),
            device
                .util_pct
                .map(|value| value.round() as u32)
                .map_or_else(
                    || crate::caps::active_glyphs().emdash.into(),
                    |value| format!("{value}%")
                )
        );
        body.put_str(0, row, &text, CellStyle::default());
    }
    if model.devices.is_empty() && !matches!(model.disk_health, Health::Absent(_)) {
        let mark = if matches!(model.disk_health, Health::Warming) {
            crate::caps::active_glyphs().ellipsis
        } else {
            crate::caps::active_glyphs().emdash
        };
        body.put_str(0, 0, mark, CellStyle::default());
    }
}

fn paint_filesystems(grid: &mut CellGrid, rect: Rect, model: &DiskModel) {
    let mut body = panel::panel(grid, Rect::full(rect.cols, rect.rows), "Filesystems", None);
    if matches!(model.fs_health, Health::Absent(_)) {
        body.put_str(0, 0, absent_mark(model.fs_health), CellStyle::default());
        return;
    }
    for (row, fs) in model.filesystems.iter().take(body.rect().rows).enumerate() {
        let used = fs
            .total_bytes
            .zip(fs.free_bytes)
            .map(|(total, free)| total.saturating_sub(free));
        let total = fs
            .total_bytes
            .map(|value| {
                crate::monitor::kit::edge_fmt(NumFmt::default()).fmt(value as f64, Unit::Bytes, 8)
            })
            .unwrap_or_else(|| crate::caps::active_glyphs().emdash.into());
        let used = used
            .map(|value| {
                crate::monitor::kit::edge_fmt(NumFmt::default()).fmt(value as f64, Unit::Bytes, 8)
            })
            .unwrap_or_else(|| crate::caps::active_glyphs().emdash.into());
        let text = format!(
            "{} {} {used}/{total}",
            truncate(&fs.mount_point, 16),
            fs.fstype
        );
        body.put_str(0, row, &text, CellStyle::default());
    }
    if model.filesystems.is_empty() {
        let mark = if matches!(model.fs_health, Health::Warming) {
            crate::caps::active_glyphs().ellipsis
        } else {
            crate::caps::active_glyphs().emdash
        };
        body.put_str(0, 0, mark, CellStyle::default());
    }
}

fn paint_slice(grid: &mut CellGrid, rect: Rect, model: &DiskModel) {
    let mut body = panel::panel(
        grid,
        Rect::full(rect.cols, rect.rows),
        "thegn.slice I/O",
        None,
    );
    if matches!(model.slice_health, Health::Absent(_)) {
        body.put_str(0, 0, absent_mark(model.slice_health), CellStyle::default());
        return;
    }
    match model.slice_io {
        Some(io) => {
            body.put_str(
                0,
                0,
                &format!(
                    "totals read {}  write {}",
                    crate::monitor::kit::edge_fmt(NumFmt::default()).fmt(
                        io.rbytes as f64,
                        Unit::Bytes,
                        8
                    ),
                    crate::monitor::kit::edge_fmt(NumFmt::default()).fmt(
                        io.wbytes as f64,
                        Unit::Bytes,
                        8
                    )
                ),
                CellStyle::default(),
            );
        }
        None => {
            let mark = if matches!(model.slice_health, Health::Warming) {
                crate::caps::active_glyphs().ellipsis
            } else {
                crate::caps::active_glyphs().emdash
            };
            body.put_str(0, 0, mark, CellStyle::default());
        }
    }
}

fn absent_mark(health: Health) -> &'static str {
    match health {
        Health::Absent(Absent::NoPermission) => crate::caps::glyph(crate::caps::Glyph::Lock),
        _ => crate::caps::active_glyphs().emdash,
    }
}

fn rate_reading(value: Option<f64>, health: Health, width: usize) -> String {
    let reading = match (health, value) {
        (Health::Warming, _) => Reading::Warming,
        (_, Some(value)) => Reading::Value(value),
        (_, None) => Reading::Unknown,
    };
    let formatted = crate::monitor::kit::edge_fmt(NumFmt::default()).reading(
        reading,
        Unit::Bytes,
        width.saturating_sub(2),
    );
    format!("{formatted}/s")
}
fn history_peak(history: &HistoryRing, health: Health, width: usize) -> String {
    if history.len == 0 {
        rate_reading(None, health, width)
    } else {
        let peak = history.values.iter().copied().fold(0.0_f32, f32::max);
        rate_reading(Some(f64::from(peak)), health, width)
    }
}
fn optional_rate(value: Option<f64>) -> String {
    value
        .map(|value| {
            format!(
                "{}/s",
                crate::monitor::kit::edge_fmt(NumFmt::default()).fmt(value, Unit::Bytes, 5)
            )
        })
        .unwrap_or_else(|| crate::caps::active_glyphs().emdash.into())
}
fn truncate(value: &str, width: usize) -> String {
    value.chars().take(width).collect()
}

#[cfg(test)]
#[path = "disk_view_tests.rs"]
mod tests;
