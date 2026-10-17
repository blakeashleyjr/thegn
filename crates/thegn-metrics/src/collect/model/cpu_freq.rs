//! Empty F3a part for the cpu_freq collector; populated by its owner lane.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct CpuFreqPart;
impl crate::collect::frame::Part for CpuFreqPart {
    fn clear(&mut self) {}
}
