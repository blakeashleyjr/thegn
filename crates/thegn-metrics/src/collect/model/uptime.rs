//! Uptime collector result.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct UptimePart {
    pub value: Option<crate::collect::parse::uptime::Uptime>,
}
impl crate::collect::frame::Part for UptimePart {
    fn clear(&mut self) {
        self.value = None;
    }
}
