//! Linux disk counters and filesystem rows, kept separate from the sampler so
//! parsing and grouping remain deterministic and easy to carry into D-1.

use crate::DiskInfo;
#[cfg(any(target_os = "linux", test))]
use crate::DiskKind;
#[cfg(target_os = "linux")]
use std::time::Instant;

#[cfg(any(target_os = "linux", test))]
pub(crate) fn parse_diskstats_line(line: &[u8]) -> Option<(&[u8], u64, u64)> {
    let mut fields = line
        .split(|b| b.is_ascii_whitespace())
        .filter(|f| !f.is_empty());
    let _major = fields.next()?;
    let _minor = fields.next()?;
    let name = fields.next()?;
    let count = 3 + fields.clone().count();
    if !matches!(count, 14 | 18 | 20) {
        return None;
    }
    let read = std::str::from_utf8(fields.clone().nth(2)?)
        .ok()?
        .parse::<u64>()
        .ok()?;
    let written = std::str::from_utf8(fields.nth(6)?)
        .ok()?
        .parse::<u64>()
        .ok()?;
    Some((name, read, written))
}

#[cfg(any(target_os = "linux", test))]
pub(crate) fn sum_physical(buf: &[u8], names: &[String]) -> (u64, u64) {
    let mut read = 0u64;
    let mut written = 0u64;
    for line in buf.split(|b| *b == b'\n') {
        if let Some((name, r, w)) = parse_diskstats_line(line)
            && names.iter().any(|n| n.as_bytes() == name)
        {
            read = read.saturating_add(r.saturating_mul(512));
            written = written.saturating_add(w.saturating_mul(512));
        }
    }
    (read, written)
}

#[cfg(any(target_os = "linux", test))]
pub(crate) fn physical_disks(sys_block: &std::path::Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(sys_block) else {
        return Vec::new();
    };
    let mut disks = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let excluded = ["dm-", "md", "loop", "zram", "ram", "nbd"]
                .iter()
                .any(|p| name.starts_with(p));
            (!excluded && entry.path().join("device").exists()).then_some(name)
        })
        .collect::<Vec<_>>();
    disks.sort();
    disks
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg(any(target_os = "linux", test))]
pub(crate) struct MountRow {
    pub major_minor: String,
    pub mount: String,
    pub fstype: String,
    pub source: String,
}

