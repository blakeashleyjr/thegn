//! Linux cgroup v2 reader. The physical-disk filter avoids holder-empty
//! double-counting observed on this host (259:0 and 254:0 both have no whole
//! disk holders, while 254:0 duplicates 259:0 and 253:0 is zram).

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

use super::procfile;
use crate::collect::env::Env;
use crate::collect::frame::MonitorFrame;
use crate::collect::health::{Absent, Health};
use crate::collect::model::cgroup::{CgroupSample, GroupRow, MAX_GROUPS};
use crate::collect::parse::cgroup;
use crate::collect::schedule::Cadence;
use crate::collect::{CollectError, Collector, CollectorId, TickCtx};

pub trait CgroupFs {
    fn read(&mut self, path: &Path, buf: &mut Vec<u8>) -> io::Result<usize>;
}

#[derive(Debug, Default)]
pub struct RealFs {
    fallback: bool,
}

impl CgroupFs for RealFs {
    fn read(&mut self, path: &Path, buf: &mut Vec<u8>) -> io::Result<usize> {
        let mut file = File::open(path)?;
        let size = buf.capacity().max(4096);
        buf.resize(size, 0);
        let length = procfile::fill(&mut file, buf, &mut self.fallback)?;
        buf.truncate(length);
        Ok(length)
    }
}

pub fn cgroup2_root(root: &Path) -> Option<PathBuf> {
    root.join("cgroup.controllers")
        .is_file()
        .then(|| root.to_path_buf())
}

#[derive(Debug, Default, Clone)]
pub struct DeviceFilter {
    pub(crate) devices: Vec<(u32, u32)>,
}

impl DeviceFilter {
    pub fn from_sys_block(sys_block: &Path) -> Self {
        let devices = crate::disk_aggregate::physical_disks(sys_block)
            .into_iter()
            .filter_map(|name| std::fs::read(sys_block.join(name).join("dev")).ok())
            .filter_map(|bytes| cgroup::maj_min(&bytes))
            .collect();
        Self { devices }
    }

    pub(crate) fn devices(&self) -> &[(u32, u32)] {
        &self.devices
    }
}

fn read_bytes<F: CgroupFs>(fs: &mut F, path: &Path, buf: &mut Vec<u8>) -> Option<usize> {
    fs.read(path, buf).ok()
}

pub fn read_group<F: CgroupFs>(
    fs: &mut F,
    filter: &DeviceFilter,
    buf: &mut Vec<u8>,
    sample: &mut CgroupSample,
    path: &mut PathBuf,
) {
    *sample = CgroupSample::default();
    macro_rules! read {
        ($file:literal, $parse:expr, $field:ident) => {
            path.push($file);
            if let Some(length) = read_bytes(fs, path, buf) {
                sample.$field = $parse(&buf[..length]);
            }
            path.pop();
        };
    }
    read!("cpu.stat", cgroup::cpu_stat, cpu);
    read!("memory.current", cgroup::single_u64, mem_current);
    read!("memory.peak", cgroup::single_u64, mem_peak);
    read!(
        "memory.stat",
        |bytes| Some(cgroup::mem_stat(bytes)),
        mem_stat
    );
    read!(
        "memory.events",
        |bytes| Some(cgroup::mem_events(bytes)),
        mem_events
    );
    read!("memory.swap.current", cgroup::single_u64, swap_current);
    read!("pids.current", cgroup::single_u64, pids_current);
    path.push("io.stat");
    if let Some(length) = read_bytes(fs, path, buf)
        && !filter.devices.is_empty()
    {
        sample.io = Some(cgroup::io_stat_sum(&buf[..length], filter.devices()));
    }
    path.pop();
}

#[derive(Debug, Default)]
pub struct CgroupCollector {
    root: Option<PathBuf>,
    targets: Vec<PathBuf>,
    truncated: bool,
    filter: DeviceFilter,
    fs: RealFs,
    buffer: Vec<u8>,
    path_buffer: PathBuf,
}

