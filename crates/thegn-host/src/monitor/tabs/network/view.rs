use crate::telemetry::{Metric, ScaleMode, SeriesBuf, SeriesReq, TelemetryHistory, Window};
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
    model::net_wifi::WifiRow,
    model::netdev::{IfaceSample, ScopeTotal},
    policy::net_scope::Scope,
};

mod paint;

pub(super) const THROUGHPUT: PanelId = PanelId("network.throughput");
pub(super) const INTERFACES: PanelId = PanelId("network.interfaces");
pub(super) const HEALTH: PanelId = PanelId("network.health");
pub(super) const WIFI: PanelId = PanelId("network.wifi");
const HISTORY: usize = 120;
pub(super) const TELEMETRY_HISTORY: usize = 480;
pub(super) const IFACE_CAP: usize = 8;
const IFACE_HISTORY: usize = 32;
const WIFI_CAP: usize = 16;
const WIFI_HISTORY: usize = 120;
const VARIANTS: [Variant; 1] = [Variant {
    kind: VariantKind::Full,
    min: (24, 4),
    pref: (72, 12),
}];
const SPECS: [PanelSpec; 4] = [
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
    PanelSpec {
        id: WIFI,
        priority: 3,
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
pub(super) struct WifiHistory {
    pub(super) ifindex: u32,
    pub(super) sample: WifiRow,
    pub(super) signal: Ring,
    pub(super) live: bool,
}

impl Default for WifiHistory {
    fn default() -> Self {
        Self {
            ifindex: 0,
            sample: WifiRow::default(),
            signal: Ring::new(WIFI_HISTORY),
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
    history_buf: SeriesBuf,
    pub(super) interfaces: Vec<InterfaceHistory>,
    pub(super) wifi: Vec<WifiHistory>,
    pub(super) overflow: usize,
    pub(super) overflow_rx: Option<f64>,
    pub(super) overflow_tx: Option<f64>,
    pub(super) health: Health,
    pub(super) wifi_health: Health,
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
            history_buf: SeriesBuf::default(),
            interfaces: Vec::with_capacity(IFACE_CAP),
            wifi: Vec::with_capacity(WIFI_CAP),
            overflow: 0,
            overflow_rx: None,
            overflow_tx: None,
            health: Health::Warming,
            wifi_health: Health::Warming,
            error_total: None,
            drop_total: None,
        }
    }
}

impl NetworkModel {
    #[cfg(test)]
    fn history_storage_probe(&self) -> (usize, *const f32) {
        (
            self.history_rx.values.capacity(),
            self.history_buf.hi().as_ptr(),
        )
    }

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
            history.series_into(
                &SeriesReq {
                    metric,
                    window,
                    scale: TelemetryHistory::scale_for(metric, scale),
                    buckets: TELEMETRY_HISTORY,
                    agg: thegn_core::series::Agg::Max,
                    now_ms,
                },
                &mut self.history_buf,
            );
            ring.reset();
            *axis_max = self.history_buf.axis_max();
            let values = self.history_buf.hi();
            let raw = self.history_buf.raw_hi();
            let gaps = self.history_buf.gap();
            let first_real = gaps.iter().position(|gap| !gap);
            if let Some(first_real) = first_real {
                let mut held = None;
                for ((value, raw), gap) in values.iter().zip(raw).zip(gaps).skip(first_real) {
                    if *gap {
                        ring.push(held.unwrap_or(f32::NAN));
                    } else {
                        let value = if raw.is_finite() { *value } else { f32::NAN };
                        ring.push(value);
                        if value.is_finite() {
                            held = Some(value);
                        }
                    }
                }
            }
        }
    }

    pub(in crate::monitor) fn absorb_frame(&mut self, frame: &MonitorFrame) {
        let source = &frame.parts.netdev;
        self.health = frame.health[CollectorId::Netdev.index()];
        self.wifi_health = frame.health[CollectorId::NetWifi.index()];
        let wifi = &frame.parts.net_wifi;
        for entry in &mut self.wifi {
            entry.live = false;
        }
        // Keep every existing interface in its slot before reusing dormant slots.
        for sample in wifi.ifaces.iter().take(WIFI_CAP) {
            if let Some(slot) = self
                .wifi
                .iter()
                .position(|entry| entry.ifindex == sample.ifindex)
            {
                store_wifi_sample(&mut self.wifi[slot], sample);
            }
        }
        for sample in wifi.ifaces.iter().take(WIFI_CAP) {
            if self
                .wifi
                .iter()
                .any(|entry| entry.live && entry.ifindex == sample.ifindex)
            {
                continue;
            }
            let slot = self
                .wifi
                .iter()
                .position(|entry| !entry.live)
                .or_else(|| (self.wifi.len() < WIFI_CAP).then_some(self.wifi.len()));
            let Some(slot) = slot else {
                continue;
            };
            if slot == self.wifi.len() {
                self.wifi.push(WifiHistory::default());
            }
            store_wifi_sample(&mut self.wifi[slot], sample);
        }
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

fn store_wifi_sample(entry: &mut WifiHistory, sample: &WifiRow) {
    if entry.ifindex != sample.ifindex {
        entry.signal.reset();
    }
    entry.ifindex = sample.ifindex;
    entry.sample.clone_from(sample);
    entry.live = true;
    entry.signal.push(sample.signal_dbm.map_or(f32::NAN, |dbm| {
        if (-120..=0).contains(&dbm) {
            f32::from(dbm)
        } else {
            f32::NAN
        }
    }));
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
        let mut panels = vec![
            (Size::Weight(2), Node::Panel(THROUGHPUT)),
            (Size::Weight(3), Node::Panel(INTERFACES)),
            (Size::Fixed(5), Node::Panel(HEALTH)),
        ];
        if !matches!(self.0.wifi_health, Health::Absent(_)) {
            panels.push((Size::Weight(2), Node::Panel(WIFI)));
        }
        Node::Col(panels)
    }

    fn demand(&self, panel: PanelId) -> CollectorSet {
        let mut demand = CollectorSet::empty();
        if matches!(panel, THROUGHPUT | INTERFACES | HEALTH)
            && !matches!(self.0.health, Health::Absent(_))
        {
            demand.insert(CollectorId::Netdev);
        }
        if panel == WIFI {
            demand.insert(CollectorId::NetWifi);
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

#[cfg(test)]
mod tests {
    use super::{NetworkModel, Ring};
    use crate::caps;
    use thegn_core::termcaps::UnicodeLevel;
    use crate::telemetry::{ScaleMode, TelemetryHistory, Window};
    use termwiz::surface::Surface;

    #[test]
    fn sparkline_nan_bucket_stays_blank_between_finite_runs() {
        let mut ring = Ring::new(3);
        ring.push(0.5);
        ring.push(f32::NAN);
        ring.push(0.5);
        let mut surface = Surface::new(3, 1);
        caps::test_override::with_unicode(UnicodeLevel::Ascii, || {
            super::paint::paint_interface_spark(&ring, 1.0, 0, 0, 3, &mut surface);
        });
        let screen_lines = surface.screen_lines();
        let cells: Vec<_> = screen_lines[0]
            .visible_cells()
            .map(|cell| cell.str().to_owned())
            .collect();
        assert_eq!(cells.len(), 3);
        assert_ne!(cells[0], " ");
        assert_eq!(cells[1], " ", "NaN bucket must remain blank");
        assert_ne!(cells[2], " ");
    }

    #[test]
    fn telemetry_series_storage_stays_warm_after_one_thousand_rebuilds() {
        let mut history = TelemetryHistory::default();
        for tick in 0..120_u64 {
            history.push(
                &thegn_metrics::StatsSnapshot {
                    net_bps: Some((10_000 + tick * 500, 5_000 + tick * 300)),
                    ..thegn_metrics::StatsSnapshot::default()
                },
                1_000 + tick * 1_000,
            );
        }
        let mut model = NetworkModel::default();
        model.absorb_history(&history, Window::from_secs(120), ScaleMode::Window, 121_000);
        let initial = model.history_storage_probe();
        for _ in 0..1_000 {
            model.absorb_history(&history, Window::from_secs(120), ScaleMode::Window, 121_000);
            assert_eq!(model.history_storage_probe(), initial);
        }
    }
}
