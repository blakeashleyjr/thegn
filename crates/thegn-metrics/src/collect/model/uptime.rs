//! Empty F3a part for the uptime collector; populated by its owner lane.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct UptimePart;
impl crate::collect::frame::Part for UptimePart {
    fn clear(&mut self) {}
}
