//! Bounded TCP socket summary and passive round-trip metrics.
use super::super::frame::Part;
use super::net_attrib::NetAttribution;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct NetSocketsPart {
    /// Linux TCP states indexed by the one-based `TCP_*` state number.
    pub states: [u32; 12],
    pub listening: u32,
    pub rtt_p50_us: Option<u32>,
    pub rtt_p90_us: Option<u32>,
    pub rtt_max_us: Option<u32>,
    pub rtt_hist: [u32; 8],
    pub sockets: u32,
    pub malformed: u32,
    pub truncated: bool,
    pub attribution: NetAttribution,
    pub attribution_interval_ms: Option<u32>,
    pub physical_known: bool,
}

impl Clone for NetSocketsPart {
    fn clone(&self) -> Self {
        let mut cloned = Self::default();
        cloned.clone_from(self);
        cloned
    }

    fn clone_from(&mut self, source: &Self) {
        self.states = source.states;
        self.listening = source.listening;
        self.rtt_p50_us = source.rtt_p50_us;
        self.rtt_p90_us = source.rtt_p90_us;
        self.rtt_max_us = source.rtt_max_us;
        self.rtt_hist = source.rtt_hist;
        self.sockets = source.sockets;
        self.malformed = source.malformed;
        self.truncated = source.truncated;
        self.attribution
            .owners
            .clone_from(&source.attribution.owners);
        self.attribution.unattributed_rx = source.attribution.unattributed_rx;
        self.attribution.unattributed_tx = source.attribution.unattributed_tx;
        self.attribution.truncated = source.attribution.truncated;
        self.attribution_interval_ms = source.attribution_interval_ms;
        self.physical_known = source.physical_known;
    }
}

impl Part for NetSocketsPart {
    fn clear(&mut self) {
        self.states = [0; 12];
        self.listening = 0;
        self.rtt_p50_us = None;
        self.rtt_p90_us = None;
        self.rtt_max_us = None;
        self.rtt_hist = [0; 8];
        self.sockets = 0;
        self.malformed = 0;
        self.truncated = false;
        self.attribution.owners.clear();
        self.attribution.unattributed_rx = 0;
        self.attribution.unattributed_tx = 0;
        self.attribution.truncated = false;
        self.attribution_interval_ms = None;
        self.physical_known = false;
    }
}

#[cfg(test)]
mod tests {
    use super::NetSocketsPart;
    use crate::collect::frame::Part;
    use crate::collect::model::net_attrib::{NetAttribution, OwnerRates};
    use crate::collect::policy::cgroup_classify::OwnerKind;
    use std::sync::Arc;

    #[test]
    fn clear_resets_part_and_preserves_owner_capacity() {
        let owner = OwnerRates {
            cgroup_id: 7,
            path: Arc::from(&b"/user.slice/app.slice"[..]),
            kind: OwnerKind::App,
            name: Arc::from(&b"app"[..]),
            rx_bytes: 10,
            tx_bytes: 20,
        };
        let mut part = NetSocketsPart {
            attribution: NetAttribution {
                owners: vec![owner],
                ..Default::default()
            },
            attribution_interval_ms: Some(5_000),
            physical_known: true,
            ..Default::default()
        };
        let capacity = part.attribution.owners.capacity();
        part.clear();
        assert_eq!(part, NetSocketsPart::default());
        assert_eq!(part.attribution.owners.capacity(), capacity);
    }

    #[test]
    fn clone_from_reuses_owner_capacity() {
        let owner = OwnerRates {
            cgroup_id: 7,
            path: Arc::from(&b"/user.slice/app.slice"[..]),
            kind: OwnerKind::App,
            name: Arc::from(&b"app"[..]),
            rx_bytes: 10,
            tx_bytes: 20,
        };
        let source = NetSocketsPart {
            attribution: NetAttribution {
                owners: vec![owner],
                ..Default::default()
            },
            ..Default::default()
        };
        let mut destination = NetSocketsPart::default();
        destination.attribution.owners.reserve(8);
        let capacity = destination.attribution.owners.capacity();
        destination.clone_from(&source);
        assert_eq!(destination, source);
        assert_eq!(destination.attribution.owners.capacity(), capacity);
    }
}
