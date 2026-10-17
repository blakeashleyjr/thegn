//! Empty F3a part for the netdev collector; populated by its owner lane.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct NetdevPart;
impl crate::collect::frame::Part for NetdevPart {
    fn clear(&mut self) {}
}
