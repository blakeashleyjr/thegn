//! `thegn fleet cost`'s Vercel section: per-sandbox current-session accrual
//! and snapshot storage for this controller's sandboxes.
//!
//! Spine stub (b156 lane 1): signatures are fixed; lane 5 fills the bodies.

use anyhow::Result;

use super::VercelProvider;

/// One sandbox's cost row (an estimate; current session only).
#[derive(Debug, Clone, PartialEq)]
pub struct UsageRow {
    pub name: String,
    pub status: String,
    pub session: Option<thegn_core::vercel::usage::Accrual>,
    pub snapshots_usd_month: f64,
    pub snapshot_bytes: u64,
}

impl VercelProvider {
    /// The cost rows plus whether the listing was complete.
    pub async fn usage_report(&self) -> Result<(Vec<UsageRow>, bool)> {
        Ok((Vec::new(), false))
    }
}
