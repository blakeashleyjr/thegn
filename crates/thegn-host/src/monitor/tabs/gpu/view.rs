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
const HISTORY_CAP: usize = 480;
const DEVICE_CAP: usize = 32;
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
pub(super) struct BusyHistory {
    pub(super) slot: String,
    pub(super) values: [f32; HISTORY_CAP],
    pub(super) next: usize,
    pub(super) len: usize,
    pub(super) vcn_values: [f32; HISTORY_CAP],
    pub(super) vcn_next: usize,
    pub(super) vcn_len: usize,
}

impl BusyHistory {
    fn new(slot: &str) -> Self {
        Self {
            slot: slot.to_owned(),
            values: [0.0; HISTORY_CAP],
            next: 0,
            len: 0,
            vcn_values: [0.0; HISTORY_CAP],
            vcn_next: 0,
            vcn_len: 0,
        }
    }

    fn push(&mut self, value: f32) {
        self.values[self.next] = value;
        self.next = (self.next + 1) % HISTORY_CAP;
        self.len = self.len.saturating_add(1).min(HISTORY_CAP);
    }

    fn ordered_into(&self, output: &mut [f32; HISTORY_CAP]) -> usize {
        let start = (self.next + HISTORY_CAP - self.len) % HISTORY_CAP;
        for (offset, value) in output.iter_mut().take(self.len).enumerate() {
            *value = self.values[(start + offset) % HISTORY_CAP];
        }
        self.len
    }

    fn push_vcn(&mut self, value: f32) {
        self.vcn_values[self.vcn_next] = value;
        self.vcn_next = (self.vcn_next + 1) % HISTORY_CAP;
        self.vcn_len = self.vcn_len.saturating_add(1).min(HISTORY_CAP);
    }

    fn ordered_vcn_into(&self, output: &mut [f32; HISTORY_CAP]) -> usize {
        let start = (self.vcn_next + HISTORY_CAP - self.vcn_len) % HISTORY_CAP;
        for (offset, value) in output.iter_mut().take(self.vcn_len).enumerate() {
            *value = self.vcn_values[(start + offset) % HISTORY_CAP];
        }
        self.vcn_len
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
    pub(in crate::monitor) fn absorb_frame(&mut self, frame: &MonitorFrame) {
        self.devices.devices.clear();
        self.devices.devices.extend_from_slice(
            &frame.parts.gpu_devices.devices
                [..frame.parts.gpu_devices.devices.len().min(DEVICE_CAP)],
        );
        self.devices.accels.clear();
        self.devices.truncated =
            frame.parts.gpu_devices.truncated || frame.parts.gpu_devices.devices.len() > DEVICE_CAP;
        self.engines.devices.clear();
        self.engines.devices.extend_from_slice(
            &frame.parts.gpu_engines.devices
                [..frame.parts.gpu_engines.devices.len().min(DEVICE_CAP)],
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
            if let Some(busy) = row
                .gfx_busy_pct
                .filter(|_| row.runtime_status == RuntimeStatus::Active)
            {
                history.push(f32::from(busy) / 100.0);
            }
            if let Some(busy) = row
                .vcn_busy_pct
                .filter(|_| row.runtime_status == RuntimeStatus::Active)
            {
                history.push_vcn(f32::from(busy) / 100.0);
            }
        }
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
            }
            MEMORY => {
                demand.insert(CollectorId::GpuDevices);
                demand.insert(CollectorId::GpuEngines);
            }
            CLOCKS => {
                demand.insert(CollectorId::GpuEngines);
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
            ENGINES => "GPU engines",
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
