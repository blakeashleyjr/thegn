//! Empty F3a part for the meminfo collector; populated by its owner lane.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct MeminfoPart;
impl crate::collect::frame::Part for MeminfoPart {
    fn clear(&mut self) {}
}
