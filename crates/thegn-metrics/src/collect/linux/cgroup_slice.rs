//! Periodic aggregate reader for the thegn.slice cgroup.

use std::fs;
use std::path::{Path, PathBuf};

use super::cgroup::{DeviceFilter, HeldFiles, RealFs, cgroup2_root, read_group};
use crate::collect::env::{self, Env};
use crate::collect::frame::MonitorFrame;
use crate::collect::health::{Absent, Health};
use crate::collect::policy::cgroup_classify::{THEGN_SLICE, slice_prefix};
use crate::collect::schedule::Cadence;
use crate::collect::{CollectError, Collector, CollectorId, TickCtx};

pub fn discover_slice(root: &Path, self_cgroup_bytes: &[u8]) -> Option<PathBuf> {
    let membership = env::self_cgroup_v2(self_cgroup_bytes)?;
    let prefix = if let Some(prefix) = slice_prefix(membership) {
        prefix
    } else {
        let service = membership
            .split(|byte| *byte == b'/')
            .find(|part| part.starts_with(b"user@") && part.ends_with(b".service"))?;
        let offset = service.as_ptr() as usize - membership.as_ptr() as usize;
        let end = offset + service.len();
        if membership[..end]
            .split(|byte| *byte == b'/')
            .any(|part| part == b"..")
        {
            return None;
        }
        let mut prefix = membership[..end].to_vec();
        prefix.push(b'/');
        prefix.extend_from_slice(THEGN_SLICE.as_bytes());
        return path_from_membership(root, prefix.strip_prefix(b"/")?);
    };
    let relative = prefix.strip_prefix(b"/")?;
    if relative
        .split(|byte| *byte == b'/')
        .any(|part| part == b"..")
    {
        return None;
    }
    path_from_membership(root, relative)
}

fn path_from_membership(root: &Path, relative: &[u8]) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    Some(root.join(std::ffi::OsStr::from_bytes(relative)))
}

#[derive(Default)]
struct LimitFold {
    minimum: Option<u64>,
    all_max: bool,
    observed: bool,
}

impl LimitFold {
    fn include(&mut self, limit: Option<crate::collect::model::cgroup::Limit>) {
        use crate::collect::model::cgroup::Limit;

        self.observed = true;
        match limit {
            Some(Limit::Value(value)) => {
                self.all_max = false;
                self.minimum = Some(self.minimum.map_or(value, |current| current.min(value)));
            }
            Some(Limit::Max) => {}
            None => self.all_max = false,
        }
    }

    fn result(self) -> Option<crate::collect::model::cgroup::Limit> {
        use crate::collect::model::cgroup::Limit;

        self.minimum
            .map(Limit::Value)
            .or_else(|| (self.observed && self.all_max).then_some(Limit::Max))
    }
}

#[derive(Debug, Default)]
pub struct CgroupSliceCollector {
    path: Option<PathBuf>,
    own_leaf: Option<PathBuf>,
    own_chain: Vec<PathBuf>,
    held: HeldFiles,
    filter: DeviceFilter,
    fs: RealFs,
    buffer: Vec<u8>,
    path_buffer: PathBuf,
}

impl CgroupSliceCollector {
    fn configure(&mut self, root: PathBuf, membership: &[u8], path: PathBuf) {
        let own_membership = env::self_cgroup_v2(membership);
        let own_leaf = own_membership
            .and_then(|member| path_from_membership(&root, member.strip_prefix(b"/")?));
        self.own_leaf = own_leaf.clone();
        self.own_chain.clear();
        if let Some(leaf) = own_leaf.as_ref() {
            let mut level = leaf.clone();
            loop {
                self.own_chain.push(level.clone());
                if level == root || !level.pop() || !level.starts_with(&root) {
                    break;
                }
            }
        }
        self.filter = DeviceFilter::from_sys_block(Path::new("/sys/block"));
        let slice_path = path.clone();
        self.path = Some(path);
        let own_paths = self.own_chain.iter().flat_map(|dir| {
            [
                dir.join("memory.current"),
                dir.join("memory.max"),
                dir.join("memory.high"),
            ]
        });
        self.held.discover(own_paths.chain([
            slice_path.join("memory.current"),
            slice_path.join("memory.max"),
            slice_path.join("memory.high"),
        ]));
    }
}

impl Collector for CgroupSliceCollector {
    fn id(&self) -> CollectorId {
        CollectorId::CgroupSlice
    }
    fn cadence(&self) -> Cadence {
        Cadence::Every(5)
    }

    fn discover(&mut self, env: &Env) -> Health {
        self.held = HeldFiles::default();
        self.own_leaf = None;
        self.own_chain.clear();
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
        let Some(path) = discover_slice(&root, &membership).filter(|path| path.is_dir()) else {
            self.path = None;
            return Health::Absent(Absent::NotOnThisOs);
        };
        self.configure(root, &membership, path);
        Health::Ok
    }

