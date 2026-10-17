//! Empty F3a part for the vmstat collector; populated by its owner lane.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct VmstatPart;
impl crate::collect::frame::Part for VmstatPart {
    fn clear(&mut self) {}
}
