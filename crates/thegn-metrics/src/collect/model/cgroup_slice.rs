//! Aggregate sample for the thegn.slice cgroup.

use super::cgroup::CgroupSample;

#[derive(Debug, Default, Clone, PartialEq)]
pub struct CgroupSlicePart {
    pub found: bool,
    pub sample: CgroupSample,
    /// Effective memory ceiling for the process's leaf-to-root cgroup chain.
    pub effective_limit: Option<super::cgroup::Limit>,
    /// Current memory usage in the process's leaf cgroup.
    pub own_current: Option<u64>,
    pub scopes: u32,
    pub panes: u32,
    pub truncated: bool,
}

impl crate::collect::frame::Part for CgroupSlicePart {
    fn clear(&mut self) {
        self.found = false;
        self.sample = CgroupSample::default();
        self.effective_limit = None;
        self.own_current = None;
        self.scopes = 0;
        self.panes = 0;
        self.truncated = false;
    }
}
