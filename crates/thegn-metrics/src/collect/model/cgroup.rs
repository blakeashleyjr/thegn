//! Empty F3a part for the cgroup collector; populated by its owner lane.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct CgroupPart;
impl crate::collect::frame::Part for CgroupPart {
    fn clear(&mut self) {}
}
