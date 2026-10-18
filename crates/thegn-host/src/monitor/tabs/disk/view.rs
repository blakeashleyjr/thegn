use crate::telemetry::{Metric, ScaleMode, SeriesBuf, SeriesReq, TelemetryHistory, Window};

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
    seg::StyleTok,
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
const WORKTREES: PanelId = PanelId("disk.worktrees");
const VARIANT: [Variant; 1] = [Variant {
    kind: VariantKind::Full,
    min: (20, 4),
    pref: (48, 10),
}];
const SPECS: [PanelSpec; 6] = [
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
    PanelSpec {
        id: WORKTREES,
        priority: 5,
        variants: &VARIANT,
    },
];
const HISTORY: usize = 240;
const MAX_DEVICES: usize = 32;
const MAX_DEVICE_HISTORIES: usize = 8;
const MAX_FILESYSTEM_HISTORIES: usize = 8;

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
        let peak = self.finite_peak().unwrap_or(0.0);
        for (offset, slot) in out.iter_mut().take(self.len).enumerate() {
            let value = self.values[(start + offset) % HISTORY];
            *slot = if value.is_nan() {
                f32::NAN
            } else if peak > 0.0 {
                value / peak
            } else {
                0.0
            };
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

    fn finite_peak(&self) -> Option<f32> {
        let start = (self.next + HISTORY - self.len) % HISTORY;
        (0..self.len)
            .map(|offset| self.values[(start + offset) % HISTORY])
            .filter(|value| value.is_finite())
            .reduce(f32::max)
    }

    fn replace_from(&mut self, values: &[f32]) {
        self.values.fill(0.0);
        self.len = values.len().min(HISTORY);
        self.next = self.len % HISTORY;
        let start = values.len().saturating_sub(self.len);
        self.values[..self.len].copy_from_slice(&values[start..]);
    }
}

#[derive(Debug, Clone)]
struct DeviceHistory {
    id: thegn_metrics::collect::instances::InstanceId,
    read: HistoryRing,
    write: HistoryRing,
    util: HistoryRing,
}

