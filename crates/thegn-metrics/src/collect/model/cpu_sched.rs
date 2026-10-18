//! Scheduler wait measurements derived from schedstat counters.
use super::super::frame::Part;

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct CoreSchedSample {
    pub id: u16,
    pub online: bool,
    pub run_queue_wait_ms_per_s: Option<f64>,
    pub avg_wait_ms_per_timeslice: Option<f64>,
}
#[derive(Debug, Default, Clone, PartialEq)]
pub struct CpuSchedPart {
    pub cores: Vec<CoreSchedSample>,
    pub version: Option<u64>,
}
impl Part for CpuSchedPart {
    fn clear(&mut self) {
        self.cores.clear();
        self.version = None;
    }
}
