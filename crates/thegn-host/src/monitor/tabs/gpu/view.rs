use crate::{
    compositor::Rect,
    monitor::{
        board::TabView,
        kit::{emit::Emitter, grid::CellGrid, panel::panel},
        layout::{Node, PanelId, PanelSpec, Placed, Size, Variant, VariantKind},
    },
    sections::Section,
};
use termwiz::surface::Surface;
use thegn_metrics::collect::{
    CollectorId, CollectorSet,
    frame::MonitorFrame,
    health::Health,
    model::{
        amd_smu::AmdSmuPart,
        gpu::{GpuDevicesPart, RuntimeStatus},
        gpu_engines::GpuEnginesPart,
    },
};

mod paint;

const DEVICES: PanelId = PanelId("gpu.devices");
const ENGINES: PanelId = PanelId("gpu.engines");
const MEMORY: PanelId = PanelId("gpu.memory");
const CLOCKS: PanelId = PanelId("gpu.clocks");
pub(super) const HISTORY_CAP: usize = 480;
const DEVICE_CAP: usize = 32;
const ACTIVITY_RING_COUNT: usize = 12;
const FULL: [Variant; 1] = [Variant {
    kind: VariantKind::Full,
    min: (34, 5),
    pref: (72, 9),
}];
const SPECS: [PanelSpec; 4] = [
    PanelSpec {
        id: DEVICES,
        priority: 0,
        variants: &FULL,
    },
    PanelSpec {
        id: ENGINES,
        priority: 1,
        variants: &FULL,
    },
    PanelSpec {
        id: MEMORY,
        priority: 2,
        variants: &FULL,
    },
    PanelSpec {
        id: CLOCKS,
        priority: 3,
        variants: &FULL,
    },
];

#[derive(Debug, Clone)]
pub(super) struct HistoryRing {
    pub(super) values: [f32; HISTORY_CAP],
    pub(super) next: usize,
    pub(super) len: usize,
}

impl HistoryRing {
    fn new() -> Self {
        Self {
            values: [0.0; HISTORY_CAP],
            next: 0,
            len: 0,
        }
    }

    fn push(&mut self, value: f32) {
        self.values[self.next] = value;
        self.next = (self.next + 1) % HISTORY_CAP;
        self.len = self.len.saturating_add(1).min(HISTORY_CAP);
    }

    fn push_optional(&mut self, value: Option<f32>) {
        if let Some(value) = value {
            self.push(value);
        } else if !self.is_empty() {
            self.push(f32::NAN);
        }
    }

    pub(super) fn ordered_into(&self, output: &mut [f32; HISTORY_CAP]) -> usize {
        let start = (self.next + HISTORY_CAP - self.len) % HISTORY_CAP;
        for (offset, value) in output.iter_mut().take(self.len).enumerate() {
            *value = self.values[(start + offset) % HISTORY_CAP];
        }
        self.len
    }

    pub(super) fn len(&self) -> usize {
        self.len
    }

    pub(super) fn is_empty(&self) -> bool {
        self.len == 0
    }
}

#[derive(Debug, Clone)]
pub(super) struct BusyHistory {
    pub(super) slot: String,
    pub(super) busy: HistoryRing,
    pub(super) vcn: HistoryRing,
    pub(super) memory: HistoryRing,
    pub(super) gtt: HistoryRing,
    pub(super) clocks: HistoryRing,
    pub(super) power: HistoryRing,
    pub(super) activity: [HistoryRing; ACTIVITY_RING_COUNT],
}

impl BusyHistory {
    fn new(slot: &str) -> Self {
        Self {
            slot: slot.to_owned(),
            busy: HistoryRing::new(),
            vcn: HistoryRing::new(),
            memory: HistoryRing::new(),
            gtt: HistoryRing::new(),
            clocks: HistoryRing::new(),
            power: HistoryRing::new(),
            activity: std::array::from_fn(|_| HistoryRing::new()),
        }
    }
}

#[derive(Debug, Clone)]
pub(in crate::monitor) struct GpuModel {
    pub(super) devices: GpuDevicesPart,
    pub(super) engines: GpuEnginesPart,
    pub(super) smu: AmdSmuPart,
    pub(super) device_health: Health,
    pub(super) engine_health: Health,
    pub(super) smu_health: Health,
    pub(super) histories: Vec<BusyHistory>,
}

impl Default for GpuModel {
    fn default() -> Self {
        Self {
            devices: GpuDevicesPart::default(),
            engines: GpuEnginesPart::default(),
            smu: AmdSmuPart::default(),
            device_health: Health::default(),
            engine_health: Health::default(),
            smu_health: Health::default(),
            histories: Vec::with_capacity(DEVICE_CAP),
        }
    }
}

