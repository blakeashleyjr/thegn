//! Bounded per-owner TCP byte attribution results.
use super::super::policy::cgroup_classify::OwnerKind;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnerRates {
    pub cgroup_id: u64,
    pub path: Vec<u8>,
    pub kind: OwnerKind,
    pub name: Vec<u8>,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct NetAttribution {
    pub owners: Vec<OwnerRates>,
    pub unattributed_rx: u64,
    pub unattributed_tx: u64,
    pub truncated: bool,
}

impl NetAttribution {
    pub fn from_attributed(
        owners: Vec<OwnerRates>,
        physical_rx: u64,
        physical_tx: u64,
        truncated: bool,
    ) -> Self {
        let attributed_rx = owners
            .iter()
            .fold(0_u64, |sum, owner| sum.saturating_add(owner.rx_bytes));
        let attributed_tx = owners
            .iter()
            .fold(0_u64, |sum, owner| sum.saturating_add(owner.tx_bytes));
        Self {
            owners,
            unattributed_rx: physical_rx.saturating_sub(attributed_rx),
            unattributed_tx: physical_tx.saturating_sub(attributed_tx),
            truncated,
        }
    }
}
