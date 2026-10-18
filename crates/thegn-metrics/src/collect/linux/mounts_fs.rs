//! Filesystem usage reader. Keep tmpfs under /tmp, /dev/shm and /run/user because
//! those mounts can hold user data; omit other pseudo mounts and container overlays.
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use crate::collect::{
    CollectError, Collector, CollectorId, TickCtx,
    env::Env,
    frame::MonitorFrame,
    health::{Absent, Health},
    model::filesystems::{FilesystemKind, FilesystemRow, FilesystemsPart},
    parse::mountinfo::{MountLine, parse_line, unescape_into},
    schedule::Cadence,
};

const ROW_CAP: usize = 64;
const REREAD_MS: u64 = 30_000;

#[derive(Debug, Clone, Copy, Default)]
pub struct StatvfsData {
    pub blocks: u64,
    pub blocks_free: u64,
    pub blocks_available: u64,
    pub fragment_size: u64,
    pub files: u64,
    pub files_free: u64,
    pub files_available: u64,
}

pub trait FilesystemIo: Send {
    fn open_mountinfo(&mut self, path: &Path) -> io::Result<()>;
    fn mountinfo_changed(&mut self) -> io::Result<bool>;
    fn read_mountinfo(&mut self) -> io::Result<String>;
    fn statvfs(&mut self, mount: &Path) -> io::Result<StatvfsData>;
    fn list(&mut self, path: &Path) -> io::Result<Vec<PathBuf>>;
    fn read_text(&mut self, path: &Path) -> io::Result<String>;
    fn canonicalize(&mut self, path: &Path) -> io::Result<PathBuf>;
}

pub struct RealFilesystemIo {
    mountinfo: Option<File>,
}
impl RealFilesystemIo {
    fn new() -> Self {
        Self { mountinfo: None }
    }
}
impl FilesystemIo for RealFilesystemIo {
    fn open_mountinfo(&mut self, path: &Path) -> io::Result<()> {
        self.mountinfo = Some(File::open(path)?);
        Ok(())
    }
    fn mountinfo_changed(&mut self) -> io::Result<bool> {
        let Some(file) = &self.mountinfo else {
            return Ok(false);
        };
        let mut descriptor = libc::pollfd {
            fd: file.as_raw_fd(),
            events: libc::POLLPRI,
            revents: 0,
        };
        // SAFETY: descriptor points to one initialized pollfd for this call.
        let result = unsafe { libc::poll(&mut descriptor, 1, 0) };
        if result < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(result > 0 && descriptor.revents & libc::POLLPRI != 0)
        }
    }
    fn read_mountinfo(&mut self) -> io::Result<String> {
        let file = self
            .mountinfo
            .as_mut()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "mountinfo is not open"))?;
        file.seek(SeekFrom::Start(0))?;
        let mut text = String::new();
        file.read_to_string(&mut text)?;
        Ok(text)
    }
    fn statvfs(&mut self, mount: &Path) -> io::Result<StatvfsData> {
        let stats = nix::sys::statvfs::statvfs(mount).map_err(io::Error::from)?;
        Ok(StatvfsData {
            blocks: stats.blocks() as u64,
            blocks_free: stats.blocks_free() as u64,
            blocks_available: stats.blocks_available() as u64,
            fragment_size: stats.fragment_size() as u64,
            files: stats.files() as u64,
            files_free: stats.files_free() as u64,
            files_available: stats.files_available() as u64,
        })
    }
    fn list(&mut self, path: &Path) -> io::Result<Vec<PathBuf>> {
        std::fs::read_dir(path)?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect()
    }
    fn read_text(&mut self, path: &Path) -> io::Result<String> {
        std::fs::read_to_string(path)
    }
    fn canonicalize(&mut self, path: &Path) -> io::Result<PathBuf> {
        std::fs::canonicalize(path)
    }
}

#[derive(Debug, Clone)]
struct DiscoveredRow {
    row: FilesystemRow,
    mount_path: PathBuf,
    btrfs_uuid: Option<String>,
}

pub struct FilesystemsCollector<F: FilesystemIo = RealFilesystemIo> {
    io: F,
    mountinfo_path: PathBuf,
    sys_fs_btrfs: PathBuf,
    rows: Vec<DiscoveredRow>,
    truncated: bool,
    opened: bool,
    last_read_ms: u64,
}

impl FilesystemsCollector<RealFilesystemIo> {
    pub fn new() -> Self {
        Self::at("/proc/self/mountinfo", "/sys/fs/btrfs")
    }
    pub fn at(mountinfo: impl Into<PathBuf>, sys_fs_btrfs: impl Into<PathBuf>) -> Self {
        Self::with_io(RealFilesystemIo::new(), mountinfo, sys_fs_btrfs)
    }
}
impl Default for FilesystemsCollector<RealFilesystemIo> {
    fn default() -> Self {
        Self::new()
    }
}
impl<F: FilesystemIo> FilesystemsCollector<F> {
    pub fn with_io(io: F, mountinfo: impl Into<PathBuf>, sys_fs_btrfs: impl Into<PathBuf>) -> Self {
        Self {
            io,
            mountinfo_path: mountinfo.into(),
            sys_fs_btrfs: sys_fs_btrfs.into(),
            rows: Vec::new(),
            truncated: false,
            opened: false,
            last_read_ms: 0,
        }
    }

