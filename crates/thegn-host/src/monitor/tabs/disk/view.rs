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

#[derive(Debug, Default, Clone)]
pub(in crate::monitor) struct DiskModel {
    devices: Vec<DiskDevice>,
    filesystems: Vec<FilesystemRow>,
    io_psi: PsiResource,
    slice_io: Option<IoTotals>,
    read_bps: Option<f64>,
    write_bps: Option<f64>,
    read_history: Vec<f32>,
    write_history: Vec<f32>,
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
        self.filesystems.clone_from(&frame.parts.filesystems.rows);
        self.io_psi.clone_from(&frame.parts.psi.io);
        self.slice_io = frame.parts.cgroup_slice.sample.io;
        self.read_bps = frame.parts.diskstats.aggregate.read_bps;
        self.write_bps = frame.parts.diskstats.aggregate.write_bps;
        self.disk_health = frame.health[CollectorId::Diskstats.index()];
        self.fs_health = frame.health[CollectorId::Filesystems.index()];
        self.psi_health = frame.health[CollectorId::Psi.index()];
        self.slice_health = frame.health[CollectorId::CgroupSlice.index()];
        push_bounded(
            &mut self.read_history,
            frame.parts.diskstats.aggregate.read_bps,
        );
        push_bounded(
            &mut self.write_history,
            frame.parts.diskstats.aggregate.write_bps,
        );
    }

    #[cfg(test)]
    pub(in crate::monitor) fn capacities(&self) -> (usize, usize, usize, usize) {
        (
            self.devices.capacity(),
            self.filesystems.capacity(),
            self.read_history.capacity(),
            self.write_history.capacity(),
        )
    }
}

fn push_bounded(ring: &mut Vec<f32>, value: Option<f64>) {
    let Some(value) = value.filter(|value| value.is_finite() && *value >= 0.0) else {
        return;
    };
    if ring.len() == HISTORY {
        ring.remove(0);
    }
    if ring.capacity() < HISTORY {
        ring.reserve_exact(HISTORY - ring.capacity());
    }
    ring.push(value.min(f64::from(f32::MAX)) as f32);
}

pub(in crate::monitor) struct DiskView<'a>(pub &'a DiskModel);

impl TabView for DiskView<'_> {
    fn specs(&self) -> &'static [PanelSpec] {
        &SPECS
    }
    fn template(&self) -> Node {
        Node::Col(vec![
            (
                Size::Min(8),
                Node::Row(vec![
                    (Size::Weight(2), Node::Panel(IO)),
                    (Size::Weight(1), Node::Panel(PRESSURE)),
                    (Size::Weight(1), Node::Panel(SLICE)),
                ]),
            ),
            (
                Size::Min(10),
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
            && !self.0.read_history.is_empty()
            && rect.rows > 3
        {
            crate::monitor::graphs::paint_history_graph(
                &self.0.read_history,
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
        if placed.id == IO
            && !matches!(self.0.disk_health, Health::Absent(_))
            && !self.0.write_history.is_empty()
            && rect.rows > 4
        {
            crate::monitor::graphs::paint_history_graph(
                &self.0.write_history,
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
    let read = model
        .read_bps
        .map(|value| reading(value, Unit::BytesPerSec, area.cols / 2))
        .unwrap_or_else(|| crate::caps::active_glyphs().emdash.into());
    let write = model
        .write_bps
        .map(|value| reading(value, Unit::BytesPerSec, area.cols / 2))
        .unwrap_or_else(|| crate::caps::active_glyphs().emdash.into());
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
        body.put_str(
            0,
            1,
            &format!(
                "{} physical devices",
                model.devices.iter().filter(|d| d.physical).count()
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
        body.put_str(
            0,
            0,
            &format!("{} measuring", crate::caps::active_glyphs().ellipsis),
            CellStyle::default(),
        );
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
            "{:<12} {:>8} {:>8} {:>5}%",
            truncate(&device.name, 12),
            optional_rate(device.read_bps),
            optional_rate(device.write_bps),
            device
                .util_pct
                .map(|value| value.round() as u32)
                .map_or_else(
                    || crate::caps::active_glyphs().emdash.into(),
                    |value| value.to_string()
                )
        );
        body.put_str(0, row, &text, CellStyle::default());
    }
    if model.devices.is_empty() && !matches!(model.disk_health, Health::Absent(_)) {
        body.put_str(
            0,
            0,
            &format!("{} measuring", crate::caps::active_glyphs().ellipsis),
            CellStyle::default(),
        );
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
            .map(|value| Unit::Bytes.fmt(value as f32))
            .unwrap_or_else(|| crate::caps::active_glyphs().emdash.into());
        let used = used
            .map(|value| Unit::Bytes.fmt(value as f32))
            .unwrap_or_else(|| crate::caps::active_glyphs().emdash.into());
        let text = format!(
            "{} {} {used}/{total}",
            truncate(&fs.mount_point, 16),
            fs.fstype
        );
        body.put_str(0, row, &text, CellStyle::default());
    }
    if model.filesystems.is_empty() {
        body.put_str(
            0,
            0,
            &format!("{} measuring", crate::caps::active_glyphs().ellipsis),
            CellStyle::default(),
        );
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
        Some(io) => body.put_str(
            0,
            0,
            &format!(
                "read {}  write {}",
                Unit::Bytes.fmt(io.rbytes as f32),
                Unit::Bytes.fmt(io.wbytes as f32)
            ),
            CellStyle::default(),
        ),
        None => body.put_str(
            0,
            0,
            &format!("{} measuring", crate::caps::active_glyphs().ellipsis),
            CellStyle::default(),
        ),
    }
}

fn absent_mark(health: Health) -> &'static str {
    match health {
        Health::Absent(Absent::NoPermission) => crate::caps::glyph(crate::caps::Glyph::Lock),
        _ => crate::caps::active_glyphs().emdash,
    }
}

fn reading(value: f64, unit: Unit, width: usize) -> String {
    kit_reading(value, unit, width)
}
fn kit_reading(value: f64, unit: Unit, width: usize) -> String {
    crate::monitor::kit::edge_fmt(NumFmt::default()).reading(Reading::Value(value), unit, width)
}
fn optional_rate(value: Option<f64>) -> String {
    value
        .map(|value| reading(value, Unit::BytesPerSec, 8))
        .unwrap_or_else(|| crate::caps::active_glyphs().emdash.into())
}
fn truncate(value: &str, width: usize) -> String {
    value.chars().take(width).collect()
}

#[cfg(test)]
#[path = "disk_view_tests.rs"]
mod tests;
