//! Empty F3a part for the cpu_stat collector; populated by its owner lane.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct CpuStatPart;
impl crate::collect::frame::Part for CpuStatPart {
    fn clear(&mut self) {}
}
