//! Empty F3a part for the psi collector; populated by its owner lane.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct PsiPart;
impl crate::collect::frame::Part for PsiPart {
    fn clear(&mut self) {}
}
