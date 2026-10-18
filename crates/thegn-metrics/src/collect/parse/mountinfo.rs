//! Borrowed parser and mount grouping helpers for `/proc/*/mountinfo`.
use super::{fields, parse_u64};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MountLine<'a> {
    pub mount_id: u32,
    pub parent_id: u32,
    pub major: u32,
    pub minor: u32,
    pub root: &'a [u8],
    pub mount_point: &'a [u8],
    pub options: &'a [u8],
    pub fstype: &'a [u8],
    pub source: &'a [u8],
    pub super_options: &'a [u8],
}

pub fn parse_line(line: &[u8]) -> Option<MountLine<'_>> {
    let sep = line.windows(3).position(|w| w == b" - ")?;
    let (left, right_with_sep) = line.split_at(sep);
    let right = &right_with_sep[3..];
    let mut left = fields(left);
    let mount_id = u32::try_from(parse_u64(left.next()?)?).ok()?;
    let parent_id = u32::try_from(parse_u64(left.next()?)?).ok()?;
    let (major, minor) = super::cgroup::maj_min(left.next()?)?;
    let root = left.next()?;
    let mount_point = left.next()?;
    let options = left.next()?;
    let mut right = fields(right);
    Some(MountLine {
        mount_id,
        parent_id,
        major,
        minor,
        root,
        mount_point,
        options,
        fstype: right.next()?,
        source: right.next()?,
        super_options: right.next()?,
    })
}

pub fn unescape_into(value: &[u8], out: &mut Vec<u8>) {
    out.clear();
    let mut i = 0;
    while i < value.len() {
        if value[i] == b'\\'
            && i + 3 < value.len()
            && let Some(byte) = match &value[i + 1..i + 4] {
                b"040" => Some(b' '),
                b"011" => Some(b'\t'),
                b"012" => Some(b'\n'),
                b"134" => Some(b'\\'),
                _ => None,
            }
        {
            out.push(byte);
            i += 4;
            continue;
        }
        out.push(value[i]);
        i += 1;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountRow {
    pub major_minor: String,
    pub mount: String,
    pub fstype: String,
    pub source: String,
}
pub fn parse_mountinfo(text: &str) -> Vec<MountRow> {
    text.lines()
        .filter_map(|line| {
            let row = parse_line(line.as_bytes())?;
            let mut mount = Vec::with_capacity(row.mount_point.len());
            unescape_into(row.mount_point, &mut mount);
            let mut source = Vec::with_capacity(row.source.len());
            unescape_into(row.source, &mut source);
            Some(MountRow {
                major_minor: format!("{}:{}", row.major, row.minor),
                mount: String::from_utf8_lossy(&mount).into_owned(),
                fstype: String::from_utf8_lossy(row.fstype).into_owned(),
                source: String::from_utf8_lossy(&source).into_owned(),
            })
        })
        .collect()
}
pub fn hidden(fstype: &str, mount: &str, source: &str) -> bool {
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
pub fn group_mounts(rows: Vec<MountRow>) -> Vec<(MountRow, u32)> {
    let mut groups: Vec<(String, Vec<MountRow>)> = Vec::new();
    for row in rows
        .into_iter()
        .filter(|r| !hidden(&r.fstype, &r.mount, &r.source))
    {
        if let Some((_, group)) = groups
            .iter_mut()
            .find(|(device, _)| *device == row.major_minor)
        {
            group.push(row);
        } else {
            groups.push((row.major_minor.clone(), vec![row]));
        }
    }
    groups
        .into_iter()
        .filter_map(|(_, mut group)| {
            group.sort_by_key(|r| r.mount.len());
            let first = group.first()?.clone();
            Some((first, group.len().saturating_sub(1) as u32))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{group_mounts, parse_line, parse_mountinfo, unescape_into};
    #[test]
    fn borrowed_parser_handles_optional_fields_escape_and_bad_prefixes() {
        let line = b"1 0 0:35 /@home /mnt/a\\040b rw - overlay overlay rw";
        let parsed = parse_line(line).unwrap();
        assert_eq!(parsed.mount_id, 1);
        assert_eq!(parsed.fstype, b"overlay");
        let optional =
            parse_line(b"2 1 8:1 / /mnt rw shared:1 master:2 propagate_from:3 - ext4 /dev/sda1 rw")
                .unwrap();
        assert_eq!(optional.mount_point, b"/mnt");
        assert_eq!(optional.super_options, b"rw");
        for end in 0..=line.len() {
            let _parsed = parse_line(&line[..end]);
        }
        assert!(parse_line(b"1 0 8:1 / / rw ext4 /dev/sda1 rw").is_none());
        let mut output = Vec::new();
        unescape_into(b"a\\040b\\011c\\012d\\134e\\999", &mut output);
        assert_eq!(output, b"a b\tc\nd\\e\\999");
        let rows =
            parse_mountinfo("1 0 0:35 /@home /a rw - btrfs dev rw\n2 0 0:35 / / rw - btrfs dev rw");
        let rows = group_mounts(rows);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].1, 1);
        let escaped =
            parse_mountinfo("3 0 8:1 / /mnt/a\\040(deleted) rw - ext4 /dev/disk\\040name rw");
        assert_eq!(escaped[0].mount, "/mnt/a (deleted)");
        assert_eq!(escaped[0].source, "/dev/disk name");
    }
}
