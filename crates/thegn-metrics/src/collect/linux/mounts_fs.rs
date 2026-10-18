//! Filesystem usage reader. Keep tmpfs under /tmp, /dev/shm and /run/user because
//! those mounts can hold user data; omit other pseudo mounts and container overlays.
//! Btrfs `bytes_used` is deferred; unallocated space uses device size minus `disk_total`.
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
    fn canonicalize(&mut self, path: &Path) -> io::Result<PathBuf>;
    fn clear_btrfs_files(&mut self);
    fn open_btrfs_file(&mut self, path: &Path) -> io::Result<usize>;
    fn read_btrfs_file(&mut self, handle: usize, buffer: &mut [u8]) -> io::Result<usize>;
}

pub struct RealFilesystemIo {
    mountinfo: Option<File>,
    btrfs_files: Vec<File>,
}
impl RealFilesystemIo {
    fn new() -> Self {
        Self {
            mountinfo: None,
            btrfs_files: Vec::new(),
        }
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
            blocks: stats.blocks(),
            blocks_free: stats.blocks_free(),
            blocks_available: stats.blocks_available(),
            fragment_size: stats.fragment_size(),
            files: stats.files(),
            files_free: stats.files_free(),
            files_available: stats.files_available(),
        })
    }
    fn list(&mut self, path: &Path) -> io::Result<Vec<PathBuf>> {
        std::fs::read_dir(path)?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect()
    }
    fn canonicalize(&mut self, path: &Path) -> io::Result<PathBuf> {
        std::fs::canonicalize(path)
    }
    fn clear_btrfs_files(&mut self) {
        self.btrfs_files.clear();
    }
    fn open_btrfs_file(&mut self, path: &Path) -> io::Result<usize> {
        let file = File::open(path)?;
        let handle = self.btrfs_files.len();
        self.btrfs_files.push(file);
        Ok(handle)
    }
    fn read_btrfs_file(&mut self, handle: usize, buffer: &mut [u8]) -> io::Result<usize> {
        let file = self
            .btrfs_files
            .get(handle)
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
        // SAFETY: the buffer is writable for its length and the held fd remains open.
        let count = unsafe {
            libc::pread(
                file.as_raw_fd(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                0,
            )
        };
        if count < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(count as usize)
        }
    }
}

#[derive(Debug, Clone)]
struct DiscoveredRow {
    row: FilesystemRow,
    mount_path: PathBuf,
    btrfs_index: Option<usize>,
}

struct BtrfsFs {
    uuid_path: PathBuf,
    allocation_handles: [Option<usize>; 3],
    device_handles: Vec<Option<usize>>,
}

#[derive(PartialEq, Eq)]
struct FilesystemIdentity {
    mount_point: String,
    fstype: String,
    source: String,
    superblock_dev: String,
    kind: FilesystemKind,
    read_only: bool,
}