    fn sample(&mut self, _ctx: &TickCtx, frame: &mut MonitorFrame) -> Result<(), CollectError> {
        let Some(path) = self.path.as_ref() else {
            return Err(CollectError::Gone);
        };
        let part = &mut frame.parts.cgroup_slice;
        crate::collect::frame::Part::clear(part);
        part.found = true;
        self.path_buffer.clear();
        self.path_buffer.push(path);
        read_group(
            &mut self.fs,
            &self.filter,
            &mut self.buffer,
            &mut part.sample,
            &mut self.path_buffer,
        );
        let slice_file_base = self.own_chain.len().saturating_mul(3);
        if let Some(length) = self.held.read(slice_file_base, &mut self.buffer) {
            part.sample.mem_current =
                crate::collect::parse::cgroup::single_u64(&self.buffer[..length]);
        }
        if let Some(length) = self
            .held
            .read(slice_file_base.saturating_add(1), &mut self.buffer)
        {
            part.sample.mem_max =
                crate::collect::parse::cgroup::memory_limit(&self.buffer[..length]);
        }
        if let Some(length) = self
            .held
            .read(slice_file_base.saturating_add(2), &mut self.buffer)
        {
            part.sample.mem_high =
                crate::collect::parse::cgroup::memory_limit(&self.buffer[..length]);
        }
        part.own_current = self.own_leaf.as_ref().and_then(|_| {
            self.held.read(0, &mut self.buffer).and_then(|length| {
                crate::collect::parse::cgroup::single_u64(&self.buffer[..length])
            })
        });
        let mut effective = LimitFold {
            all_max: true,
            ..LimitFold::default()
        };
        for (level_index, _) in self.own_chain.iter().enumerate() {
            for file_index in [1, 2] {
                let held_index = level_index.saturating_mul(3).saturating_add(file_index);
                if !self.held.is_present(held_index) {
                    continue;
                }
                let limit = self
                    .held
                    .read(held_index, &mut self.buffer)
                    .and_then(|length| {
                        crate::collect::parse::cgroup::memory_limit(&self.buffer[..length])
                    });
                effective.include(limit);
            }
        }
        part.effective_limit = effective.result();
        if self.held.needs_reopen() {
            return Err(CollectError::Gone);
        }
        frame.sampled.insert(CollectorId::CgroupSlice);
        Ok(())
    }

    fn close(&mut self) {
        self.path = None;
        self.own_leaf = None;
        self.own_chain.clear();
        self.held = HeldFiles::default();
        self.filter.devices.clear();
        self.buffer.clear();
        self.path_buffer.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::{CgroupSliceCollector, LimitFold, discover_slice};
    use crate::collect::frame::MonitorFrame;
    use crate::collect::health::{Absent, Health};
    use crate::collect::{Collector, TickCtx, env::Env};

    #[test]
    fn collector_skips_absent_root_limits_and_holds_discovered_files() {
        use crate::collect::model::cgroup::Limit;
        for high in [b"max" as &[u8], b"4096"] {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path();
            std::fs::write(root.join("cgroup.controllers"), b"memory").unwrap();
            let membership = b"0::/a/b\n";
            let leaf = root.join("a/b");
            std::fs::create_dir_all(&leaf).unwrap();
            std::fs::write(root.join("a/memory.max"), b"max\n").unwrap();
            std::fs::write(root.join("a/memory.high"), b"max\n").unwrap();
            std::fs::write(leaf.join("memory.current"), b"2048\n").unwrap();
            std::fs::write(leaf.join("memory.max"), b"max\n").unwrap();
            std::fs::write(leaf.join("memory.high"), [high, b"\n"].concat()).unwrap();
            let slice = root.join("a/slice");
            std::fs::create_dir_all(&slice).unwrap();
            std::fs::write(slice.join("memory.current"), b"8192\n").unwrap();
            std::fs::write(slice.join("memory.max"), b"max\n").unwrap();
            std::fs::write(slice.join("memory.high"), b"max\n").unwrap();
            let mut collector = CgroupSliceCollector::default();
            collector.configure(root.to_path_buf(), membership, slice.clone());
            let opens_at_discover = collector.held.open_attempts();
            std::fs::rename(leaf.join("memory.max"), leaf.join("memory.max.moved")).unwrap();
            for _ in 0..5 {
                let mut frame = MonitorFrame::default();
                collector.sample(&TickCtx::default(), &mut frame).unwrap();
                let expected = if high == b"max" {
                    Limit::Max
                } else {
                    Limit::Value(4096)
                };
                assert_eq!(frame.parts.cgroup_slice.effective_limit, Some(expected));
            }
            assert_eq!(collector.held.open_attempts(), opens_at_discover);
        }
    }

    #[test]
    fn effective_limit_uses_minimum_numeric_value_and_only_reports_all_max_when_complete() {
        use crate::collect::model::cgroup::Limit;

        let mut limits = LimitFold {
            all_max: true,
            ..LimitFold::default()
        };
        limits.include(Some(Limit::Max));
        limits.include(Some(Limit::Value(4096)));
        limits.include(Some(Limit::Value(2048)));
        assert_eq!(limits.result(), Some(Limit::Value(2048)));

        let mut unlimited = LimitFold {
            all_max: true,
            ..LimitFold::default()
        };
        unlimited.include(Some(Limit::Max));
        unlimited.include(Some(Limit::Max));
        assert_eq!(unlimited.result(), Some(Limit::Max));

        let mut unreadable = LimitFold {
            all_max: true,
            ..LimitFold::default()
        };
        unreadable.include(None);
        assert_eq!(unreadable.result(), None);
    }

    #[test]
    fn discovers_the_slice_from_mixed_membership_and_rejects_outside_paths() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        assert_eq!(discover_slice(root, b"2:cpu:/legacy\n0::/user.slice/user-1000.slice/user@1000.service/thegn.slice/run.scope"), Some(root.join("user.slice/user-1000.slice/user@1000.service/thegn.slice")));
        assert_eq!(
            discover_slice(
                root,
                b"0::/user.slice/user-1000.slice/user@1000.service/app.slice/run.scope"
            ),
            Some(root.join("user.slice/user-1000.slice/user@1000.service/thegn.slice"))
        );
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