#[derive(Debug, Clone)]
struct FilesystemHistory {
    mount_point: String,
    source: String,
    used: HistoryRing,
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
    telemetry_read_history: HistoryRing,
    telemetry_write_history: HistoryRing,
    telemetry_cache: Option<(u64, Option<u32>, ScaleMode, u64)>,
    history_buf: SeriesBuf,
    telemetry_read_peak: Option<f32>,
    telemetry_write_peak: Option<f32>,
    #[cfg(test)]
    telemetry_rebuilds: usize,
    psi_some_history: HistoryRing,
    psi_full_history: HistoryRing,
    slice_read_history: HistoryRing,
    slice_write_history: HistoryRing,
    last_rbytes: Option<u64>,
    last_wbytes: Option<u64>,
    device_history: Vec<DeviceHistory>,
    filesystem_history: Vec<FilesystemHistory>,
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
                destination.extra_mounts = source.extra_mounts;
            } else {
                self.filesystems.push(source.clone());
            }
        }
        self.filesystems
            .truncate(frame.parts.filesystems.rows.len());
        if self.filesystem_history.capacity() == 0 && !frame.parts.filesystems.rows.is_empty() {
            self.filesystem_history
                .reserve_exact(MAX_FILESYSTEM_HISTORIES);
        }
        for fs in frame
            .parts
            .filesystems
            .rows
            .iter()
            .take(MAX_FILESYSTEM_HISTORIES)
        {
            if self
                .filesystem_history
                .iter()
                .all(|history| history.mount_point != fs.mount_point || history.source != fs.source)
            {
                if self.filesystem_history.len() < MAX_FILESYSTEM_HISTORIES {
                    self.filesystem_history.push(FilesystemHistory {
                        mount_point: fs.mount_point.clone(),
                        source: fs.source.clone(),
                        used: HistoryRing::default(),
                    });
                } else if let Some(history) = self.filesystem_history.iter_mut().find(|history| {
                    !frame.parts.filesystems.rows.iter().any(|current| {
                        current.mount_point == history.mount_point
                            && current.source == history.source
                    })
                }) {
                    history.mount_point.clone_from(&fs.mount_point);
                    history.source.clone_from(&fs.source);
                    history.used = HistoryRing::default();
                }
            }
            if let Some(history) = self.filesystem_history.iter_mut().find(|history| {
                history.mount_point == fs.mount_point && history.source == fs.source
            }) {
                let ratio = fs
                    .total_bytes
                    .filter(|total| *total > 0)
                    .zip(fs.free_bytes)
                    .map_or(f32::NAN, |(total, free)| {
                        total.saturating_sub(free).min(total) as f32 / total as f32
                    });
                history.used.push(ratio);
            }
        }
        for history in &mut self.filesystem_history {
            if !frame
                .parts
                .filesystems
                .rows
                .iter()
                .any(|fs| fs.mount_point == history.mount_point && fs.source == history.source)
            {
                history.used.push(f32::NAN);
            }
        }
        self.io_psi.clone_from(&frame.parts.psi.io);
        self.psi_some_history.push(
            self.io_psi
                .file
                .some
                .map_or(f32::NAN, |line| line.avg10 as f32 / 10_000.0),
        );
        self.psi_full_history.push(
            self.io_psi
                .file
                .full
                .map_or(f32::NAN, |line| line.avg10 as f32 / 10_000.0),
        );
        self.slice_io = frame.parts.cgroup_slice.sample.io;
        let interval_secs = f64::from(frame.dt_ms.max(1)) / 1000.0;
        let (read_rate, next_rbytes) = self.slice_io.map_or((f32::NAN, None), |io| {
            let rate = self.last_rbytes.map_or(f32::NAN, |last| {
                io.rbytes.checked_sub(last).map_or(f32::NAN, |delta| {
                    (delta as f64 / interval_secs).min(f64::from(f32::MAX)) as f32
                })
            });
            (rate, Some(io.rbytes))
        });
        let (write_rate, next_wbytes) = self.slice_io.map_or((f32::NAN, None), |io| {
            let rate = self.last_wbytes.map_or(f32::NAN, |last| {
                io.wbytes.checked_sub(last).map_or(f32::NAN, |delta| {
                    (delta as f64 / interval_secs).min(f64::from(f32::MAX)) as f32
                })
            });
            (rate, Some(io.wbytes))
        });
        self.slice_read_history.push(read_rate);
        self.slice_write_history.push(write_rate);
        self.last_rbytes = next_rbytes;
        self.last_wbytes = next_wbytes;
        self.physical_count = frame.parts.diskstats.physical;
        self.read_bps = frame.parts.diskstats.aggregate.read_bps;
        self.write_bps = frame.parts.diskstats.aggregate.write_bps;
        self.disk_health = frame.health[CollectorId::Diskstats.index()];
        self.fs_health = frame.health[CollectorId::Filesystems.index()];
        self.psi_health = frame.health[CollectorId::Psi.index()];
        self.slice_health = frame.health[CollectorId::CgroupSlice.index()];
        if let Some(read) = frame.parts.diskstats.aggregate.read_bps
            && read.is_finite()
            && read >= 0.0
        {
            self.read_history.push(read.min(f64::from(f32::MAX)) as f32);
        } else {
            self.read_history.push(f32::NAN);
        }
        if let Some(write) = frame.parts.diskstats.aggregate.write_bps
            && write.is_finite()
            && write >= 0.0
        {
            self.write_history
                .push(write.min(f64::from(f32::MAX)) as f32);
        } else {
            self.write_history.push(f32::NAN);
        }
        if self.device_history.capacity() == 0 && !frame.parts.diskstats.devices.is_empty() {
            self.device_history.reserve_exact(MAX_DEVICE_HISTORIES);
        }
        for device in self
            .devices
            .iter()
            .filter(|device| device.physical)
            .take(MAX_DEVICE_HISTORIES)
        {
            if self
                .device_history
                .iter()
                .all(|history| history.id != device.id)
                && self.device_history.len() >= MAX_DEVICE_HISTORIES
                && let Some(index) = self
                    .device_history
                    .iter()
                    .position(|history| !self.devices.iter().any(|device| device.id == history.id))
            {
                self.device_history.remove(index);
            }
            if self
                .device_history
                .iter()
                .all(|history| history.id != device.id)
                && self.device_history.len() < MAX_DEVICE_HISTORIES
            {
                self.device_history.push(DeviceHistory {
                    id: device.id,
                    read: HistoryRing::default(),
                    write: HistoryRing::default(),
                    util: HistoryRing::default(),
                });
            }
            if let Some(history) = self
                .device_history
                .iter_mut()
                .find(|history| history.id == device.id)
            {
                if let Some(value) = device
                    .read_bps
                    .filter(|value| value.is_finite() && *value >= 0.0)
                {
                    history.read.push(value.min(f64::from(f32::MAX)) as f32);
                } else {
                    history.read.push(f32::NAN);
                }
                if let Some(value) = device
                    .write_bps
                    .filter(|value| value.is_finite() && *value >= 0.0)
                {
                    history.write.push(value.min(f64::from(f32::MAX)) as f32);
                } else {
                    history.write.push(f32::NAN);
                }
                if let Some(value) = device
                    .util_pct
                    .filter(|value| value.is_finite() && *value >= 0.0)
                {
                    history.util.push(value.min(100.0) as f32 / 100.0);
                } else {
                    history.util.push(f32::NAN);
                }
            }
        }
        for history in &mut self.device_history {
            if !self.devices.iter().any(|device| device.id == history.id) {
                history.read.push(f32::NAN);
                history.write.push(f32::NAN);
                history.util.push(f32::NAN);
            }
        }
    }

    pub(in crate::monitor) fn absorb_history(
        &mut self,
        history: &TelemetryHistory,
        window: Window,
        scale: ScaleMode,
        now_ms: u64,
    ) {
        let bucket_ms = window
            .secs()
            .map_or(1000, |secs| u64::from(secs) * 1000 / HISTORY as u64)
            .max(1);
        let cache_key = (
            history.generation(),
            window.secs(),
            scale,
            now_ms / bucket_ms,
        );
        if self.telemetry_cache == Some(cache_key) {
            return;
        }
        #[cfg(test)]
        {
            self.telemetry_rebuilds = self.telemetry_rebuilds.saturating_add(1);
        }
        for (metric, slot) in [
            (Metric::DiskRead, &mut self.telemetry_read_history),
            (Metric::DiskWrite, &mut self.telemetry_write_history),
        ] {
            history.series_into(
                &SeriesReq {
                    metric,
                    window,
                    scale: TelemetryHistory::scale_for(metric, scale),
                    buckets: HISTORY,
                    agg: thegn_core::series::Agg::Max,
                    now_ms,
                },
                &mut self.history_buf,
            );
            if self.history_buf.empty() {
                slot.replace_from(&[]);
            } else {
                let hi = self.history_buf.hi();
                let raw_hi = self.history_buf.raw_hi();
                let gaps = self.history_buf.gap();
                let mut held = f32::NAN;
                let mut values = [f32::NAN; HISTORY];
                for (slot, ((&hi, &raw), &gap)) in
                    values.iter_mut().zip(hi.iter().zip(raw_hi).zip(gaps))
                {
                    if gap {
                        *slot = held;
                    } else if raw.is_finite() {
                        held = hi;
                        *slot = held;
                    } else {
                        held = f32::NAN;
                    }
                }
                slot.replace_from(&values[..hi.len().min(HISTORY)]);
            }
            let peak = if self.history_buf.empty() {
                None
            } else {
                self.history_buf
                    .raw_hi()
                    .iter()
                    .copied()
                    .filter(|value| value.is_finite())
                    .reduce(f32::max)
            };
            if metric == Metric::DiskRead {
                self.telemetry_read_peak = peak;
            } else {
                self.telemetry_write_peak = peak;
            }
        }
        self.telemetry_cache = Some(cache_key);
    }

    #[cfg(test)]
    pub(in crate::monitor) fn capacities(&self) -> (usize, usize, usize, usize, usize) {
        (
            self.devices.capacity(),
            self.filesystems.capacity(),
            self.device_history.capacity(),
            self.filesystem_history.capacity(),
            HISTORY,
        )
    }

    #[cfg(test)]
    pub(in crate::monitor) fn test_read_bps(&self) -> Option<f64> {
        self.read_bps
    }

    #[cfg(test)]
    pub(in crate::monitor) fn test_telemetry_state(
        &self,
    ) -> (usize, usize, Option<f32>, Option<f32>) {
        (
            self.telemetry_read_history.len,
            self.telemetry_write_history.len,
            self.telemetry_read_peak,
            self.telemetry_write_peak,
        )
    }

    #[cfg(test)]
    pub(in crate::monitor) fn test_telemetry_read_values(&self) -> [f32; HISTORY] {
        let mut values = [f32::NAN; HISTORY];
        self.telemetry_read_history.copy_ordered(&mut values);
        values
    }

    #[cfg(test)]
    pub(in crate::monitor) fn test_telemetry_rebuilds(&self) -> usize {
        self.telemetry_rebuilds
    }
}

