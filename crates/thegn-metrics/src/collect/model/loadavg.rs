//! Load-average collector result.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct LoadavgPart {
    pub value: Option<crate::collect::parse::loadavg::Loadavg>,
}
impl crate::collect::frame::Part for LoadavgPart {
    fn clear(&mut self) {
        self.value = None;
    }
}
