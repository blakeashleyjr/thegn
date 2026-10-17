//! Empty F3a part for the diskstats collector; populated by its owner lane.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct DiskstatsPart;
impl crate::collect::frame::Part for DiskstatsPart {
    fn clear(&mut self) {}
}