pub(in crate::monitor) struct DiskView<'a> {
    model: &'a DiskModel,
    worktrees: &'a [crate::monitor::build::DiskWtRow],
    sel: usize,
}

impl<'a> DiskView<'a> {
    pub(in crate::monitor) fn new(
        model: &'a DiskModel,
        worktrees: &'a [crate::monitor::build::DiskWtRow],
        sel: usize,
    ) -> Self {
        Self {
            model,
            worktrees,
            sel,
        }
    }
}

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
            (Size::Weight(1), Node::Panel(WORKTREES)),
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
            IO => paint_io(&mut grid, rect, self.model),
            PRESSURE => paint_pressure(&mut grid, rect, self.model),
            DEVICES => paint_devices(&mut grid, rect, self.model),
            FILESYSTEMS => paint_filesystems(&mut grid, rect, self.model),
            SLICE => paint_slice(&mut grid, rect, self.model),
            WORKTREES => paint_worktrees(&mut grid, rect, self.worktrees, self.sel),
            _ => return,
        }
        Emitter::default().emit(
            &grid,
            Rect::full(rect.cols, rect.rows),
            surface,
            rect.x,
            rect.y,
        );
        if placed.id == FILESYSTEMS && !matches!(self.model.fs_health, Health::Absent(_)) {
            let inner = Rect {
                x: rect.x + 1,
                y: rect.y + 1,
                cols: rect.cols.saturating_sub(2),
                rows: rect.rows.saturating_sub(2),
            };
            for (row, fs) in self
                .model
                .filesystems
                .iter()
                .take(inner.rows / 2)
                .enumerate()
            {
                if let Some(history) = self.model.filesystem_history.iter().find(|history| {
                    history.mount_point == fs.mount_point && history.source == fs.source
                }) {
                    let mut values = [0.0; HISTORY];
                    let len = history.used.copy_ordered(&mut values);
                    let meter_cols = inner.cols.saturating_mul(2) / 3;
                    let trend_cols = inner.cols.saturating_sub(meter_cols);
                    crate::monitor::graphs::paint_sparkline(
                        &values[..len],
                        trend_cols,
                        RampId::Util,
                        Rect {
                            x: inner.x + meter_cols,
                            y: inner.y + row * 2 + 1,
                            cols: trend_cols,
                            rows: 1,
                        },
                        surface,
                    );
                }
            }
        }
        if placed.id == PRESSURE
            && !matches!(self.model.psi_health, Health::Absent(_))
            && rect.rows > 5
            && rect.cols > 2
        {
            let graph_rows = rect.rows.saturating_sub(4) / 2;
            let mut normalized = [0.0; HISTORY];
            let some_len = self.model.psi_some_history.copy_ordered(&mut normalized);
            let braille = crate::caps::raster_table().braille;
            let some_rect = crate::monitor::kit::history_rect::graph_rect(
                Rect {
                    x: rect.x + 1,
                    y: rect.y + 3,
                    cols: rect.cols.saturating_sub(2),
                    rows: graph_rows,
                },
                some_len,
                braille,
            );
            crate::monitor::graphs::paint_history_graph(
                &normalized[..some_len],
                RampId::Read,
                some_rect,
                surface,
            );
            let full_len = self.model.psi_full_history.copy_ordered(&mut normalized);
            let full_rect = crate::monitor::kit::history_rect::graph_rect(
                Rect {
                    x: rect.x + 1,
                    y: rect.y + 3 + graph_rows,
                    cols: rect.cols.saturating_sub(2),
                    rows: graph_rows,
                },
                full_len,
                braille,
            );
            crate::monitor::graphs::paint_history_graph(
                &normalized[..full_len],
                RampId::Write,
                full_rect,
                surface,
            );
        }
        if placed.id == SLICE
            && !matches!(self.model.slice_health, Health::Absent(_))
            && rect.rows > 5
            && rect.cols > 2
        {
            let graph_rows = rect.rows.saturating_sub(4) / 2;
            let mut values = [0.0; HISTORY];
            let read_len = self.model.slice_read_history.copy_normalized(&mut values).0;
            let braille = crate::caps::raster_table().braille;
            let graph_rect = crate::monitor::kit::history_rect::graph_rect(
                Rect {
                    x: rect.x + 1,
                    y: rect.y + 3,
                    cols: rect.cols.saturating_sub(2),
                    rows: graph_rows,
                },
                read_len,
                braille,
            );
            crate::monitor::graphs::paint_history_graph(
                &values[..read_len],
                RampId::Read,
                graph_rect,
                surface,
            );
            let write_len = self
                .model
                .slice_write_history
                .copy_normalized(&mut values)
                .0;
            let graph_rect = crate::monitor::kit::history_rect::graph_rect(
                Rect {
                    x: rect.x + 1,
                    y: rect.y + 3 + graph_rows,
                    cols: rect.cols.saturating_sub(2),
                    rows: graph_rows,
                },
                write_len,
                braille,
            );
            crate::monitor::graphs::paint_history_graph(
                &values[..write_len],
                RampId::Write,
                graph_rect,
                surface,
            );
        }
        if placed.id == DEVICES && !matches!(self.model.disk_health, Health::Absent(_)) {
            for (row, device) in self
                .model
                .devices
                .iter()
                .take(rect.rows.saturating_sub(2) / 3)
                .enumerate()
            {
                if let Some(history) = self
                    .model
                    .device_history
                    .iter()
                    .find(|history| history.id == device.id)
                {
                    let mut util = [0.0; HISTORY];
                    let util_len = history.util.copy_ordered(&mut util);
                    if rect.cols > 38 + 8 {
                        crate::monitor::graphs::paint_sparkline(
                            &util[..util_len],
                            8,
                            RampId::Util,
                            Rect {
                                x: rect.x + 38,
                                y: rect.y + row * 3 + 1,
                                cols: 8,
                                rows: 1,
                            },
                            surface,
                        );
                    }
                    if device.physical {
                        let mut read = [0.0; HISTORY];
                        let (read_len, read_peak) = history.read.copy_normalized(&mut read);
                        let mut write = [0.0; HISTORY];
                        let (write_len, write_peak) = history.write.copy_normalized(&mut write);
                        if rect.cols > 39 {
                            let plot = Rect {
                                x: rect.x + 38,
                                y: rect.y + row * 3 + 2,
                                cols: rect.cols.saturating_sub(39),
                                rows: 2,
                            };
                            let mut chart = CellGrid::new(plot.cols, plot.rows);
                            paint_mirrored_history(
                                &mut chart,
                                Rect::full(plot.cols, plot.rows),
                                &read[..read_len],
                                &write[..write_len],
                            );
                            Emitter::default().emit(
                                &chart,
                                Rect::full(plot.cols, plot.rows),
                                surface,
                                plot.x,
                                plot.y,
                            );
                        }
                        let format_peak = |values: &[f32], peak: f32| {
                            if matches!(self.model.disk_health, Health::Warming) {
                                crate::caps::active_glyphs().ellipsis.into()
                            } else if values.iter().any(|value| value.is_finite()) {
                                format!(
                                    "{}/s",
                                    crate::monitor::kit::edge_fmt(NumFmt::default()).fmt(
                                        f64::from(peak),
                                        Unit::Bytes,
                                        5,
                                    )
                                )
                            } else {
                                crate::caps::active_glyphs().emdash.into()
                            }
                        };
                        let read_peak = format_peak(&read[..read_len], read_peak);
                        let write_peak = format_peak(&write[..write_len], write_peak);
                        let peaks = format!("240-sample pk {read_peak} {write_peak}");
                        let label_cols = peaks.chars().count().min(rect.cols.saturating_sub(2));
                        let mut grid = CellGrid::new(label_cols, 1);
                        grid.view(Rect::full(label_cols, 1)).put_str(
                            0,
                            0,
                            &peaks,
                            CellStyle::default(),
                        );
                        Emitter::default().emit(
                            &grid,
                            Rect::full(label_cols, 1),
                            surface,
                            rect.x + 1,
                            rect.y + row * 3 + 2,
                        );
                    }
                }
            }
        }
    }
}

