//! Empty F3a part for the cgroup_slice collector; populated by its owner lane.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct CgroupSlicePart;
impl crate::collect::frame::Part for CgroupSlicePart {
    fn clear(&mut self) {}
}
