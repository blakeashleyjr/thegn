//! Empty F3a part for the hwmon collector; populated by its owner lane.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct HwmonPart;
impl crate::collect::frame::Part for HwmonPart {
    fn clear(&mut self) {}
}