fn paint_io(grid: &mut CellGrid, rect: Rect, model: &DiskModel) {
    {
        let mut body = panel::panel(grid, Rect::full(rect.cols, rect.rows), "I/O", None);
        let area = body.rect();
        if matches!(model.disk_health, Health::Absent(_)) {
            let mark = format!("{} root-only", absent_mark(model.disk_health));
            body.put_str(0, 0, &mark, CellStyle::default());
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
            let count = if model.devices.is_empty() && matches!(model.disk_health, Health::Warming)
            {
                format!("{} physical devices", crate::caps::active_glyphs().ellipsis)
            } else {
                format!("{} physical devices", model.physical_count)
            };
            body.put_str(0, 1, &count, CellStyle::default());
        }
        if area.rows > 2 {
            let read_peak = model.telemetry_read_peak.map_or_else(
                || history_peak(&model.read_history, model.disk_health, area.cols / 2),
                |peak| rate_reading(Some(f64::from(peak)), Health::Ok, area.cols / 2),
            );
            let write_peak = model.telemetry_write_peak.map_or_else(
                || history_peak(&model.write_history, model.disk_health, area.cols / 2),
                |peak| rate_reading(Some(f64::from(peak)), Health::Ok, area.cols / 2),
            );
            let scale = format!("window peak read {read_peak} write {write_peak}");
            body.put_str(0, 2, &scale, CellStyle::default());
            if area.rows > 3 {
                body.put_str(
                    0,
                    3,
                    "device trend scale: each row peak, labeled",
                    CellStyle::default(),
                );
            }
        }
    }
    if !matches!(model.disk_health, Health::Absent(_)) && rect.rows > 5 {
        let mut read = [0.0; HISTORY];
        let read_len = if model.telemetry_read_history.len > 0 {
            model.telemetry_read_history.copy_ordered(&mut read)
        } else {
            model.read_history.copy_normalized(&mut read).0
        };
        let mut write = [0.0; HISTORY];
        let write_len = if model.telemetry_write_history.len > 0 {
            model.telemetry_write_history.copy_ordered(&mut write)
        } else {
            model.write_history.copy_normalized(&mut write).0
        };
        paint_mirrored_history(
            grid,
            crate::monitor::kit::history_rect::graph_rect(
                Rect {
                    x: rect.cols / 2,
                    y: 4,
                    cols: rect.cols.saturating_sub(rect.cols / 2 + 1),
                    rows: rect.rows.saturating_sub(5),
                },
                read_len.max(write_len),
                crate::monitor::graphs::table().braille,
            ),
            &read[..read_len],
            &write[..write_len],
        );
        let device_cols = (rect.cols / 2).saturating_sub(2);
        for (row, device) in model.devices.iter().take(MAX_DEVICE_HISTORIES).enumerate() {
            if let Some(history) = model
                .device_history
                .iter()
                .find(|history| history.id == device.id && device.physical)
            {
                let mut read = [0.0; HISTORY];
                let read_len = history.read.copy_normalized(&mut read).0;
                let mut write = [0.0; HISTORY];
                let write_len = history.write.copy_normalized(&mut write).0;
                paint_mirrored_history(
                    grid,
                    Rect {
                        x: 1,
                        y: 4 + row * 3,
                        cols: device_cols,
                        rows: 2,
                    },
                    &read[..read_len],
                    &write[..write_len],
                );
            }
        }
    }
}

