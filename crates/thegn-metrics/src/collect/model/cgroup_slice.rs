//! Aggregate sample for the thegn.slice cgroup.

use super::cgroup::CgroupSample;

#[derive(Debug, Default, Clone, PartialEq)]
pub struct CgroupSlicePart {
    pub found: bool,
    pub sample: CgroupSample,
    pub scopes: u32,
    pub panes: u32,
    pub truncated: bool,
}

impl crate::collect::frame::Part for CgroupSlicePart {
    fn clear(&mut self) {
        self.found = false;
        self.sample = CgroupSample::default();
        self.scopes = 0;
        self.panes = 0;
        self.truncated = false;
    }
}