impl CgroupCollector {
    pub fn set_targets(&mut self, targets: &[PathBuf]) {
        self.targets.clear();
        self.truncated = targets.len() > MAX_GROUPS;
        self.targets.extend(
            targets
                .iter()
                .filter(|target| valid_target(target))
                .take(MAX_GROUPS)
                .map(|target| {
                    self.root
                        .as_ref()
                        .map_or_else(|| target.clone(), |root| root.join(target))
                }),
        );
        if let Some(root) = self.root.as_ref() {
            self.targets.retain(|target| target.starts_with(root));
        }
    }
}

fn valid_target(target: &Path) -> bool {
    !target.is_absolute()
        && target
            .components()
            .all(|component| !matches!(component, std::path::Component::ParentDir))
}

impl Collector for CgroupCollector {
    fn id(&self) -> CollectorId {
        CollectorId::Cgroup
    }
    fn cadence(&self) -> Cadence {
        Cadence::Every(1)
    }

    fn discover(&mut self, env: &Env) -> Health {
        let root = env
            .cgroup_root
            .as_deref()
            .unwrap_or_else(|| Path::new("/sys/fs/cgroup"));
        let Some(root) = cgroup2_root(root) else {
            self.root = None;
            return Health::Absent(Absent::NotOnThisOs);
        };
        self.filter = DeviceFilter::from_sys_block(Path::new("/sys/block"));
        for target in &mut self.targets {
            if !target.is_absolute() {
                *target = root.join(&*target);
            }
        }
        self.targets.retain(|target| target.starts_with(&root));
        self.root = Some(root);
        Health::Ok
    }

    fn sample(&mut self, _ctx: &TickCtx, frame: &mut MonitorFrame) -> Result<(), CollectError> {
        let Some(root) = self.root.as_ref() else {
            return Err(CollectError::Gone);
        };
        frame.parts.cgroup.groups.clear();
        frame.parts.cgroup.truncated = self.truncated;
        for (index, target) in self.targets.iter().enumerate() {
            let mut sample = CgroupSample::default();
            self.path_buffer.clear();
            if target.is_absolute() {
                self.path_buffer.push(target);
            } else {
                self.path_buffer.push(root);
                self.path_buffer.push(target);
            }
            read_group(
                &mut self.fs,
                &self.filter,
                &mut self.buffer,
                &mut sample,
                &mut self.path_buffer,
            );
            frame.parts.cgroup.groups.push(GroupRow {
                target: index as u16,
                sample,
            });
        }
        frame.sampled.insert(CollectorId::Cgroup);
        Ok(())
    }