pub struct FilesystemsCollector<F: FilesystemIo = RealFilesystemIo> {
    io: F,
    mountinfo_path: PathBuf,
    sys_fs_btrfs: PathBuf,
    rows: Vec<DiscoveredRow>,
    btrfs: Vec<BtrfsFs>,
    truncated: bool,
    opened: bool,
    last_read_ms: Option<u64>,
    generation: u64,
    published_generation: u64,
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
            btrfs: Vec::new(),
            truncated: false,
            opened: false,
            last_read_ms: None,
            generation: 0,
            published_generation: u64::MAX,
        }
    }

    fn discover_rows(&mut self, now_ms: Option<u64>) -> Health {
        if !self.opened {
            if self.io.open_mountinfo(&self.mountinfo_path).is_err() {
                return Health::Absent(Absent::NoHardware);
            }
            self.opened = true;
        }
        let Ok(text) = self.io.read_mountinfo() else {
            return Health::Absent(Absent::NoHardware);
        };
        let previous_identity = self
            .rows
            .iter()
            .map(|row| row_identity(&row.row))
            .collect::<Vec<_>>();
        self.rows.clear();
        self.btrfs.clear();
        self.io.clear_btrfs_files();
        self.truncated = false;
        let mut unique_count = 0;
        for line in text.lines() {
            let Some(parsed) = parse_line(line.as_bytes()) else {
                continue;
            };
            let Some((mut row, mount_path)) = self.make_row(parsed) else {
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
                let btrfs_index = if row.fstype == "btrfs" {
                    self.btrfs_index(&row.source)
                } else {
                    None
                };
                row.btrfs_unallocated = None;
                self.rows.push(DiscoveredRow {
                    row,
                    mount_path,
                    btrfs_index,
                });
            }
        }
        self.truncated = unique_count > ROW_CAP;
        let next_identity = self
            .rows
            .iter()
            .map(|row| row_identity(&row.row))
            .collect::<Vec<_>>();
        if previous_identity != next_identity {
            self.generation = self.generation.wrapping_add(1);
        }
        self.last_read_ms = now_ms;
        if self.rows.is_empty() {
            Health::Absent(Absent::NoHardware)
        } else {
            Health::Ok
        }
    }

    fn make_row(&mut self, parsed: MountLine<'_>) -> Option<(FilesystemRow, PathBuf)> {
        let fstype = String::from_utf8_lossy(parsed.fstype).into_owned();
        let mut mount_bytes = Vec::with_capacity(parsed.mount_point.len());
        unescape_into(parsed.mount_point, &mut mount_bytes);
        let mount_point = String::from_utf8_lossy(&mount_bytes).into_owned();
        let mut source_bytes = Vec::with_capacity(parsed.source.len());
        unescape_into(parsed.source, &mut source_bytes);
        let source = String::from_utf8_lossy(&source_bytes).into_owned();
        let kind = filesystem_kind(&fstype, &mount_point)?;
        let superblock_dev = format!("{}:{}", parsed.major, parsed.minor);
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
        ))
    }

    fn btrfs_index(&mut self, source: &str) -> Option<usize> {
        let entries = match self.io.list(&self.sys_fs_btrfs) {
            Ok(entries) => entries,
            Err(_) => return None,
        };
        let block_device = match self.io.canonicalize(Path::new(source)) {
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
                    if let Some(index) = self
                        .btrfs
                        .iter()
                        .position(|filesystem| filesystem.uuid_path == uuid_path)
                    {
                        return Some(index);
                    }
                    let allocation = uuid_path.join("allocation");
                    let allocation_handles = ["data", "metadata", "system"].map(|class| {
                        self.io
                            .open_btrfs_file(&allocation.join(class).join("disk_total"))
                            .ok()
                    });
                    let device_handles = self
                        .io
                        .list(&devices)
                        .ok()?
                        .into_iter()
                        .map(|path| self.io.open_btrfs_file(&path.join("size")).ok())
                        .collect();
                    let index = self.btrfs.len();
                    self.btrfs.push(BtrfsFs {
                        uuid_path,
                        allocation_handles,
                        device_handles,
                    });
                    return Some(index);
                }
            }
        }
        None
    }

    fn refresh_if_needed(&mut self, now_ms: u64) {
        let changed = self.io.mountinfo_changed().unwrap_or_default();
        match self.last_read_ms {
            None if changed => {
                self.discover_rows(Some(now_ms));
            }
            None => self.last_read_ms = Some(now_ms),
            Some(last) if changed || now_ms.saturating_sub(last) >= REREAD_MS => {
                self.discover_rows(Some(now_ms));
            }
            Some(_) => {}
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

    fn read_u64(&mut self, handle: usize) -> Option<u64> {
        let mut buffer = [0_u8; 64];
        let count = self.io.read_btrfs_file(handle, &mut buffer).ok()?;
        std::str::from_utf8(&buffer[..count])
            .ok()?
            .trim()
            .parse()
            .ok()
    }

    fn sample_btrfs(&mut self) {
        for btrfs_index in 0..self.btrfs.len() {
            let allocation_handles = self.btrfs[btrfs_index].allocation_handles;
            let device_count = self.btrfs[btrfs_index].device_handles.len();
            let mut allocated = Some(0_u64);
            for handle in allocation_handles.into_iter().flatten() {
                allocated = allocated.and_then(|sum| {
                    self.read_u64(handle)
                        .and_then(|value| sum.checked_add(value))
                });
            }
            if allocation_handles.iter().any(Option::is_none) {
                allocated = None;
            }
            let mut device_total = Some(0_u64);
            for device_index in 0..device_count {
                let handle = self.btrfs[btrfs_index].device_handles[device_index];
                let Some(handle) = handle else {
                    device_total = None;
                    break;
                };
                device_total = device_total.and_then(|sum| {
                    self.read_u64(handle).and_then(|size| {
                        size.checked_mul(512)
                            .and_then(|bytes| sum.checked_add(bytes))
                    })
                });
            }
            let unallocated = device_total
                .zip(allocated)
                .and_then(|(total, used)| total.checked_sub(used));
            for row in &mut self.rows {
                if row.btrfs_index == Some(btrfs_index) {
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
        self.discover_rows(None)
    }
    fn sample(&mut self, ctx: &TickCtx, frame: &mut MonitorFrame) -> Result<(), CollectError> {
        self.refresh_if_needed(ctx.now_ms);
        for index in 0..self.rows.len() {
            if self.rows[index].row.kind != FilesystemKind::Network {
                self.sample_row(index);
            }
        }
        self.sample_btrfs();
        let part: &mut FilesystemsPart = &mut frame.parts.filesystems;
        part.truncated = self.truncated;
        if self.generation != self.published_generation
            || part.rows.len() != self.rows.len()
            || part
                .rows
                .iter()
                .zip(&self.rows)
                .any(|(published, discovered)| !same_row_identity(published, &discovered.row))
        {
            part.rows.clear();
            part.rows
                .extend(self.rows.iter().map(|discovered| discovered.row.clone()));
            self.published_generation = self.generation;
        } else {
            for (published, discovered) in part.rows.iter_mut().zip(&self.rows) {
                copy_numeric(published, &discovered.row);
            }
        }
        frame.sampled.insert(CollectorId::Filesystems);
        Ok(())
    }
    fn close(&mut self) {
        self.opened = false;
        self.rows.clear();
        self.btrfs.clear();
        self.io.clear_btrfs_files();
        self.last_read_ms = None;
        self.published_generation = self.generation.wrapping_sub(1);
    }
}

fn filesystem_kind(fstype: &str, mount: &str) -> Option<FilesystemKind> {
    if fstype == "fuse.lxcfs" {
        return None;
    }
    if matches!(
        fstype,
        "nfs"
            | "nfs4"
            | "cifs"
            | "smb"
            | "smbfs"
            | "smb3"
            | "sshfs"
            | "9p"
            | "ceph"
            | "glusterfs"
            | "afs"
            | "lustre"
            | "gpfs"
            | "davfs"
    ) || fstype == "fuse"
        || fstype == "fuseblk"
        || fstype.starts_with("fuse.")
    {
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
            | "nsfs"
            | "efivarfs"
            | "squashfs"
            | "selinuxfs"
            | "rpc_pipefs"
            | "nfsd"
            | "autofs"
            | "binfmt_misc"
            | "rootfs"
            | "devpts"
            | "ramfs"
    ) || (fstype == "overlay"
        && (mount == "/var/lib/containers"
            || mount.starts_with("/var/lib/containers/")
            || mount.contains("/containers/storage/")))
    {
        return None;
    }
    if fstype == "tmpfs" || fstype == "devtmpfs" || fstype == "overlay" {
        Some(FilesystemKind::Pseudo)
    } else {
        Some(FilesystemKind::Local)
    }
}

fn row_identity(row: &FilesystemRow) -> FilesystemIdentity {
    FilesystemIdentity {
        mount_point: row.mount_point.clone(),
        fstype: row.fstype.clone(),
        source: row.source.clone(),
        superblock_dev: row.superblock_dev.clone(),
        kind: row.kind,
        read_only: row.read_only,
    }
}

fn same_row_identity(left: &FilesystemRow, right: &FilesystemRow) -> bool {
    left.mount_point == right.mount_point
        && left.fstype == right.fstype
        && left.source == right.source
        && left.superblock_dev == right.superblock_dev
        && left.kind == right.kind
        && left.read_only == right.read_only
}

fn copy_numeric(target: &mut FilesystemRow, source: &FilesystemRow) {
    target.total_bytes = source.total_bytes;
    target.free_bytes = source.free_bytes;
    target.avail_bytes = source.avail_bytes;
    target.total_inodes = source.total_inodes;
    target.free_inodes = source.free_inodes;
    target.avail_inodes = source.avail_inodes;
    target.btrfs_unallocated = source.btrfs_unallocated;
}

#[cfg(test)]
#[path = "mounts_fs_tests.rs"]
mod tests;