fn paint_mirrored_history(grid: &mut CellGrid, rect: Rect, read: &[f32], write: &[f32]) {
    if rect.cols == 0 || rect.rows == 0 || (read.is_empty() && write.is_empty()) {
        return;
    }
    let table = crate::monitor::graphs::table();
    let used_rect = crate::monitor::kit::history_rect::graph_rect(
        rect,
        read.len().max(write.len()),
        table.braille,
    );
    let columns = used_rect.cols;
    let samples = columns * if table.braille { 2 } else { 1 };
    let read_start = read.len().saturating_sub(samples);
    let write_start = write.len().saturating_sub(samples);
    let mut raster = thegn_core::viz_raster::CellBuf::new();
    raster.reset(columns, rect.rows);
    thegn_core::viz_raster::mirrored(
        &read[read_start..],
        &write[write_start..],
        table,
        &mut raster,
    );
    let mut view = grid.view(used_rect);
    let upper_rows = used_rect.rows.div_ceil(2);
    for y in 0..used_rect.rows {
        let ramp = if y < upper_rows {
            RampId::Read
        } else {
            RampId::Write
        };
        for x in 0..used_rect.cols {
            if let Some(cell) = raster.get(x, y) {
                view.put(
                    x,
                    y,
                    CellStyle {
                        ch: cell.ch,
                        fg: crate::seg::StyleTok::from_tok(crate::seg::Tok::Ramp(ramp, cell.fg))
                            .unwrap_or(crate::seg::StyleTok::INHERIT),
                        ..CellStyle::default()
                    },
                );
            }
        }
    }
}

