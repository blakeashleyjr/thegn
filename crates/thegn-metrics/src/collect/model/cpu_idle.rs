//! Per-CPU cpuidle state metadata and residency percentages.

#[derive(Debug, Clone, Default, PartialEq)]
pub struct CpuIdleState {
    pub index: u8,
    pub name: Option<String>,
    pub latency_us: Option<u64>,
    pub disabled: Option<bool>,
    pub residency_pct: Option<f32>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct CpuIdleCore {
    pub cpu: u16,
    pub states: Vec<CpuIdleState>,
    pub c0_pct: Option<f32>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct CpuIdlePart {
    pub cores: Vec<CpuIdleCore>,
}

impl crate::collect::frame::Part for CpuIdlePart {
    fn clear(&mut self) {
        self.cores.clear();
    }
}
