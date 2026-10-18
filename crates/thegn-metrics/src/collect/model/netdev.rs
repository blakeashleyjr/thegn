//! Bounded interface rows and aggregation totals.
use super::super::{
    frame::Part,
    instances::InstanceId,
    parse::netdev::NetdevCounters,
    policy::net_scope::{IfaceKind, Scope},
};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IfaceName {
    bytes: [u8; 16],
    len: u8,
}

impl IfaceName {
    pub fn new(name: &[u8]) -> Self {
        let mut result = Self::default();
        let len = name.len().min(15);
        result.bytes[..len].copy_from_slice(&name[..len]);
        result.len = u8::try_from(len).unwrap_or(15);
        result
    }
    pub fn as_str(&self) -> &str {
        std::str::from_utf8(&self.bytes[..usize::from(self.len)]).unwrap_or("")
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct IfaceSample {
    pub id: InstanceId,
    pub name: IfaceName,
    pub kind: IfaceKind,
    pub scope: Scope,
    pub counters: NetdevCounters,
    pub rx_bps: Option<f64>,
    pub tx_bps: Option<f64>,
    pub rx_pps: Option<f64>,
    pub tx_pps: Option<f64>,
    pub rx_errs_ps: Option<f64>,
    pub tx_errs_ps: Option<f64>,
    pub rx_drop_ps: Option<f64>,
    pub tx_drop_ps: Option<f64>,
}

impl Default for IfaceSample {
    fn default() -> Self {
        Self {
            id: InstanceId::OTHER,
            name: IfaceName::default(),
            kind: IfaceKind::Unknown,
            scope: Scope::Virtual,
            counters: NetdevCounters::default(),
            rx_bps: None,
            tx_bps: None,
            rx_pps: None,
            tx_pps: None,
            rx_errs_ps: None,
            tx_errs_ps: None,
            rx_drop_ps: None,
            tx_drop_ps: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ScopeTotal {
    pub rx_bps: Option<f64>,
    pub tx_bps: Option<f64>,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct NetdevPart {
    pub ifaces: Vec<IfaceSample>,
    pub totals: [ScopeTotal; 5],
    pub all: ScopeTotal,
    pub truncated: bool,
}

impl NetdevPart {
    pub fn physical_or_all(&self) -> ScopeTotal {
        if self
            .ifaces
            .iter()
            .any(|iface| iface.scope == Scope::Physical)
        {
            self.totals[Scope::Physical.index()]
        } else {
            self.all
        }
    }
}

impl Part for NetdevPart {
    fn clear(&mut self) {
        self.ifaces.clear();
        self.totals = [ScopeTotal::default(); 5];
        self.all = ScopeTotal::default();
        self.truncated = false;
    }
}

#[cfg(test)]
mod tests {
    use super::{IfaceName, NetdevPart, ScopeTotal};
    use crate::collect::{frame::Part, policy::net_scope::Scope};
    #[test]
    fn names_are_inline_bounded_and_clear_keeps_vec_capacity() {
        let name = IfaceName::new(b"enp95s0");
        assert_eq!(name.as_str(), "enp95s0");
        let mut part = NetdevPart {
            ifaces: vec![Default::default()],
            all: ScopeTotal {
                rx_bps: Some(1.0),
                tx_bps: Some(2.0),
            },
            ..Default::default()
        };
        let capacity = part.ifaces.capacity();
        assert_eq!(part.physical_or_all(), part.all);
        part.clear();
        assert!(part.ifaces.capacity() >= capacity);
        assert_eq!(part.all, ScopeTotal::default());
        assert_eq!(Scope::Physical.index(), 0);
    }
}
