use super::{FilesystemIo, FilesystemsCollector, StatvfsData};
use crate::collect::{
    Collector, TickCtx, env::Env, frame::MonitorFrame, health::Health,
    model::filesystems::FilesystemKind,
};
use std::{
    collections::HashMap,
    io,
    path::{Path, PathBuf},
};

#[derive(Default)]
struct FakeIo {
    mountinfo: String,
    changed: bool,
    reads: usize,
    opens: usize,
    stat_calls: Vec<PathBuf>,
    stats: HashMap<PathBuf, Result<StatvfsData, io::ErrorKind>>,
    dirs: HashMap<PathBuf, Vec<PathBuf>>,
    text: HashMap<PathBuf, String>,
    canonical: HashMap<PathBuf, PathBuf>,
    opened_btrfs: Vec<PathBuf>,
}
impl FilesystemIo for FakeIo {
    fn open_mountinfo(&mut self, _path: &Path) -> io::Result<()> {
        self.opens += 1;
        Ok(())
    }
    fn mountinfo_changed(&mut self) -> io::Result<bool> {
        Ok(std::mem::take(&mut self.changed))
    }
    fn read_mountinfo(&mut self) -> io::Result<String> {
        self.reads += 1;
        Ok(self.mountinfo.clone())
    }
    fn statvfs(&mut self, mount: &Path) -> io::Result<StatvfsData> {
        self.stat_calls.push(mount.to_path_buf());
        match self.stats.get(mount) {
            Some(Ok(stats)) => Ok(*stats),
            Some(Err(kind)) => Err(io::Error::from(*kind)),
            None => Err(io::Error::from(io::ErrorKind::NotFound)),
        }
    }
    fn list(&mut self, path: &Path) -> io::Result<Vec<PathBuf>> {
        self.dirs
            .get(path)
            .cloned()
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
    }
    fn canonicalize(&mut self, path: &Path) -> io::Result<PathBuf> {
        self.canonical
            .get(path)
            .cloned()
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
    }
    fn clear_btrfs_files(&mut self) {
        self.opened_btrfs.clear();
    }
    fn open_btrfs_file(&mut self, path: &Path) -> io::Result<usize> {
        let handle = self.opened_btrfs.len();
        self.opened_btrfs.push(path.to_path_buf());
        Ok(handle)
    }
    fn read_btrfs_file(&mut self, handle: usize, buffer: &mut [u8]) -> io::Result<usize> {
        let path = self
            .opened_btrfs
            .get(handle)
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
        let text = self
            .text
            .get(path)
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
        let bytes = text.as_bytes();
        let count = bytes.len().min(buffer.len());
        buffer[..count].copy_from_slice(&bytes[..count]);
        Ok(count)
    }
}

fn mount(id: usize, dev: &str, point: &str, fstype: &str, source: &str, options: &str) -> String {
    format!("{id} 1 {dev} / {point} {options} - {fstype} {source} {options}\n")
}
fn make_collector(io: FakeIo) -> FilesystemsCollector<FakeIo> {
    FilesystemsCollector::with_io(io, "/fake/mountinfo", "/fake/btrfs")
}
fn sample(collector: &mut impl Collector, at_ms: u64) -> MonitorFrame {
    let mut frame = MonitorFrame::default();
    collector
        .sample(
            &TickCtx {
                tick: 0,
                now_ms: at_ms,
            },
            &mut frame,
        )
        .unwrap();
    frame
}

#[test]
fn classifies_remote_filesystems_and_drops_kernel_and_container_mounts() {
    for fstype in [
        "nfs",
        "nfs4",
        "cifs",
        "smb",
        "smbfs",
        "smb3",
        "sshfs",
        "9p",
        "ceph",
        "glusterfs",
        "afs",
        "lustre",
        "gpfs",
        "davfs",
        "virtiofs",
        "coda",
        "ocfs2",
        "gfs2",
        "orangefs",
        "pvfs2",
        "beegfs",
        "fuse",
        "fuseblk",
        "fuse.portal",
        "fuse.gvfsd-fuse",
    ] {
        assert_eq!(
            super::filesystem_kind(fstype, "/mnt/remote"),
            Some(FilesystemKind::Network)
        );
    }
    for (fstype, mount_point) in [
        ("fusectl", "/sys/fs/fuse/connections"),
        ("fuse.lxcfs", "/var/lib/lxcfs"),
        ("nsfs", "/run/netns"),
        ("efivarfs", "/sys/firmware/efi/efivars"),
        ("squashfs", "/snap/core"),
        ("selinuxfs", "/sys/fs/selinux"),
        ("rpc_pipefs", "/var/lib/nfs/rpc_pipefs"),
        ("nfsd", "/proc/fs/nfsd"),
        (
            "overlay",
            "/home/user/.local/share/containers/storage/overlay/id/merged",
        ),
    ] {
        assert_eq!(super::filesystem_kind(fstype, mount_point), None);
    }
}

