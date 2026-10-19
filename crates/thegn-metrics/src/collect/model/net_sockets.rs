//! Bounded TCP socket summary and passive round-trip metrics.
use super::super::frame::Part;
use super::net_attrib::NetAttribution;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
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
}