impl GpuModel {
    pub(crate) fn has_engine_devices(&self) -> bool {
        !matches!(self.engine_health, Health::Absent(_))
    }

    pub(in crate::monitor) fn absorb_frame(&mut self, frame: &MonitorFrame) {
        copy_gpu_devices(
            &mut self.devices.devices,
            &frame.parts.gpu_devices.devices,
            DEVICE_CAP,
        );
        self.devices.accels.clear();
        self.devices.truncated =
            frame.parts.gpu_devices.truncated || frame.parts.gpu_devices.devices.len() > DEVICE_CAP;
        copy_rows(
            &mut self.engines.devices,
            &frame.parts.gpu_engines.devices,
            DEVICE_CAP,
        );
        self.engines.truncated =
            frame.parts.gpu_engines.truncated || frame.parts.gpu_engines.devices.len() > DEVICE_CAP;
        self.smu.cards.clone_from(&frame.parts.amd_smu.cards);
        self.device_health = frame.health[CollectorId::GpuDevices.index()];
        self.engine_health = frame.health[CollectorId::GpuEngines.index()];
        self.smu_health = frame.health[CollectorId::AmdSmu.index()];

        for row in self.engines.devices.iter().take(DEVICE_CAP) {
            let slot = row.slot.as_str();
            let history_index =
                if let Some(index) = self.histories.iter().position(|item| item.slot == slot) {
                    index
                } else if let Some(index) = self.histories.iter().position(|item| {
                    !self
                        .engines
                        .devices
                        .iter()
                        .any(|active| active.slot.as_str() == item.slot)
                }) {
                    self.histories[index] = BusyHistory::new(slot);
                    index
                } else if self.histories.len() < DEVICE_CAP {
                    self.histories.push(BusyHistory::new(slot));
                    self.histories.len() - 1
                } else {
                    continue;
                };
            let history = &mut self.histories[history_index];
            if row.runtime_status == RuntimeStatus::Active {
                history
                    .busy
                    .push_optional(row.gfx_busy_pct.map(|busy| f32::from(busy) / 100.0));
                history
                    .vcn
                    .push_optional(row.vcn_busy_pct.map(|busy| f32::from(busy) / 100.0));
                history.memory.push_optional(
                    row.vram_used
                        .zip(row.vram_total)
                        .filter(|(_, total)| *total > 0)
                        .map(|(used, total)| (used as f64 / total as f64).clamp(0.0, 1.0) as f32),
                );
                history.gtt.push_optional(
                    row.gtt_used
                        .zip(row.gtt_total)
                        .filter(|(_, total)| *total > 0)
                        .map(|(used, total)| (used as f64 / total as f64).clamp(0.0, 1.0) as f32),
                );
                let gfx_max = row
                    .sclk
                    .levels_mhz
                    .iter()
                    .take(usize::from(row.sclk.len).min(row.sclk.levels_mhz.len()))
                    .copied()
                    .max();
                history.clocks.push_optional(
                    row.sclk
                        .current_mhz
                        .zip(gfx_max)
                        .filter(|(_, max)| *max > 0)
                        .map(|(current, max)| (current as f32 / max as f32).clamp(0.0, 1.0)),
                );
                let device_card = self
                    .devices
                    .devices
                    .iter()
                    .find(|device| device.slot == slot)
                    .map(|device| device.card);
                let metrics = device_card.and_then(|card| {
                    self.smu
                        .cards
                        .iter()
                        .find(|reading| u32::from(reading.card) == card && !reading.suspended)
                        .map(|reading| &reading.metrics)
                });
                let mut activities = [None; ACTIVITY_RING_COUNT];
                if let Some(metrics) = metrics.filter(|metrics| metrics.layout_verified) {
                    activities[..4].copy_from_slice(&[
                        metrics.activity_gfx,
                        metrics.activity_mm,
                        metrics.activity_vcn,
                        metrics.activity_umc,
                    ]);
                    activities[4..].copy_from_slice(&metrics.activity_ipu);
                }
                for (ring, value) in history.activity.iter_mut().zip(activities) {
                    ring.push_optional(
                        value
                            .filter(|value| *value <= 100)
                            .map(|value| f32::from(value) / 100.0),
                    );
                }
                history.power.push_optional(metrics.and_then(|metrics| {
                    metrics
                        .power_socket
                        .zip(
                            metrics
                                .power_current_stapm_limit
                                .filter(|watts| *watts > 0 && *watts != u32::MAX),
                        )
                        .map(|(watts_mw, cap_w)| {
                            (watts_mw as f32 / (cap_w as f32 * 1000.0)).clamp(0.0, 1.0)
                        })
                }));
            }
        }
    }
}

