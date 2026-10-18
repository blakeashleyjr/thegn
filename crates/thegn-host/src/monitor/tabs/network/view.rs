use crate::telemetry::{Metric, ScaleMode, SeriesReq, TelemetryHistory, Window};
use crate::{
    compositor::Rect,
    monitor::{
        board::TabView,
        layout::{Node, PanelId, PanelSpec, Placed, Size, Variant, VariantKind},
    },
    sections::Section,
};
use termwiz::surface::Surface;
use thegn_metrics::collect::{
    CollectorId, CollectorSet,
    frame::MonitorFrame,
    health::Health,
    instances::InstanceId,
    model::netdev::{IfaceSample, ScopeTotal},
    policy::net_scope::Scope,
};

mod paint;

pub(super) const THROUGHPUT: PanelId = PanelId("network.throughput");
pub(super) const INTERFACES: PanelId = PanelId("network.interfaces");
pub(super) const HEALTH: PanelId = PanelId("network.health");
const HISTORY: usize = 120;
pub(super) const TELEMETRY_HISTORY: usize = 480;
pub(super) const IFACE_CAP: usize = 8;
const IFACE_HISTORY: usize = 32;
const VARIANTS: [Variant; 1] = [Variant {
    kind: VariantKind::Full,
    min: (24, 4),
    pref: (72, 12),
}];
const SPECS: [PanelSpec; 3] = [
    PanelSpec {
        id: THROUGHPUT,
        priority: 0,
        variants: &VARIANTS,
    },
    PanelSpec {
        id: INTERFACES,
        priority: 1,
        variants: &VARIANTS,
    },
    PanelSpec {
        id: HEALTH,
        priority: 2,
        variants: &VARIANTS,
    },
];

#[derive(Debug, Clone)]
pub(super) struct Ring {
    pub(super) values: Vec<f32>,
    pub(super) head: usize,
    pub(super) len: usize,
    capacity: usize,
}

impl Ring {
    fn new(capacity: usize) -> Self {
        Self {
            values: Vec::with_capacity(capacity),
            head: 0,
            len: 0,
            capacity,
        }
    }

    fn push(&mut self, value: f32) {
        if self.len < self.capacity {
            self.values.push(value);
            self.len += 1;
        } else if !self.values.is_empty() {
            self.values[self.head] = value;
            self.head = (self.head + 1) % self.values.len();
        }
    }

    pub(super) fn copy_ordered(&self, out: &mut [f32]) -> usize {
        let len = self.len.min(out.len());
        let start = if self.len == self.capacity {
            self.head
        } else {
            0
        };
        for (index, slot) in out.iter_mut().take(len).enumerate() {
            *slot = self.values[(start + index) % self.len];
        }
        len
    }

    fn reset(&mut self) {
        self.values.clear();
        self.head = 0;
        self.len = 0;
    }

    fn has_finite_sample(&self) -> bool {
        self.values.iter().any(|value| value.is_finite())
    }
}

#[derive(Debug, Clone)]
pub(super) struct InterfaceHistory {
    pub(super) id: InstanceId,
    pub(super) sample: IfaceSample,
    pub(super) rx: Ring,
    pub(super) tx: Ring,
    pub(super) live: bool,
}

impl Default for InterfaceHistory {
    fn default() -> Self {
        Self {
            id: InstanceId::OTHER,
            sample: IfaceSample::default(),
            rx: Ring::new(IFACE_HISTORY),
            tx: Ring::new(IFACE_HISTORY),
            live: false,
        }
    }
}

#[derive(Debug, Clone)]
pub(in crate::monitor) struct NetworkModel {
    pub(super) total: ScopeTotal,
    pub(super) history_rx: Ring,
    pub(super) history_tx: Ring,
    pub(super) history_rx_axis_max: f32,
    pub(super) history_tx_axis_max: f32,
    pub(super) history_scale: ScaleMode,
    pub(super) error_history: Ring,
    pub(super) drop_history: Ring,
    pub(super) interfaces: Vec<InterfaceHistory>,
    pub(super) overflow: usize,
    pub(super) overflow_rx: Option<f64>,
    pub(super) overflow_tx: Option<f64>,
    pub(super) health: Health,
    pub(super) error_total: Option<f64>,
    pub(super) drop_total: Option<f64>,
}

impl Default for NetworkModel {
    fn default() -> Self {
        Self {
            total: ScopeTotal::default(),
            history_rx: Ring::new(TELEMETRY_HISTORY),
            history_tx: Ring::new(TELEMETRY_HISTORY),
            history_rx_axis_max: 0.0,
            history_tx_axis_max: 0.0,
            history_scale: ScaleMode::Window,
            error_history: Ring::new(HISTORY),
            drop_history: Ring::new(HISTORY),
            interfaces: Vec::with_capacity(IFACE_CAP),
            overflow: 0,
            overflow_rx: None,
            overflow_tx: None,
            health: Health::Warming,
            error_total: None,
            drop_total: None,
        }
    }
}

