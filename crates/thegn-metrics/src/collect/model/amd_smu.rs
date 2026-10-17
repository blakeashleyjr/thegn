//! Per-card results from the amdgpu SMU metrics reader.
use std::io;

use crate::collect::parse::amd_gpu_metrics::{Decoded, GpuMetrics};

pub const MAX_CARDS: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AmdSmuStatus {
    Decoded(Decoded),
    Error(io::ErrorKind),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AmdSmuCard {
    pub card: u8,
    pub suspended: bool,
    pub decoded: AmdSmuStatus,
    pub metrics: GpuMetrics,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct AmdSmuPart {
    pub cards: Vec<AmdSmuCard>,
}

impl crate::collect::frame::Part for AmdSmuPart {
    fn clear(&mut self) {
        self.cards.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::{AmdSmuCard, AmdSmuPart, AmdSmuStatus};
    use crate::collect::frame::Part;
    use crate::collect::parse::amd_gpu_metrics::GpuMetrics;
    use std::io;

    #[test]
    fn clear_keeps_card_vector_capacity() {
        let mut part = AmdSmuPart::default();
        part.cards.push(AmdSmuCard {
            card: 1,
            suspended: false,
            decoded: AmdSmuStatus::Error(io::ErrorKind::NotFound),
            metrics: GpuMetrics::default(),
        });
        let capacity = part.cards.capacity();
        part.clear();
        assert!(part.cards.is_empty());
        assert_eq!(part.cards.capacity(), capacity);
    }
}
