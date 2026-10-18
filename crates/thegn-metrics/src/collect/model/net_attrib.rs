//! Bounded per-owner TCP byte attribution results.
use super::super::policy::cgroup_classify::OwnerKind;
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnerRates {
    pub cgroup_id: u64,
    pub path: Arc<[u8]>,
    pub kind: OwnerKind,
    pub name: Arc<[u8]>,
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
    /// Builds a result using only non-loopback TCP deltas for the physical remainder.
    pub fn from_attributed(
        owners: Vec<OwnerRates>,
        physical_rx: u64,
        physical_tx: u64,
        attributed_rx: u64,
        attributed_tx: u64,
        truncated: bool,
    ) -> Self {
        Self {
            owners,
            unattributed_rx: physical_rx.saturating_sub(attributed_rx),
            unattributed_tx: physical_tx.saturating_sub(attributed_tx),
            truncated,
        }
    }
}