impl NetworkModel {
    pub(in crate::monitor) fn absorb_history(
        &mut self,
        history: &TelemetryHistory,
        window: Window,
        scale: ScaleMode,
        now_ms: u64,
    ) {
        self.history_scale = scale;
        for (metric, ring, axis_max) in [
            (
                Metric::NetRx,
                &mut self.history_rx,
                &mut self.history_rx_axis_max,
            ),
            (
                Metric::NetTx,
                &mut self.history_tx,
                &mut self.history_tx_axis_max,
            ),
        ] {
            let series = history.series(&SeriesReq {
                metric,
                window,
                scale: TelemetryHistory::scale_for(metric, scale),
                buckets: TELEMETRY_HISTORY,
                agg: thegn_core::series::Agg::Max,
                now_ms,
            });
            ring.reset();
            *axis_max = series.axis_max;
            let first_real = series.gap.iter().position(|gap| !gap);
            if let Some(first_real) = first_real {
                let mut held = None;
                for (value, gap) in series.hi.iter().zip(series.gap.iter()).skip(first_real) {
                    if *gap {
                        ring.push(held.unwrap_or(f32::NAN));
                    } else {
                        ring.push(*value);
                        if value.is_finite() {
                            held = Some(*value);
                        }
                    }
                }
            }
        }
    }

    pub(in crate::monitor) fn absorb_frame(&mut self, frame: &MonitorFrame) {
        let source = &frame.parts.netdev;
        self.health = frame.health[CollectorId::Netdev.index()];
        self.total = source.physical_or_all();
        self.overflow = 0;
        self.overflow_rx = Some(0.0);
        self.overflow_tx = Some(0.0);
        for history in &mut self.interfaces {
            history.live = source.ifaces.iter().any(|sample| sample.id == history.id);
        }
        for sample in &source.ifaces {
            let slot = self
                .interfaces
                .iter()
                .position(|entry| entry.id == sample.id)
                .or_else(|| (self.interfaces.len() < IFACE_CAP).then_some(self.interfaces.len()))
                .or_else(|| self.interfaces.iter().position(|entry| !entry.live));
            let Some(slot) = slot else {
                self.overflow += 1;
                add(&mut self.overflow_rx, sample.rx_bps);
                add(&mut self.overflow_tx, sample.tx_bps);
                continue;
            };
            if slot == self.interfaces.len() {
                self.interfaces.push(InterfaceHistory::default());
            }
            let history = &mut self.interfaces[slot];
            if history.id != sample.id {
                history.rx.reset();
                history.tx.reset();
            }
            history.id = sample.id;
            history.sample.clone_from(sample);
            history.live = true;
            push_option(&mut history.rx, sample.rx_bps);
            push_option(&mut history.tx, sample.tx_bps);
        }
        self.error_total = sum_rates(
            source
                .ifaces
                .iter()
                .filter(|iface| {
                    iface.scope == Scope::Physical
                        || source
                            .ifaces
                            .iter()
                            .all(|other| other.scope != Scope::Physical)
                })
                .map(|iface| [iface.rx_errs_ps, iface.tx_errs_ps]),
        );
        self.drop_total = sum_rates(
            source
                .ifaces
                .iter()
                .filter(|iface| {
                    iface.scope == Scope::Physical
                        || source
                            .ifaces
                            .iter()
                            .all(|other| other.scope != Scope::Physical)
                })
                .map(|iface| [iface.rx_drop_ps, iface.tx_drop_ps]),
        );
        push_option(&mut self.error_history, self.error_total);
        push_option(&mut self.drop_history, self.drop_total);
    }
}

fn push_option(ring: &mut Ring, value: Option<f64>) {
    ring.push(value.map_or(f32::NAN, |rate| rate as f32));
}

fn add(total: &mut Option<f64>, value: Option<f64>) {
    *total = total.zip(value).map(|(sum, value)| sum + value);
}

fn sum_rates(values: impl Iterator<Item = [Option<f64>; 2]>) -> Option<f64> {
    let mut total = 0.0;
    let mut found = false;
    for pair in values {
        for value in pair.into_iter().flatten() {
            found = true;
            total += value;
        }
    }
    found.then_some(total)
}

pub(in crate::monitor) struct NetworkView<'a>(pub &'a NetworkModel);

impl TabView for NetworkView<'_> {
    fn specs(&self) -> &'static [PanelSpec] {
        &SPECS
    }

    fn template(&self) -> Node {
        Node::Col(vec![
            (Size::Weight(2), Node::Panel(THROUGHPUT)),
            (Size::Weight(3), Node::Panel(INTERFACES)),
            (Size::Fixed(5), Node::Panel(HEALTH)),
        ])
    }

    fn demand(&self, panel: PanelId) -> CollectorSet {
        let mut demand = CollectorSet::empty();
        if matches!(panel, THROUGHPUT | INTERFACES | HEALTH)
            && !matches!(self.0.health, Health::Absent(_))
        {
            demand.insert(CollectorId::Netdev);
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
        paint::paint(self.0, surface, placed.id, rect);
    }
}
