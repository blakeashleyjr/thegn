#![allow(dead_code)] // wiring waits for V-DSK-2's worktree panel and clean action

use crate::telemetry::{Metric, ScaleMode, SeriesReq, TelemetryHistory, Window};

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
const MAX_DEVICE_HISTORIES: usize = 8;

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
    #[cfg(test)]
    telemetry_rebuilds: usize,
    psi_some_history: HistoryRing,
    psi_full_history: HistoryRing,
    slice_read_history: HistoryRing,
    slice_write_history: HistoryRing,
    last_rbytes: Option<u64>,
    last_wbytes: Option<u64>,
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
            let series = history.series(&SeriesReq {
                metric,
                window,
                scale: TelemetryHistory::scale_for(metric, scale),
                buckets: HISTORY,
                agg: thegn_core::series::Agg::Max,
                now_ms,
            });
            if series.empty {
                slot.replace_from(&[]);
            } else {
                slot.replace_from(&series.hi);
            }
        }
        self.telemetry_cache = Some(cache_key);
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
        if placed.id == PRESSURE
            && !matches!(self.0.psi_health, Health::Absent(_))
            && rect.rows > 5
            && rect.cols > 2
        {
            let graph_rows = rect.rows.saturating_sub(4) / 2;
            let mut normalized = [0.0; HISTORY];
            let some_len = self.0.psi_some_history.copy_ordered(&mut normalized);
            let braille = crate::caps::raster_table().braille;
            let some_cols = history_graph_columns(some_len, rect.cols.saturating_sub(2), braille);
            let some_samples = some_cols * if braille { 2 } else { 1 };
            crate::monitor::graphs::paint_history_graph(
                &normalized[some_len.saturating_sub(some_samples)..some_len],
                RampId::Read,
                Rect {
                    x: rect.x + rect.cols.saturating_sub(some_cols + 1),
                    y: rect.y + 3,
                    cols: some_cols,
                    rows: graph_rows,
                },
                surface,
            );
            let full_len = self.0.psi_full_history.copy_ordered(&mut normalized);
            let full_cols = history_graph_columns(full_len, rect.cols.saturating_sub(2), braille);
            let full_samples = full_cols * if braille { 2 } else { 1 };
            crate::monitor::graphs::paint_history_graph(
                &normalized[full_len.saturating_sub(full_samples)..full_len],
                RampId::Write,
                Rect {
                    x: rect.x + rect.cols.saturating_sub(full_cols + 1),
                    y: rect.y + 3 + graph_rows,
                    cols: full_cols,
                    rows: graph_rows,
                },
                surface,
            );
        }
        if placed.id == SLICE
            && !matches!(self.0.slice_health, Health::Absent(_))
            && rect.rows > 5
            && rect.cols > 2
        {
            let graph_rows = rect.rows.saturating_sub(4) / 2;
            let mut values = [0.0; HISTORY];
            let read_len = self.0.slice_read_history.copy_normalized(&mut values).0;
            let braille = crate::caps::raster_table().braille;
            let cols = history_graph_columns(read_len, rect.cols.saturating_sub(2), braille);
            let samples = cols * if braille { 2 } else { 1 };
            crate::monitor::graphs::paint_history_graph(
                &values[read_len.saturating_sub(samples)..read_len],
                RampId::Read,
                Rect {
                    x: rect.x + rect.cols.saturating_sub(cols + 1),
                    y: rect.y + 3,
                    cols,
                    rows: graph_rows,
                },
                surface,
            );
            let write_len = self.0.slice_write_history.copy_normalized(&mut values).0;
            let cols = history_graph_columns(write_len, rect.cols.saturating_sub(2), braille);
            let samples = cols * if braille { 2 } else { 1 };
            crate::monitor::graphs::paint_history_graph(
                &values[write_len.saturating_sub(samples)..write_len],
                RampId::Write,
                Rect {
                    x: rect.x + rect.cols.saturating_sub(cols + 1),
                    y: rect.y + 3 + graph_rows,
                    cols,
                    rows: graph_rows,
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
                    let spark_x = rect.x + 38;
                    let spark_cols = rect.x.saturating_add(rect.cols).saturating_sub(spark_x);
                    if spark_cols >= 8 {
                        let mut read = [0.0; HISTORY];
                        let read_len = history.read.copy_normalized(&mut read).0;
                        crate::monitor::graphs::paint_sparkline(
                            &read[..read_len],
                            8,
                            RampId::Read,
                            Rect {
                                x: spark_x,
                                y: rect.y + row + 1,
                                cols: spark_cols.min(8),
                                rows: 1,
                            },
                            surface,
                        );
                        if spark_cols > 10 {
                            let mut write = [0.0; HISTORY];
                            let write_len = history.write.copy_normalized(&mut write).0;
                            crate::monitor::graphs::paint_sparkline(
                                &write[..write_len],
                                8,
                                RampId::Write,
                                Rect {
                                    x: spark_x + 10,
                                    y: rect.y + row + 1,
                                    cols: spark_cols.saturating_sub(10).min(8),
                                    rows: 1,
                                },
                                surface,
                            );
                        }
                        if spark_cols > 20 {
                            let mut util = [0.0; HISTORY];
                            let util_len = history.util.copy_ordered(&mut util);
                            crate::monitor::graphs::paint_sparkline(
                                &util[..util_len],
                                8,
                                RampId::Util,
                                Rect {
                                    x: spark_x + 20,
                                    y: rect.y + row + 1,
                                    cols: spark_cols.saturating_sub(20).min(8),
                                    rows: 1,
                                },
                                surface,
                            );
                        }
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
            let scale =
                if model.telemetry_read_history.len > 0 || model.telemetry_write_history.len > 0 {
                    "peak=100% telemetry scale".to_owned()
                } else {
                    format!(
                        "peak {}{}  {}{}",
                        crate::caps::active_glyphs().arrow_up,
                        history_peak(&model.read_history, model.disk_health, area.cols / 2),
                        crate::caps::active_glyphs().arrow_down,
                        history_peak(&model.write_history, model.disk_health, area.cols / 2)
                    )
                };
            body.put_str(0, 2, &scale, CellStyle::default());
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
            history_plot_rect(
                Rect {
                    x: 1,
                    y: 4,
                    cols: rect.cols.saturating_sub(2),
                    rows: rect.rows.saturating_sub(5),
                },
                read_len.max(write_len),
                crate::caps::raster_table().braille,
            ),
            &read[..read_len],
            &write[..write_len],
        );
    }
}

fn paint_mirrored_history(grid: &mut CellGrid, rect: Rect, read: &[f32], write: &[f32]) {
    if rect.cols == 0 || rect.rows == 0 || (read.is_empty() && write.is_empty()) {
        return;
    }
    let table = crate::caps::raster_table();
    let columns = history_columns(read.len().max(write.len()), rect.cols, table.braille);
    let samples = columns * if table.braille { 2 } else { 1 };
    let read_start = read.len().saturating_sub(samples);
    let write_start = write.len().saturating_sub(samples);
    let used_rect = Rect {
        x: rect.x + rect.cols - columns,
        y: rect.y,
        cols: columns,
        rows: rect.rows,
    };
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

fn history_graph_columns(len: usize, max_cols: usize, braille: bool) -> usize {
    (if braille { len.div_ceil(2) } else { len }).min(max_cols)
}

fn history_columns(len: usize, max_cols: usize, braille: bool) -> usize {
    history_graph_columns(len, max_cols, braille)
}

fn history_plot_rect(rect: Rect, len: usize, braille: bool) -> Rect {
    let cols = history_graph_columns(len, rect.cols, braille);
    Rect {
        x: rect.x + rect.cols.saturating_sub(cols),
        y: rect.y,
        cols,
        rows: rect.rows,
    }
}

fn paint_pressure(grid: &mut CellGrid, rect: Rect, model: &DiskModel) {
    let mut body = panel::panel(grid, Rect::full(rect.cols, rect.rows), "I/O pressure", None);
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
    let mut body = panel::panel(grid, Rect::full(rect.cols, rect.rows), "Devices", None);
    if matches!(model.disk_health, Health::Absent(_)) {
        let mark = format!("{} root-only", absent_mark(model.disk_health));
        body.put_str(0, 0, &mark, CellStyle::default());
        return;
    }
    for (row, device) in model.devices.iter().take(body.rect().rows).enumerate() {
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
    let area;
    {
        let mut body = panel::panel(grid, Rect::full(rect.cols, rect.rows), "Filesystems", None);
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
            let text = format!(
                "{} {} {used}/{total}",
                truncate(&fs.mount_point, 16),
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
                    cols: area.cols,
                    rows: 1,
                },
                crate::monitor::kit::meter::MeterSpec {
                    value: used_ratio,
                    warn: 0.80,
                    crit: 0.95,
                    ramp: RampId::Fill,
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
                body.put_str(0, 1, "rates B/s per dt, peak=100%", CellStyle::default());
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
    match history
        .values
        .iter()
        .copied()
        .filter(|value| value.is_finite())
        .reduce(f32::max)
    {
        Some(peak) if history.len > 0 => rate_reading(Some(f64::from(peak)), health, width),
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
