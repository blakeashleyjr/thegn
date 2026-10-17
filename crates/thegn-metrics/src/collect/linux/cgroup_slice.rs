//! Periodic aggregate reader for the thegn.slice cgroup.

use std::fs;
use std::path::{Path, PathBuf};

use super::cgroup::{DeviceFilter, RealFs, cgroup2_root, read_group};
use crate::collect::env::{self, Env};
use crate::collect::frame::MonitorFrame;
use crate::collect::frame::Part;
use crate::collect::health::{Absent, Health};
use crate::collect::model::cgroup::CgroupSample;
use crate::collect::model::cgroup_slice::CgroupSlicePart;
use crate::collect::policy::cgroup_classify::slice_prefix;
use crate::collect::schedule::Cadence;
use crate::collect::{CollectError, Collector, CollectorId, TickCtx};

pub fn discover_slice(root: &Path, self_cgroup_bytes: &[u8]) -> Option<PathBuf> {
    let membership = env::self_cgroup_v2(self_cgroup_bytes)?;
    let prefix = slice_prefix(membership)?;
    let relative = prefix.strip_prefix(b"/")?;
    if relative
        .split(|byte| *byte == b'/')
        .any(|part| part == b"..")
    {
        return None;
    }
    use std::os::unix::ffi::OsStrExt;
    Some(root.join(std::ffi::OsStr::from_bytes(relative)))
}

#[derive(Debug, Default)]
pub struct CgroupSliceCollector {
    path: Option<PathBuf>,
    filter: DeviceFilter,
    fs: RealFs,
    buffer: Vec<u8>,
}

impl Collector for CgroupSliceCollector {
    fn id(&self) -> CollectorId {
        CollectorId::CgroupSlice
    }
    fn cadence(&self) -> Cadence {
        Cadence::Every(5)
    }

    fn discover(&mut self, env: &Env) -> Health {
        let root = env
            .cgroup_root
            .as_deref()
            .unwrap_or_else(|| Path::new("/sys/fs/cgroup"));
        let Some(root) = cgroup2_root(root) else {
            self.path = None;
            return Health::Absent(Absent::NotOnThisOs);
        };
        let Ok(membership) = fs::read("/proc/self/cgroup") else {
            self.path = None;
            return Health::Absent(Absent::NotOnThisOs);
        };
        let Some(path) = discover_slice(&root, &membership) else {
            self.path = None;
            return Health::Absent(Absent::NotOnThisOs);
        };
        self.filter = DeviceFilter::from_sys_block(Path::new("/sys/block"));
        self.path = Some(path);
        Health::Ok
    }

    fn sample(&mut self, _ctx: &TickCtx, frame: &mut MonitorFrame) -> Result<(), CollectError> {
        let Some(path) = self.path.as_ref() else {
            return Err(CollectError::Gone);
        };
        let part = &mut frame.parts.cgroup_slice;
        part.clear();
        part.found = true;
        read_group(
            &mut self.fs,
            path,
            &self.filter,
            &mut self.buffer,
            &mut part.sample,
        );
        frame.sampled.insert(CollectorId::CgroupSlice);
        Ok(())
    }

    fn close(&mut self) {
        self.path = None;
        self.filter.devices.clear();
        self.buffer.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::{CgroupSliceCollector, discover_slice};
    use crate::collect::frame::MonitorFrame;
    use crate::collect::health::{Absent, Health};
    use crate::collect::{Collector, TickCtx, env::Env};

    #[test]
    fn discovers_the_slice_from_mixed_membership_and_rejects_outside_paths() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        assert_eq!(discover_slice(root, b"2:cpu:/legacy\n0::/user.slice/user-1000.slice/user@1000.service/thegn.slice/run.scope"), Some(root.join("user.slice/user-1000.slice/user@1000.service/thegn.slice")));
        assert_eq!(discover_slice(root, b"0::/user.slice/other.service"), None);
        let mut collector = CgroupSliceCollector::default();
        assert_eq!(
            collector.discover(&Env {
                cgroup_root: Some(root.join("v1")),
                ..Env::default()
            }),
            Health::Absent(Absent::NotOnThisOs)
        );
        let mut frame = MonitorFrame::default();
        assert!(collector.sample(&TickCtx::default(), &mut frame).is_err());
    }
}