    fn discover_rows(&mut self, now_ms: u64) -> Health {
        if !self.opened {
            if self.io.open_mountinfo(&self.mountinfo_path).is_err() {
                return Health::Absent(Absent::NoHardware);
            }
            self.opened = true;
        }
        let Ok(text) = self.io.read_mountinfo() else {
            return Health::Absent(Absent::NoHardware);
        };
        self.rows.clear();
        self.truncated = false;
        let mut unique_count = 0;
        for line in text.lines() {
            let Some(parsed) = parse_line(line.as_bytes()) else {
                continue;
            };
            let Some((row, mount_path, btrfs_uuid)) = self.make_row(parsed) else {
                continue;
            };
            if self
                .rows
                .iter()
                .any(|existing| existing.row.superblock_dev == row.superblock_dev)
            {
                continue;
            }
            unique_count += 1;
            if self.rows.len() < ROW_CAP {
                self.rows.push(DiscoveredRow {
                    row,
                    mount_path,
                    btrfs_uuid,
                });
            }
        }
        self.truncated = unique_count > ROW_CAP;
        self.last_read_ms = now_ms;
        if self.rows.is_empty() {
            Health::Absent(Absent::NoHardware)
        } else {
            Health::Ok
        }
    }

    fn make_row(
        &mut self,
        parsed: MountLine<'_>,
    ) -> Option<(FilesystemRow, PathBuf, Option<String>)> {
        let fstype = String::from_utf8_lossy(parsed.fstype).into_owned();
        let mut mount_bytes = Vec::with_capacity(parsed.mount_point.len());
        unescape_into(parsed.mount_point, &mut mount_bytes);
        let mount_point = String::from_utf8_lossy(&mount_bytes).into_owned();
        let mut source_bytes = Vec::with_capacity(parsed.source.len());
        unescape_into(parsed.source, &mut source_bytes);
        let source = String::from_utf8_lossy(&source_bytes).into_owned();
        let kind = filesystem_kind(&fstype, &mount_point)?;
        let superblock_dev = format!("{}:{}", parsed.major, parsed.minor);
        let btrfs_uuid = if fstype == "btrfs" {
            self.btrfs_uuid(&superblock_dev)
        } else {
            None
        };
        let read_only = parsed
            .options
            .split(|byte| *byte == b',')
            .any(|option| option == b"ro");
        Some((
            FilesystemRow {
                mount_point: mount_point.clone(),
                fstype,
                source,
                superblock_dev,
                kind,
                total_bytes: None,
                free_bytes: None,
                avail_bytes: None,
                total_inodes: None,
                free_inodes: None,
                avail_inodes: None,
                btrfs_unallocated: None,
                read_only,
            },
            PathBuf::from(mount_point),
            btrfs_uuid,
        ))
    }