#[test]
fn added_network_filesystems_are_listed_without_statvfs_calls() {
    let types = [
        "virtiofs", "coda", "ocfs2", "gfs2", "orangefs", "pvfs2", "beegfs",
    ];
    let io = FakeIo {
        mountinfo: types
            .iter()
            .enumerate()
            .map(|(index, fstype)| {
                mount(
                    index + 1,
                    &format!("0:{index}"),
                    &format!("/mnt/{index}"),
                    fstype,
                    "remote",
                    "rw",
                )
            })
            .collect(),
        ..FakeIo::default()
    };
    let mut collector = make_collector(io);
    assert_eq!(collector.discover(&Env::default()), Health::Ok);
    let frame = sample(&mut collector, 40_000);
    assert_eq!(frame.parts.filesystems.rows.len(), types.len());
    assert!(
        frame
            .parts
            .filesystems
            .rows
            .iter()
            .all(|row| { row.kind == FilesystemKind::Network && row.total_bytes.is_none() })
    );
    assert!(collector.io.stat_calls.is_empty());
}

#[test]
fn deduplicates_superblocks_filters_pseudo_keeps_network_without_statvfs_and_reuses_mountinfo() {
    let mut io = FakeIo {
        mountinfo: [
            mount(1, "8:1", "/data", "ext4", "/dev/a", "rw"),
            mount(2, "8:1", "/bind", "ext4", "/dev/a", "rw"),
            mount(3, "0:5", "/proc", "proc", "proc", "rw"),
            mount(4, "0:6", "/tmp", "tmpfs", "tmpfs", "rw"),
            mount(5, "0:7", "/run", "tmpfs", "tmpfs", "rw"),
            mount(6, "0:8", "/run/user/1000", "tmpfs", "tmpfs", "rw"),
            mount(7, "0:9", "/network", "nfs4", "host:/export", "ro"),
            mount(8, "0:10", "/portal", "fuse.portal", "portal", "rw"),
            mount(9, "0:11", "/gvfs", "fuse.gvfsd-fuse", "gvfs", "rw"),
            mount(10, "0:12", "/block", "fuseblk", "/dev/fuse", "rw"),
            mount(
                11,
                "0:13",
                "/home/user/.local/share/containers/storage/overlay/id/merged",
                "overlay",
                "overlay",
                "rw",
            ),
        ]
        .concat(),
        ..FakeIo::default()
    };
    io.stats.insert(
        PathBuf::from("/data"),
        Ok(StatvfsData {
            blocks: 20,
            blocks_free: 8,
            blocks_available: 6,
            fragment_size: 4096,
            files: 100,
            files_free: 40,
            files_available: 30,
        }),
    );
    io.stats
        .insert(PathBuf::from("/tmp"), Err(io::ErrorKind::PermissionDenied));
    io.stats
        .insert(PathBuf::from("/run/user/1000"), Ok(StatvfsData::default()));
    let mut collector = make_collector(io);
    assert_eq!(collector.discover(&Env::default()), Health::Ok);
    let frame = sample(&mut collector, 40_000);
    let part = frame.parts.filesystems;
    assert_eq!(part.rows.len(), 7);
    assert_eq!(part.rows[0].mount_point, "/data");
    assert_eq!(part.rows[0].total_bytes, Some(81_920));
    assert_eq!(part.rows[0].free_bytes, Some(32_768));
    assert_eq!(part.rows[0].avail_bytes, Some(24_576));
    assert_eq!(part.rows[0].total_inodes, Some(100));
    assert_eq!(part.rows[0].free_inodes, Some(40));
    assert_eq!(part.rows[0].avail_inodes, Some(30));
    assert_eq!(part.rows[1].mount_point, "/tmp");
    assert_eq!(part.rows[1].kind, FilesystemKind::Pseudo);
    assert_eq!(part.rows[1].total_bytes, None);
    assert_eq!(part.rows[2].mount_point, "/run/user/1000");
    assert_eq!(part.rows[3].mount_point, "/network");
    assert_eq!(part.rows[3].kind, FilesystemKind::Network);
    assert!(part.rows[3].read_only);
    assert_eq!(part.rows[3].total_bytes, None);
    let expected_stat_calls = [
        PathBuf::from("/data"),
        PathBuf::from("/tmp"),
        PathBuf::from("/run/user/1000"),
    ];
    assert_eq!(
        collector.io.stat_calls.as_slice(),
        expected_stat_calls.as_slice()
    );
    assert_eq!(collector.io.reads, 1);
    let _next = sample(&mut collector, 40_200);
    assert_eq!(collector.io.reads, 1);
}

