//! Empty F3a part for the powercap collector; populated by its owner lane.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct PowercapPart;
impl crate::collect::frame::Part for PowercapPart {
    fn clear(&mut self) {}
}
