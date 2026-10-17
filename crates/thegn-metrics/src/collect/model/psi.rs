//! Pressure-stall collector result.
use crate::collect::parse::psi::PsiFile;

#[derive(Debug, Default, Clone, PartialEq)]
pub struct PsiResource {
    pub present: bool,
    pub file: PsiFile,
    pub some_stall_centi: Option<u32>,
    pub full_stall_centi: Option<u32>,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct PsiPart {
    pub cpu: PsiResource,
    pub memory: PsiResource,
    pub io: PsiResource,
    pub irq: PsiResource,
}
impl crate::collect::frame::Part for PsiPart {
    fn clear(&mut self) {
        *self = Self::default();
    }
}
