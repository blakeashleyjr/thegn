//! Empty F3a part for the amd_smu collector; populated by its owner lane.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct AmdSmuPart;
impl crate::collect::frame::Part for AmdSmuPart {
    fn clear(&mut self) {}
}
