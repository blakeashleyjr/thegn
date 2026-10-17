//! Empty F3a part for the loadavg collector; populated by its owner lane.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct LoadavgPart;
impl crate::collect::frame::Part for LoadavgPart {
    fn clear(&mut self) {}
}