fn copy_rows<T: Clone>(destination: &mut Vec<T>, source: &[T], cap: usize) {
    let count = source.len().min(cap);
    for (index, source_row) in source.iter().take(count).enumerate() {
        if let Some(destination_row) = destination.get_mut(index) {
            destination_row.clone_from(source_row);
        } else {
            destination.push(source_row.clone());
        }
    }
    destination.truncate(count);
}

fn copy_gpu_devices(
    destination: &mut Vec<thegn_metrics::collect::model::gpu::GpuDevice>,
    source: &[thegn_metrics::collect::model::gpu::GpuDevice],
    cap: usize,
) {
    let count = source.len().min(cap);
    for (index, source_row) in source.iter().take(count).enumerate() {
        if let Some(destination_row) = destination.get_mut(index) {
            destination_row.card = source_row.card;
            destination_row.kind = source_row.kind;
            destination_row.ip_version = source_row.ip_version;
            destination_row.runtime_status = source_row.runtime_status;
            destination_row.vram_total = source_row.vram_total;
            destination_row.gtt_total = source_row.gtt_total;
            destination_row.boot_vga = source_row.boot_vga;
            destination_row.slot.clone_from(&source_row.slot);
            clone_optional_string(&mut destination_row.driver, &source_row.driver);
            clone_optional_string(&mut destination_row.pci_id, &source_row.pci_id);
            clone_optional_string(&mut destination_row.family, &source_row.family);
        } else {
            destination.push(source_row.clone());
        }
    }
    destination.truncate(count);
}

fn clone_optional_string(destination: &mut Option<String>, source: &Option<String>) {
    match source {
        Some(source) => match destination {
            Some(destination) => destination.clone_from(source),
            None => *destination = Some(source.clone()),
        },
        None => *destination = None,
    }
}

pub(in crate::monitor) struct GpuView<'a>(pub &'a GpuModel);

impl TabView for GpuView<'_> {
    fn specs(&self) -> &'static [PanelSpec] {
        &SPECS
    }

    fn template(&self) -> Node {
        Node::Col(vec![
            (Size::Weight(1), Node::Panel(DEVICES)),
            (Size::Weight(3), Node::Panel(ENGINES)),
            (
                Size::Weight(2),
                Node::Row(vec![
                    (Size::Weight(1), Node::Panel(MEMORY)),
                    (Size::Weight(1), Node::Panel(CLOCKS)),
                ]),
            ),
        ])
    }

    fn demand(&self, panel: PanelId) -> CollectorSet {
        let mut demand = CollectorSet::empty();
        match panel {
            DEVICES => {
                demand.insert(CollectorId::GpuDevices);
            }
            ENGINES => {
                demand.insert(CollectorId::GpuEngines);
                demand.insert(CollectorId::AmdSmu);
                demand.insert(CollectorId::GpuDevices);
            }
            MEMORY => {
                demand.insert(CollectorId::GpuDevices);
                demand.insert(CollectorId::GpuEngines);
            }
            CLOCKS => {
                demand.insert(CollectorId::GpuEngines);
                demand.insert(CollectorId::GpuDevices);
                demand.insert(CollectorId::AmdSmu);
            }
            _ => {}
        }
        demand
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
        let title = match placed.id {
            DEVICES => "GPU devices",
            ENGINES => "GPU engines  busy % (0-100)",
            MEMORY => "GPU memory",
            CLOCKS => "GPU clocks / power / thermal",
            _ => return,
        };
        let body = panel(&mut grid, Rect::full(rect.cols, rect.rows), title, None).rect();
        match placed.id {
            DEVICES => self.paint_devices(&mut grid, body),
            ENGINES => self.paint_engines(&mut grid, body),
            MEMORY => self.paint_memory(&mut grid, body),
            CLOCKS => self.paint_clocks(&mut grid, body),
            _ => {}
        }
        Emitter::default().emit(
            &grid,
            Rect::full(rect.cols, rect.rows),
            surface,
            rect.x,
            rect.y,
        );
        if placed.id == ENGINES {
            self.paint_busy_history(surface, rect, body);
        } else if placed.id == MEMORY {
            self.paint_aux_history(surface, rect, body, true);
        } else if placed.id == CLOCKS {
            self.paint_aux_history(surface, rect, body, false);
        }
    }
}

impl GpuView<'_> {
    fn device(&self, slot: &str) -> Option<&thegn_metrics::collect::model::gpu::GpuDevice> {
        self.0
            .devices
            .devices
            .iter()
            .find(|device| device.slot == slot)
    }
}