#[cfg(any(target_os = "linux", test))]
fn unescape_mount(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 3 < bytes.len() {
            let code = &bytes[i + 1..i + 4];
            let decoded = match code {
                b"040" => Some(b' '),
                b"011" => Some(b'\t'),
                b"012" => Some(b'\n'),
                b"134" => Some(b'\\'),
                _ => None,
            };
            if let Some(byte) = decoded {
                out.push(byte);
                i += 4;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(any(target_os = "linux", test))]
pub(crate) fn parse_mountinfo(text: &str) -> Vec<MountRow> {
    text.lines()
        .filter_map(|line| {
            let (before, after) = line.split_once(" - ")?;
            let left = before.split_whitespace().collect::<Vec<_>>();
            let right = after.split_whitespace().collect::<Vec<_>>();
            Some(MountRow {
                major_minor: left.get(2)?.to_string(),
                mount: unescape_mount(left.get(4)?),
                fstype: right.first()?.to_string(),
                source: unescape_mount(right.get(1)?),
            })
        })
        .collect()
}

#[cfg(any(target_os = "linux", test))]
pub(crate) fn hidden(fstype: &str, mount: &str, source: &str) -> bool {
    matches!(
        fstype,
        "rootfs"
            | "sysfs"
            | "proc"
            | "devtmpfs"
            | "cgroup"
            | "cgroup2"
            | "pstore"
            | "squashfs"
            | "rpc_pipefs"
            | "iso9660"
            | "devpts"
            | "hugetlbfs"
            | "mqueue"
            | "tmpfs"
            | "cifs"
            | "nfs"
            | "nfs4"
            | "autofs"
    ) || (mount.starts_with("/sys/")
        || mount == "/sys"
        || mount.starts_with("/proc/")
        || mount == "/proc"
        || ((mount.starts_with("/run/") || mount == "/run") && !mount.starts_with("/run/media")))
        || source.starts_with("sunrpc")
}

#[cfg(any(target_os = "linux", test))]
pub(crate) fn group_mounts(rows: Vec<MountRow>) -> Vec<(MountRow, u32)> {
    let mut groups: Vec<(String, Vec<MountRow>)> = Vec::new();
    for row in rows
        .into_iter()
        .filter(|r| !hidden(&r.fstype, &r.mount, &r.source))
    {
        if let Some((_, rows)) = groups
            .iter_mut()
            .find(|(major_minor, _)| *major_minor == row.major_minor)
        {
            rows.push(row);
        } else {
            groups.push((row.major_minor.clone(), vec![row]));
        }
    }
    groups
        .into_iter()
        .filter_map(|(_, mut rows)| {
            rows.sort_by_key(|r| r.mount.len());
            let primary = rows.first()?.clone();
            Some((primary, rows.len().saturating_sub(1) as u32))
        })
        .collect()
}

#[cfg(any(target_os = "linux", test))]
pub(crate) fn rate(prev: (u64, u64), cur: (u64, u64), dt: f64) -> Option<(u64, u64)> {
    if dt < 0.001 || cur.0 < prev.0 || cur.1 < prev.1 {
        return None;
    }
    Some((
        ((cur.0 - prev.0) as f64 / dt) as u64,
        ((cur.1 - prev.1) as f64 / dt) as u64,
    ))
}

#[cfg(any(not(target_os = "linux"), test))]
pub(crate) fn dedupe_by_name(disks: Vec<DiskInfo>) -> Vec<DiskInfo> {
    let mut groups: std::collections::BTreeMap<String, Vec<DiskInfo>> =
        std::collections::BTreeMap::new();
    for disk in disks {
        groups.entry(disk.name.clone()).or_default().push(disk);
    }
    groups
        .into_values()
        .filter_map(|mut rows| {
            rows.sort_by_key(|d| d.mount.len());
            let mut primary = rows.first()?.clone();
            primary.extra_mounts = rows.len().saturating_sub(1) as u32;
            Some(primary)
        })
        .collect()
}

#[cfg(any(target_os = "linux", test))]
pub(crate) trait VolumeProbe {
    fn statvfs(&mut self, mount: &str) -> Option<(u64, u64)>;
    fn block_name(&mut self, source: &str) -> Option<String>;
    fn rotational(&mut self, block: &str) -> Option<bool>;
}

#[cfg(any(target_os = "linux", test))]
fn probe_mount_groups(
    groups: Vec<(MountRow, u32)>,
    probe: &mut dyn VolumeProbe,
) -> Vec<(MountRow, u32, u64, u64)> {
    groups
        .into_iter()
        .filter_map(|(row, extra)| {
            probe
                .statvfs(&row.mount)
                .map(|(total, avail)| (row, extra, total, avail))
        })
        .collect()
}

#[cfg(target_os = "linux")]
struct LinuxVolumeProbe;

#[cfg(target_os = "linux")]
impl VolumeProbe for LinuxVolumeProbe {
    fn statvfs(&mut self, mount: &str) -> Option<(u64, u64)> {
        crate::disk_space(std::path::Path::new(mount)).map(|(total, avail, _)| (total, avail))
    }
    fn block_name(&mut self, source: &str) -> Option<String> {
        let path = std::fs::canonicalize(source).ok()?;
        Some(
            path.strip_prefix("/dev/")
                .ok()?
                .to_string_lossy()
                .into_owned(),
        )
    }
    fn rotational(&mut self, block: &str) -> Option<bool> {
        let root = std::path::Path::new("/sys/class/block").join(block);
        std::fs::read_to_string(root.join("queue/rotational"))
            .or_else(|_| std::fs::read_to_string(root.join("../queue/rotational")))
            .ok()?
            .trim()
            .parse::<u8>()
            .ok()
            .map(|v| v != 0)
    }
}

#[cfg(target_os = "linux")]
pub(crate) struct DiskAggregate {
    diskstats: Option<std::fs::File>,
    mountinfo: Option<std::fs::File>,
    disk_buf: Vec<u8>,
    mount_buf: Vec<u8>,
    physical: Vec<String>,
    previous: Option<((u64, u64), Instant)>,
    device_previous: std::collections::BTreeMap<String, ((u64, u64), Instant)>,
    volume_devices: std::collections::BTreeMap<(String, String), (Option<String>, DiskKind)>,
    last_classify: Instant,
    probe: LinuxVolumeProbe,
    disks: std::sync::Arc<[DiskInfo]>,
}

#[cfg(target_os = "linux")]
impl DiskAggregate {
    pub(crate) fn new() -> Self {
        let now = Instant::now();
        Self {
            diskstats: std::fs::File::open("/proc/diskstats").ok(),
            mountinfo: std::fs::File::open("/proc/self/mountinfo").ok(),
            disk_buf: Vec::with_capacity(16 * 1024),
            mount_buf: Vec::with_capacity(32 * 1024),
            physical: physical_disks(std::path::Path::new("/sys/block")),
            previous: None,
            device_previous: std::collections::BTreeMap::new(),
            volume_devices: std::collections::BTreeMap::new(),
            last_classify: now,
            probe: LinuxVolumeProbe,
            disks: std::sync::Arc::from([]),
        }
    }

    fn read_file(file: &mut Option<std::fs::File>, buf: &mut Vec<u8>) -> bool {
        use std::os::unix::fs::FileExt;
        let Some(file) = file else { return false };
        buf.clear();
        loop {
            let mut chunk = [0u8; 8192];
            match file.read_at(&mut chunk, buf.len() as u64) {
                Ok(0) => return true,
                Ok(n) => {
                    buf.extend_from_slice(&chunk[..n]);
                }
                Err(_) => return false,
            }
        }
    }

    pub(crate) fn sample(
        &mut self,
        slow: bool,
        now: Instant,
    ) -> (Option<(u64, u64)>, std::sync::Arc<[DiskInfo]>) {
        if slow && now.duration_since(self.last_classify).as_secs() >= 60 {
            let next = physical_disks(std::path::Path::new("/sys/block"));
            if next != self.physical {
                self.physical = next;
                self.previous = None;
            }
            self.last_classify = now;
        }
        let io = if Self::read_file(&mut self.diskstats, &mut self.disk_buf)
            && !self.disk_buf.is_empty()
            && !self.physical.is_empty()
        {
            let current = sum_physical(&self.disk_buf, &self.physical);
            let result = self
                .previous
                .and_then(|(prev, at)| rate(prev, current, now.duration_since(at).as_secs_f64()));
            self.previous = Some((current, now));
            result
        } else {
            self.previous = None;
            None
        };
        if slow && Self::read_file(&mut self.mountinfo, &mut self.mount_buf) {
            let text = String::from_utf8_lossy(&self.mount_buf);
            let groups = probe_mount_groups(group_mounts(parse_mountinfo(&text)), &mut self.probe);
            let mut disks = Vec::with_capacity(groups.len());
            let mut seen_volume_keys = std::collections::BTreeSet::new();
            let mut seen_device_names = std::collections::BTreeSet::new();
            for (row, extra_mounts, total, avail) in groups {
                let cache_key = (row.major_minor.clone(), row.source.clone());
                seen_volume_keys.insert(cache_key.clone());
                let (block, kind) = if let Some(cached) = self.volume_devices.get(&cache_key) {
                    cached.clone()
                } else {
                    let block = self.probe.block_name(&row.source);
                    let kind = match block.as_deref().and_then(|b| self.probe.rotational(b)) {
                        Some(true) => DiskKind::Hdd,
                        Some(false) => DiskKind::Ssd,
                        None => DiskKind::Unknown,
                    };
                    self.volume_devices.insert(cache_key, (block.clone(), kind));
                    (block, kind)
                };
                let block_io = block.as_ref().and_then(|name| {
                    self.disk_buf.split(|b| *b == b'\n').find_map(|line| {
                        parse_diskstats_line(line)
                            .filter(|(disk, _, _)| *disk == name.as_bytes())
                            .map(|(_, r, w)| (r.saturating_mul(512), w.saturating_mul(512)))
                    })
                });
                let io_rate = block
                    .as_ref()
                    .and_then(|name| {
                        seen_device_names.insert(name.clone());
                        let current = block_io?;
                        let previous = self.device_previous.insert(name.clone(), (current, now));
                        previous.and_then(|(old, at)| {
                            rate(old, current, now.duration_since(at).as_secs_f64())
                        })
                    })
                    .unwrap_or((0, 0));
                disks.push(DiskInfo {
                    name: row.source,
                    mount: row.mount,
                    free_pct: if total > 0 {
                        ((avail as f64 / total as f64) * 100.0).round() as u8
                    } else {
                        0
                    }
                    .min(100),
                    read_bps: io_rate.0,
                    write_bps: io_rate.1,
                    kind,
                    extra_mounts,
                });
            }
            self.volume_devices
                .retain(|key, _| seen_volume_keys.contains(key));
            self.device_previous
                .retain(|name, _| seen_device_names.contains(name));
            self.disks = disks.into();
        }
        (io, std::sync::Arc::clone(&self.disks))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diskstats_parser_accepts_supported_shapes_and_rejects_bad_lines() {
        assert_eq!(
            parse_diskstats_line(b"259 0 nvme0n1 1 0 2 0 3 0 4 0 0 0 0"),
            Some((&b"nvme0n1"[..], 2, 4))
        );
        assert_eq!(
            parse_diskstats_line(b"259 0 nvme0n1 1 0 2 0 3 0 4 0 0 0 0 0 0 0 0"),
            Some((&b"nvme0n1"[..], 2, 4))
        );
        assert!(parse_diskstats_line(b"bad line").is_none());
        assert_eq!(
            parse_diskstats_line(b"8 0 sda 1 0 2 0 3 0 4 0 0 0 0 0 0 0 0 0 0")
                .map(|(_, r, w)| (r, w)),
            Some((2, 4))
        );
    }

    #[test]
    fn aggregate_counts_only_top_level_physical_disks() {
        let root = tempfile::tempdir().unwrap();
        for name in ["nvme0n1", "dm-0", "zram0", "loop0", "md0", "sda", "sdb"] {
            std::fs::create_dir_all(root.path().join(name)).unwrap();
        }
        std::fs::create_dir_all(root.path().join("nvme0n1/nvme0n1p5")).unwrap();
        std::fs::create_dir_all(root.path().join("sda/sda1")).unwrap();
        for name in ["nvme0n1", "md0", "sda"] {
            std::fs::create_dir_all(root.path().join(name).join("device")).unwrap();
        }
        assert_eq!(
            physical_disks(root.path()),
            vec!["nvme0n1".to_string(), "sda".to_string()]
        );
        let stats = b"259 0 nvme0n1 1 0 10 0 1 0 20 0 0 0 0 0 0 0 0 0 0\n259 5 nvme0n1p5 1 0 100 0 1 0 200 0 0 0 0 0 0 0 0 0 0\n253 0 dm-0 1 0 300 0 1 0 400 0 0 0 0 0 0 0 0 0 0\n252 0 zram0 1 0 500 0 1 0 600 0 0 0 0 0 0 0 0 0 0\n8 0 sda 1 0 30 0 1 0 40 0 0 0 0 0 0 0 0 0 0\n8 1 sda1 1 0 300 0 1 0 400 0 0 0 0 0 0 0 0 0 0";
        assert_eq!(
            sum_physical(stats, &physical_disks(root.path())),
            (40 * 512, 60 * 512)
        );
    }

    #[test]
    fn mountinfo_unescapes_and_groups_mounts() {
        let rows = parse_mountinfo(
            r"1 0 0:35 / /\040root rw shared:1 - btrfs /dev/mapper/crypt\040ed rw
2 0 0:35 /x /mnt/x rw - btrfs /dev/mapper/crypt\040ed rw",
        );
        assert_eq!(rows[0].mount, "/ root");
        let groups = group_mounts(rows);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].1, 1);
        let escaped = parse_mountinfo(r"3 0 8:1 / /a\040b\011c\012d\134e rw - ext4 /dev/sda1 rw");
        assert_eq!(escaped[0].mount, "/a b\tc\nd\\e");
        let optional = parse_mountinfo(
            "5 0 8:1 / /x rw shared:1 master:2 propagate_from:3 - ext4 /dev/sda1 rw",
        );
        assert_eq!(optional[0].major_minor, "8:1");
        assert_eq!(optional[0].mount, "/x");
        assert_eq!(optional[0].fstype, "ext4");
        assert_eq!(optional[0].source, "/dev/sda1");
    }

    #[test]
    fn forty_three_mounts_group_to_one_statvfs_probe() {
        struct CountingProbe(usize);
        impl VolumeProbe for CountingProbe {
            fn statvfs(&mut self, _: &str) -> Option<(u64, u64)> {
                self.0 += 1;
                Some((100, 50))
            }
            fn block_name(&mut self, _: &str) -> Option<String> {
                None
            }
            fn rotational(&mut self, _: &str) -> Option<bool> {
                None
            }
        }
        let mut text = String::new();
        for i in 0..43 {
            let mount = if i == 0 {
                "/".to_string()
            } else {
                format!("/btrfs/{i}")
            };
            text.push_str(&format!(
                "{i} 0 0:35 / {mount} rw - btrfs /dev/mapper/crypted rw\n"
            ));
        }
        text.push_str("50 0 259:2 / / rw - ext4 /dev/nvme0n1p2 rw\n44 0 259:1 / /boot rw - vfat /dev/nvme0n1p1 rw\n45 0 8:1 / /mnt/data rw - ext4 /dev/sda1 rw\n46 0 0:99 / /proc rw - proc proc rw\n47 0 0:100 / /sys rw - sysfs sysfs rw\n48 0 0:101 / /run/user rw - tmpfs tmpfs rw\n49 0 0:102 / /sys/fs/cgroup rw - cgroup2 cgroup rw\n");
        let groups = group_mounts(parse_mountinfo(&text));
        assert_eq!(groups.len(), 4);
        assert_eq!(groups[0].0.major_minor, "0:35");
        assert_eq!(groups[1].0.major_minor, "259:2");
        assert_eq!(groups[2].0.major_minor, "259:1");
        assert_eq!(groups[3].0.major_minor, "8:1");
        let btrfs = groups
            .iter()
            .find(|(row, _)| row.major_minor == "0:35")
            .unwrap();
        assert_eq!(btrfs.0.mount, "/");
        assert_eq!(btrfs.1, 42);
        let mut probe = CountingProbe(0);
        let measured = probe_mount_groups(groups, &mut probe);
        assert_eq!(measured.len(), 4);
        assert_eq!(probe.0, 4);
    }

    #[test]
    fn rates_reprime_on_decrease_or_short_interval() {
        assert_eq!(rate((1, 2), (3, 6), 2.0), Some((1, 2)));
        assert_eq!(rate((3, 2), (1, 6), 2.0), None);
        assert_eq!(rate((1, 2), (3, 6), 0.0005), None);
    }

    #[test]
    fn non_linux_dedupe_keeps_io_once_and_counts_mounts() {
        let row = |name: &str, mount: &str, read_bps| DiskInfo {
            name: name.into(),
            mount: mount.into(),
            free_pct: 50,
            read_bps,
            write_bps: read_bps,
            kind: DiskKind::Ssd,
            extra_mounts: 0,
        };
        let rows = dedupe_by_name(vec![
            row("a", "/long", 7),
            row("a", "/", 7),
            row("b", "/b", 9),
        ]);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].mount, "/");
        assert_eq!(rows[0].read_bps, 7);
        assert_eq!(rows[0].extra_mounts, 1);
    }

    #[test]
    fn snapshot_disks_are_shared_between_fast_ticks() {
        let mut sampler = crate::StatsSampler::new(std::env::temp_dir());
        let first = sampler.sample();
        let second = sampler.sample();
        let third = sampler.sample();
        assert!(std::sync::Arc::ptr_eq(&first.disks, &second.disks));
        assert!(std::sync::Arc::ptr_eq(&second.disks, &third.disks));
    }
}
