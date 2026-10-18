//! Memory snapshot and derived byte totals.

use super::super::parse::{meminfo::MeminfoRaw, swaps::SwapDev, zram::ZramMmStat};

#[derive(Debug, Default, Clone, PartialEq)]
pub struct ZramDev {
    pub name: String,
    pub mm: Option<ZramMmStat>,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct MeminfoPart {
    pub raw: MeminfoRaw,
    pub swaps: Vec<SwapDev>,
    pub swaps_present: bool,
    pub zram: Vec<ZramDev>,
}

impl MeminfoPart {
    pub fn used_bytes(&self) -> Option<u64> {
        Some(
            self.raw
                .get(super::super::parse::meminfo::MemField::MemTotal)?
                .saturating_sub(
                    self.raw
                        .get(super::super::parse::meminfo::MemField::MemAvailable)?,
                ),
        )
    }
    pub fn swap_used_bytes(&self) -> Option<u64> {
        self.swaps_present.then(|| {
            self.swaps
                .iter()
                .fold(0_u64, |total, swap| total.saturating_add(swap.used_bytes))
        })
    }
    pub fn page_cache_bytes(&self) -> Option<u64> {
        Some(
            self.raw
                .get(super::super::parse::meminfo::MemField::Cached)?
                .saturating_sub(
                    self.raw
                        .get(super::super::parse::meminfo::MemField::Shmem)?,
                ),
        )
    }
}

impl crate::collect::frame::Part for MeminfoPart {
    fn clear(&mut self) {
        self.raw = MeminfoRaw::default();
        self.swaps.clear();
        self.swaps_present = false;
        self.zram.clear();
    }
}