fn paint_pressure(grid: &mut CellGrid, rect: Rect, model: &DiskModel) {
    let mut body = panel::panel(
        grid,
        Rect::full(rect.cols, rect.rows),
        "I/O pressure 0-100%",
        None,
    );
    if matches!(model.psi_health, Health::Absent(_)) {
        let mark = format!("{} root-only", absent_mark(model.psi_health));
        body.put_str(0, 0, &mark, CellStyle::default());
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
        .map(|line| format!("some {:.2}% (0-100)", f64::from(line.avg10) / 100.0))
        .unwrap_or_else(|| format!("some {}", crate::caps::active_glyphs().emdash));
    let full = model
        .io_psi
        .file
        .full
        .map(|line| format!("full {:.2}% (0-100)", f64::from(line.avg10) / 100.0))
        .unwrap_or_else(|| format!("full {}", crate::caps::active_glyphs().emdash));
    body.put_str(0, 0, &some, CellStyle::default());
    body.put_str(0, 1, &full, CellStyle::default());
}

fn paint_devices(grid: &mut CellGrid, rect: Rect, model: &DiskModel) {
    let mut body = panel::panel(
        grid,
        Rect::full(rect.cols, rect.rows),
        "Devices trends; row peaks labeled",
        None,
    );
    if matches!(model.disk_health, Health::Absent(_)) {
        let mark = format!("{} root-only", absent_mark(model.disk_health));
        body.put_str(0, 0, &mark, CellStyle::default());
        return;
    }
    for (row, device) in model.devices.iter().take(body.rect().rows / 3).enumerate() {
        let text = format!(
            "{:<12} {:>8} {:>8} {:>5}",
            truncate(&device.name, 12),
            optional_rate(device.read_bps, model.disk_health),
            optional_rate(device.write_bps, model.disk_health),
            device
                .util_pct
                .map(|value| value.round() as u32)
                .map_or_else(
                    || if matches!(model.disk_health, Health::Warming) {
                        crate::caps::active_glyphs().ellipsis.into()
                    } else {
                        crate::caps::active_glyphs().emdash.into()
                    },
                    |value| format!("{value}%")
                )
        );
        body.put_str(0, row * 3, &text, CellStyle::default());
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
    let area;
    {
        let mut body = panel::panel(
            grid,
            Rect::full(rect.cols, rect.rows),
            "Filesystems used + trend 0-100%",
            None,
        );
        if matches!(model.fs_health, Health::Absent(_)) {
            body.put_str(0, 0, absent_mark(model.fs_health), CellStyle::default());
            return;
        }
        area = body.rect();
        let visible_rows = area.rows / 2;
        for (row, fs) in model.filesystems.iter().take(visible_rows).enumerate() {
            let used = fs
                .total_bytes
                .zip(fs.free_bytes)
                .map(|(total, free)| total.saturating_sub(free));
            let total = fs
                .total_bytes
                .map(|value| {
                    crate::monitor::kit::edge_fmt(NumFmt::default()).fmt(
                        value as f64,
                        Unit::Bytes,
                        8,
                    )
                })
                .unwrap_or_else(|| crate::caps::active_glyphs().emdash.into());
            let used = used
                .map(|value| {
                    crate::monitor::kit::edge_fmt(NumFmt::default()).fmt(
                        value as f64,
                        Unit::Bytes,
                        8,
                    )
                })
                .unwrap_or_else(|| crate::caps::active_glyphs().emdash.into());
            let mount = truncate(&fs.mount_point, 16);
            let extra_mounts = (fs.extra_mounts > 0).then(|| format!(" +{}", fs.extra_mounts));
            let text = format!(
                "{}{} {} {used}/{total}",
                mount,
                extra_mounts.as_deref().unwrap_or(""),
                fs.fstype
            );
            body.put_str(0, row * 2, &text, CellStyle::default());
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
    let visible_rows = area.rows / 2;
    let mut meter = crate::monitor::kit::meter::Meter::default();
    for (row, fs) in model.filesystems.iter().take(visible_rows).enumerate() {
        if let Some(total) = fs.total_bytes.filter(|total| *total > 0)
            && let Some(free) = fs.free_bytes
        {
            let used_ratio = total.saturating_sub(free).min(total) as f32 / total as f32;
            meter.paint(
                grid,
                Rect {
                    x: area.x,
                    y: area.y + row * 2 + 1,
                    cols: area.cols.saturating_mul(2) / 3,
                    rows: 1,
                },
                crate::monitor::kit::meter::MeterSpec {
                    value: used_ratio,
                    warn: 0.80,
                    crit: 0.95,
                    ramp: RampId::Util,
                    label: true,
                    fmt: NumFmt::default(),
                },
            );
        }
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
            if body.rect().rows > 1 {
                let read_peak = history_peak(&model.slice_read_history, model.slice_health, 16);
                let write_peak = history_peak(&model.slice_write_history, model.slice_health, 16);
                body.put_str(
                    0,
                    1,
                    &format!("rate peak read {read_peak} write {write_peak}"),
                    CellStyle::default(),
                );
            }
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

fn paint_worktrees(
    grid: &mut CellGrid,
    rect: Rect,
    rows: &[crate::monitor::build::DiskWtRow],
    sel: usize,
) {
    let reclaimable = rows
        .iter()
        .fold(0_u64, |total, row| total.saturating_add(row.target_bytes));
    let title = if rows.is_empty() {
        "Worktrees - no cached sizes yet".to_owned()
    } else {
        format!(
            "Worktrees - {} - {} reclaimable",
            rows.len(),
            crate::monitor::kit::edge_fmt(NumFmt::default()).fmt(
                reclaimable as f64,
                Unit::Bytes,
                8,
            )
        )
    };
    let mut body = panel::panel(grid, Rect::full(rect.cols, rect.rows), &title, None);
    let area = body.rect();
    if rows.is_empty() {
        body.put_str(
            0,
            0,
            "no cached sizes yet (or [disk] show_sizes = false)",
            CellStyle::default(),
        );
        return;
    }
    let visible = area.rows.max(1);
    let start = sel.saturating_sub(visible.saturating_sub(1));
    let end = rows.len().min(start.saturating_add(visible));
    for (line, (index, row)) in rows
        .iter()
        .enumerate()
        .skip(start)
        .take(end - start)
        .enumerate()
    {
        let text = format!(
            "{}  {}  target {}  {}",
            truncate(&row.name, 22),
            crate::monitor::kit::edge_fmt(NumFmt::default()).fmt(
                row.total_bytes as f64,
                Unit::Bytes,
                8,
            ),
            crate::monitor::kit::edge_fmt(NumFmt::default()).fmt(
                row.target_bytes as f64,
                Unit::Bytes,
                8,
            ),
            super::fmt_age(row.age_secs),
        );
        let style = if index == sel {
            CellStyle {
                fg: StyleTok::from_tok(crate::seg::Tok::Series(
                    thegn_core::theme::SeriesRole::Selected,
                ))
                .unwrap_or(StyleTok::INHERIT),
                attr: crate::monitor::kit::grid::BOLD,
                ..CellStyle::default()
            }
        } else {
            CellStyle::default()
        };
        body.put_str(0, line, &text, style);
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
    match reading {
        Reading::Value(_) => format!("{formatted}/s"),
        Reading::Unknown | Reading::Warming | Reading::Denied | Reading::NotApplicable => formatted,
    }
}
fn history_peak(history: &HistoryRing, health: Health, width: usize) -> String {
    match history.finite_peak() {
        Some(peak) => rate_reading(Some(f64::from(peak)), health, width),
        _ => rate_reading(None, health, width),
    }
}
fn optional_rate(value: Option<f64>, health: Health) -> String {
    match (health, value) {
        (Health::Warming, _) => crate::caps::active_glyphs().ellipsis.into(),
        (_, Some(value)) => format!(
            "{}/s",
            crate::monitor::kit::edge_fmt(NumFmt::default()).fmt(value, Unit::Bytes, 5)
        ),
        (_, None) => crate::caps::active_glyphs().emdash.into(),
    }
}
fn truncate(value: &str, width: usize) -> String {
    value.chars().take(width).collect()
}

#[cfg(test)]
#[path = "disk_view_tests.rs"]
mod tests;