    fn close(&mut self) {
        self.root = None;
        self.targets.clear();
        self.filter.devices.clear();
        self.buffer.clear();
        self.path_buffer.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::{CgroupCollector, CgroupFs, DeviceFilter, cgroup2_root, read_group};
    use crate::collect::frame::MonitorFrame;
    use crate::collect::health::{Absent, Health};
    use crate::collect::model::cgroup::{CgroupSample, MAX_GROUPS};
    use crate::collect::{Collector, CollectorId, TickCtx, env::Env};
    use std::io;
    use std::path::{Path, PathBuf};

    #[derive(Default)]
    struct CountingFs(usize);
    impl CgroupFs for CountingFs {
        fn read(&mut self, path: &Path, buf: &mut Vec<u8>) -> io::Result<usize> {
            self.0 += 1;
            let bytes = std::fs::read(path)?;
            buf.clear();
            buf.extend_from_slice(&bytes);
            Ok(bytes.len())
        }
    }

    #[test]
    fn reader_is_bounded_and_missing_files_stay_absent() {
        let temp = tempfile::tempdir().unwrap();
        let group = temp.path().join("group");
        std::fs::create_dir(&group).unwrap();
        std::fs::write(group.join("memory.current"), b"123\n").unwrap();
        let mut fs = CountingFs::default();
        let mut buf = Vec::new();
        let mut sample = CgroupSample::default();
        let mut path = group.clone();
        read_group(
            &mut fs,
            &DeviceFilter::default(),
            &mut buf,
            &mut sample,
            &mut path,
        );
        assert!(fs.0 <= 12);
        assert_eq!(sample.mem_current, Some(123));
        assert_eq!(sample.cpu, None);
    }

    #[test]
    fn physical_device_filter_counts_the_single_real_disk_once() {
        let temp = tempfile::tempdir().unwrap();
        for (name, dev, physical) in [
            ("nvme0n1", "259:0", true),
            ("dm-0", "254:0", false),
            ("zram0", "253:0", false),
        ] {
            let dir = temp.path().join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("dev"), format!("{dev}\n")).unwrap();
            if physical {
                std::fs::create_dir(dir.join("device")).unwrap();
            }
        }
        let filter = DeviceFilter::from_sys_block(temp.path());
        let text = b"253:0 rbytes=100 wbytes=100\n259:0 rbytes=10 wbytes=20\n254:0 rbytes=10 wbytes=20\n259:0 rbytes=90\n";
        assert_eq!(
            crate::collect::parse::cgroup::io_stat_sum(text, filter.devices()).rbytes,
            10
        );
    }

    #[test]
    fn cgroup_root_targets_frame_and_absence_work() {
        let temp = tempfile::tempdir().unwrap();
        assert_eq!(cgroup2_root(temp.path()), None);
        std::fs::write(temp.path().join("cgroup.controllers"), b"cpu memory").unwrap();
        assert_eq!(cgroup2_root(temp.path()), Some(temp.path().to_path_buf()));
        let group = temp.path().join("group");
        std::fs::create_dir(&group).unwrap();
        let mut collector = CgroupCollector {
            root: Some(temp.path().to_path_buf()),
            filter: DeviceFilter::default(),
            ..CgroupCollector::default()
        };
        collector.set_targets(&vec![PathBuf::from("group"); MAX_GROUPS + 1]);
        assert!(collector.truncated);
        let mut frame = MonitorFrame::default();
        collector.sample(&TickCtx::default(), &mut frame).unwrap();
        assert!(frame.sampled.contains(CollectorId::Cgroup));
        assert_eq!(frame.parts.cgroup.groups.len(), MAX_GROUPS);
        assert_eq!(
            collector.discover(&Env {
                cgroup_root: Some(temp.path().to_path_buf()),
                ..Env::default()
            }),
            Health::Ok
        );
        assert_eq!(
            collector.discover(&Env {
                cgroup_root: Some(temp.path().join("missing")),
                ..Env::default()
            }),
            Health::Absent(Absent::NotOnThisOs)
        );
    }

    #[test]
    fn targets_cannot_escape_root_and_empty_device_filter_is_unknown() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("cgroup.controllers"), b"cpu").unwrap();
        let mut collector = CgroupCollector {
            root: Some(temp.path().to_path_buf()),
            ..CgroupCollector::default()
        };
        collector.set_targets(&[
            PathBuf::from("group"),
            PathBuf::from("../outside"),
            PathBuf::from("/etc"),
        ]);
        assert_eq!(collector.targets, vec![temp.path().join("group")]);

        let group = temp.path().join("group");
        std::fs::create_dir(&group).unwrap();
        std::fs::write(group.join("io.stat"), b"259:0 rbytes=10\n").unwrap();
        let mut fs = CountingFs::default();
        let mut buf = Vec::new();
        let mut sample = CgroupSample::default();
        let mut path = group.clone();
        read_group(
            &mut fs,
            &DeviceFilter::default(),
            &mut buf,
            &mut sample,
            &mut path,
        );
        assert_eq!(sample.io, None);
    }
}
