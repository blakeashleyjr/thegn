//! Bounded TCP socket summary and passive round-trip metrics.
use super::super::frame::Part;

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
}

impl Part for NetSocketsPart {
    fn clear(&mut self) {
        *self = Self::default();
    }
}