    fn btrfs_uuid(&mut self, dev: &str) -> Option<String> {
        let entries = match self.io.list(&self.sys_fs_btrfs) {
            Ok(entries) => entries,
            Err(_) => return None,
        };
        let sys_root = self.sys_fs_btrfs.parent()?.parent()?;
        let block_device = match self.io.canonicalize(&sys_root.join("dev/block").join(dev)) {
            Ok(path) => path,
            Err(_) => return None,
        };
        let block_name = block_device.file_name()?;
        for uuid_path in entries {
            let devices = uuid_path.join("devices");
            let Ok(device_paths) = self.io.list(&devices) else {
                continue;
            };
            for device_path in device_paths {
                if device_path.file_name() == Some(block_name) {
                    return uuid_path
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned());
                }
            }
        }
        None
    }

    fn refresh_if_needed(&mut self, now_ms: u64) {
        let changed = match self.io.mountinfo_changed() {
            Ok(changed) => changed,
            Err(_) => false,
        };
        if changed || now_ms.saturating_sub(self.last_read_ms) >= REREAD_MS {
            self.discover_rows(now_ms);
        }
    }

    fn sample_row(&mut self, index: usize) {
        let stats = self.io.statvfs(&self.rows[index].mount_path);
        let row = &mut self.rows[index].row;
        if let Ok(stats) = stats {
            row.total_bytes = stats.blocks.checked_mul(stats.fragment_size);
            row.free_bytes = stats.blocks_free.checked_mul(stats.fragment_size);
            row.avail_bytes = stats.blocks_available.checked_mul(stats.fragment_size);
            row.total_inodes = Some(stats.files);
            row.free_inodes = Some(stats.files_free);
            row.avail_inodes = Some(stats.files_available);
        } else {
            row.total_bytes = None;
            row.free_bytes = None;
            row.avail_bytes = None;
            row.total_inodes = None;
            row.free_inodes = None;
            row.avail_inodes = None;
        }
        row.btrfs_unallocated = None;
    }

    fn sample_btrfs(&mut self) {
        for index in 0..self.rows.len() {
            let Some(uuid) = self.rows[index].btrfs_uuid.clone() else {
                continue;
            };
            if self.rows[..index]
                .iter()
                .any(|row| row.btrfs_uuid.as_deref() == Some(uuid.as_str()))
            {
                continue;
            }
            let allocation = self.sys_fs_btrfs.join(&uuid).join("allocation");
            let mut allocated = Some(0_u64);
            for class in ["data", "metadata", "system"] {
                let path = allocation.join(class);
                let total = self.io.read_text(&path.join("total_bytes"));
                let Ok(total) = total else {
                    allocated = None;
                    break;
                };
                let Ok(total) = total.trim().parse::<u64>() else {
                    allocated = None;
                    break;
                };
                allocated = allocated.and_then(|sum| sum.checked_add(total));
            }
            let device_total = match self.io.list(&self.sys_fs_btrfs.join(&uuid).join("devices")) {
                Ok(devices) => devices.into_iter().try_fold(0_u64, |sum, device| {
                    let size = match self.io.read_text(&device.join("size")) {
                        Ok(size) => size,
                        Err(_) => return None,
                    };
                    let Ok(size) = size.trim().parse::<u64>() else {
                        return None;
                    };
                    sum.checked_add(size.checked_mul(512)?)
                }),
                Err(_) => None,
            };
            let unallocated = device_total
                .zip(allocated)
                .and_then(|(total, used)| total.checked_sub(used));
            for row in &mut self.rows {
                if row.btrfs_uuid.as_deref() == Some(uuid.as_str()) {
                    row.row.btrfs_unallocated = unallocated;
                }
            }
        }
    }
}

impl<F: FilesystemIo> Collector for FilesystemsCollector<F> {
    fn id(&self) -> CollectorId {
        CollectorId::Filesystems
    }
    fn cadence(&self) -> Cadence {
        Cadence::Every(5)
    }
    fn discover(&mut self, _env: &Env) -> Health {
        self.discover_rows(0)
    }
    fn sample(&mut self, ctx: &TickCtx, frame: &mut MonitorFrame) -> Result<(), CollectError> {
        self.refresh_if_needed(ctx.now_ms);
        for index in 0..self.rows.len() {
            if self.rows[index].row.kind == FilesystemKind::Local {
                self.sample_row(index);
            }
        }
        self.sample_btrfs();
        let part: &mut FilesystemsPart = &mut frame.parts.filesystems;
        part.truncated = self.truncated;
        for (index, discovered) in self.rows.iter().enumerate() {
            if index < part.rows.len() {
                part.rows[index].clone_from(&discovered.row);
            } else {
                part.rows.push(discovered.row.clone());
            }
        }
        part.rows.truncate(self.rows.len());
        frame.sampled.insert(CollectorId::Filesystems);
        Ok(())
    }
    fn close(&mut self) {
        self.opened = false;
        self.rows.clear();
    }
}

fn filesystem_kind(fstype: &str, mount: &str) -> Option<FilesystemKind> {
    if matches!(
        fstype,
        "nfs" | "nfs4" | "cifs" | "smb3" | "sshfs" | "fuse.sshfs" | "9p"
    ) {
        return Some(FilesystemKind::Network);
    }
    if (fstype == "tmpfs" || fstype == "devtmpfs")
        && !(mount == "/tmp"
            || mount == "/dev/shm"
            || mount == "/run/user"
            || mount.starts_with("/run/user/"))
    {
        return None;
    }
    if matches!(
        fstype,
        "proc"
            | "sysfs"
            | "cgroup"
            | "cgroup2"
            | "securityfs"
            | "debugfs"
            | "tracefs"
            | "bpf"
            | "pstore"
            | "mqueue"
            | "hugetlbfs"
            | "configfs"
            | "fusectl"
            | "autofs"
            | "binfmt_misc"
            | "rootfs"
            | "devpts"
            | "ramfs"
    ) || (fstype == "overlay"
        && (mount == "/var/lib/containers" || mount.starts_with("/var/lib/containers/")))
    {
        return None;
    }
    if fstype == "tmpfs" || fstype == "devtmpfs" || fstype == "overlay" {
        Some(FilesystemKind::Pseudo)
    } else {
        Some(FilesystemKind::Local)
    }
}

#[cfg(test)]
#[path = "mounts_fs_tests.rs"]
mod tests;