#[test]
fn counts_further_mounts_for_each_kept_superblock_row() {
    let io = FakeIo {
        mountinfo: [
            mount(1, "8:1", "/data", "ext4", "/dev/a", "rw"),
            mount(2, "8:1", "/bind-a", "ext4", "/dev/a", "rw"),
            mount(3, "8:1", "/bind-b", "ext4", "/dev/a", "rw"),
            mount(4, "8:2", "/other", "ext4", "/dev/b", "rw"),
        ]
        .concat(),
        ..FakeIo::default()
    };
    let mut collector = make_collector(io);
    assert_eq!(collector.discover(&Env::default()), Health::Ok);

    let frame = sample(&mut collector, 40_000);
    assert_eq!(frame.parts.filesystems.rows.len(), 2);
    assert_eq!(frame.parts.filesystems.rows[0].mount_point, "/data");
    assert_eq!(frame.parts.filesystems.rows[0].extra_mounts, 2);
    assert_eq!(frame.parts.filesystems.rows[1].mount_point, "/other");
    assert_eq!(frame.parts.filesystems.rows[1].extra_mounts, 0);
}

#[test]
fn btrfs_unallocated_uses_device_size_minus_allocation_totals_and_missing_tree_is_absent() {
    let uuid = "aabbcc";
    let mountinfo = mount(1, "0:34", "/btrfs", "btrfs", "/dev/mapper/crypted", "rw");
    let mut io = FakeIo {
        mountinfo,
        ..FakeIo::default()
    };
    let temp = tempfile::tempdir().unwrap();
    let sys = temp.path().join("sys/fs/btrfs");
    let uuid_dir = sys.join(uuid);
    let devices = uuid_dir.join("devices");
    let device = devices.join("dm-0");
    io.dirs.insert(sys.clone(), vec![uuid_dir.clone()]);
    io.dirs.insert(devices.clone(), vec![device.clone()]);
    io.canonical.insert(
        PathBuf::from("/dev/mapper/crypted"),
        temp.path().join("sys/devices/dm-0"),
    );
    io.text.insert(device.join("size"), "1000\n".to_owned());
    let allocation = uuid_dir.join("allocation");
    for (class, total, disk_total) in [
        ("data", "100\n", "100\n"),
        ("metadata", "20\n", "40\n"),
        ("system", "5\n", "10\n"),
    ] {
        let class_path = allocation.join(class);
        io.text
            .insert(class_path.join("total_bytes"), total.to_owned());
        io.text
            .insert(class_path.join("disk_total"), disk_total.to_owned());
        io.text
            .insert(class_path.join("bytes_used"), "1\n".to_owned());
    }
    let mut collector = FilesystemsCollector::with_io(io, "/fake/mountinfo", &sys);
    assert_eq!(collector.discover(&Env::default()), Health::Ok);
    assert_eq!(
        sample(&mut collector, 1).parts.filesystems.rows[0].btrfs_unallocated,
        Some(511_850)
    );

    let missing_uuid = sys.join(uuid);
    let missing_devices = missing_uuid.join("devices");
    let missing_device = missing_devices.join("dm-0");
    let mut missing_io = FakeIo {
        mountinfo: mount(1, "0:34", "/btrfs", "btrfs", "/dev/mapper/crypted", "rw"),
        ..FakeIo::default()
    };
    missing_io.dirs.insert(sys.clone(), vec![missing_uuid]);
    missing_io
        .dirs
        .insert(missing_devices, vec![missing_device]);
    missing_io.canonical.insert(
        PathBuf::from("/dev/mapper/crypted"),
        temp.path().join("sys/devices/dm-0"),
    );
    let mut missing = FilesystemsCollector::with_io(missing_io, "/fake/mountinfo", &sys);
    assert_eq!(missing.discover(&Env::default()), Health::Ok);
    assert_eq!(
        sample(&mut missing, 1).parts.filesystems.rows[0].btrfs_unallocated,
        None
    );
}

#[test]
fn truncates_after_sixty_four_unique_superblocks_and_backstop_rereads() {
    let mountinfo = (0..65)
        .map(|index| {
            mount(
                index + 1,
                &format!("8:{index}"),
                &format!("/mnt/{index}"),
                "ext4",
                "/dev/a",
                "rw",
            )
        })
        .collect::<String>();
    let mut collector = make_collector(FakeIo {
        mountinfo,
        ..FakeIo::default()
    });
    assert_eq!(collector.discover(&Env::default()), Health::Ok);
    let frame = sample(&mut collector, 1);
    assert_eq!(frame.parts.filesystems.rows.len(), 64);
    assert!(frame.parts.filesystems.truncated);
    let _frame = sample(&mut collector, 30_001);
    assert_eq!(collector.io.reads, 2);
}
