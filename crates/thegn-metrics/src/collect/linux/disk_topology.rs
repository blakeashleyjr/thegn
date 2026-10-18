//! Linux block-device topology from `/sys/block`.
use crate::collect::{
    model::diskstats::DevKind,
    parse::{cgroup::maj_min, diskstats::is_virtual_name},
};
use std::path::Path;
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopoDev {
    pub name: Arc<str>,
    pub major: u32,
    pub minor: u32,
    pub kind: DevKind,
    pub physical: bool,
    pub parent: Option<usize>,
    pub slaves: Vec<usize>,
    pub label: Option<Arc<str>>,
}
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Topology {
    pub devs: Vec<TopoDev>,
    pub rev: u32,
}

fn text(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_owned())
}
fn kind(name: &str) -> DevKind {
    if partition {
        DevKind::Partition
    } else if name.starts_with("dm-") {
        DevKind::Dm
    } else if name.starts_with("md") {
        DevKind::Md
    } else if name.starts_with("loop") {
        DevKind::Loop
    } else if name.starts_with("zram") {
        DevKind::Zram
    } else {
        DevKind::Disk
    }
}
pub fn scan(sys_block: &Path, rev: u32) -> Topology {
    let mut topo = Topology {
        devs: Vec::new(),
        rev,
    };
    let Ok(entries) = std::fs::read_dir(sys_block) else {
        return topo;
    };
    let mut dirs: Vec<_> = entries.flatten().collect();
    dirs.sort_by_key(|e| e.file_name());
    for entry in dirs {
        let name = entry.file_name().to_string_lossy().into_owned();
        let root = entry.path();
        let (major, minor) = text(&root.join("dev"))
            .and_then(|value| maj_min(value.as_bytes()))
            .unwrap_or((0, 0));
        let disk_idx = topo.devs.len();
        let label = text(&root.join("dm/name")).map(Arc::<str>::from);
        let parent_disk = text(&root.join("partition")).is_some();
        topo.devs.push(TopoDev {
            name: Arc::from(name.as_str()),
            major,
            minor,
            kind: kind(&name),
            physical: !parent_disk
                && root.join("device").exists()
                && !is_virtual_name(name.as_bytes()),
            parent: None,
            slaves: Vec::new(),
            label,
        });
        let Ok(children) = std::fs::read_dir(&root) else {
            continue;
        };
        let mut children: Vec<_> = children.flatten().collect();
        children.sort_by_key(|e| e.file_name());
        for child in children {
            let child_name = child.file_name().to_string_lossy().into_owned();
            let child_path = child.path();
            if !child_path.join("partition").exists() {
                continue;
            }
            let (major, minor) = text(&child_path.join("dev"))
                .and_then(|value| maj_min(value.as_bytes()))
                .unwrap_or((0, 0));
            topo.devs.push(TopoDev {
                name: Arc::from(child_name.as_str()),
                major,
                minor,
                kind: DevKind::Partition,
                physical: false,
                parent: Some(disk_idx),
                slaves: Vec::new(),
                label: None,
            });
        }
    }
    let names: std::collections::HashMap<_, _> = topo
        .devs
        .iter()
        .enumerate()
        .map(|(i, d)| (d.name.to_string(), i))
        .collect();
    for index in 0..topo.devs.len() {
        let sys_path = sys_block.join(topo.devs[index].name.as_ref());
        let Ok(slaves) = std::fs::read_dir(sys_path.join("slaves")) else {
            continue;
        };
        topo.devs[index].slaves = slaves
            .flatten()
            .filter_map(|e| names.get(e.file_name().to_string_lossy().as_ref()).copied())
            .collect();
        topo.devs[index].parent = topo.devs[index]
            .slaves
            .first()
            .copied()
            .or(topo.devs[index].parent);
    }
    topo
}
pub fn physical_disks(sys_block: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(sys_block) else {
        return Vec::new();
    };
    let mut names: Vec<_> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            (!is_virtual_name(name.as_bytes()) && entry.path().join("device").exists())
                .then_some(name)
        })
        .collect();
    names.sort();
    names
}

#[cfg(test)]
mod tests {
    use super::{DevKind, physical_disks, scan};
    use std::fs;

    fn attr(root: &std::path::Path, path: &str, value: &str) {
        let target = root.join(path);
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(target, value).unwrap();
    }

    #[test]
    fn scans_partitions_slaves_physical_rule_and_degraded_dev() {
        let root = tempfile::tempdir().unwrap();
        for (disk, dev) in [
            ("nvme0n1", "259:0\n"),
            ("sda", "8:0\n"),
            ("dm-0", "254:0\n"),
            ("md0", "9:0\n"),
            ("loop0", "7:0\n"),
            ("zram0", "253:0\n"),
            ("broken", ""),
        ] {
            let base = root.path().join(disk);
            fs::create_dir_all(&base).unwrap();
            if !dev.is_empty() {
                fs::write(base.join("dev"), dev).unwrap();
            }
            if ["nvme0n1", "sda", "md0"].contains(&disk) {
                fs::create_dir_all(base.join("device")).unwrap();
            }
        }
        for (partition, dev) in [("p1", "259:1\n"), ("p5", "259:5\n")] {
            let dir = root
                .path()
                .join("nvme0n1")
                .join(format!("nvme0n1{partition}"));
            fs::create_dir_all(&dir).unwrap();
            fs::write(
                dir.join("partition"),
                if partition == "p1" { "1\n" } else { "5\n" },
            )
            .unwrap();
            fs::write(dir.join("dev"), dev).unwrap();
        }
        let dm = root.path().join("dm-0");
        fs::create_dir_all(dm.join("slaves/nvme0n1p5")).unwrap();
        attr(root.path(), "dm-0/dm/name", "crypted\n");
        fs::create_dir_all(root.path().join("sda/sda1")).unwrap();
        fs::write(root.path().join("sda/sda1/partition"), "1\n").unwrap();
        fs::write(root.path().join("sda/sda1/dev"), "8:1\n").unwrap();
        let topology = scan(root.path(), 7);
        assert_eq!(topology.rev, 7);
        assert_eq!(physical_disks(root.path()), vec!["nvme0n1", "sda"]);
        let p5 = topology
            .devs
            .iter()
            .position(|dev| dev.name.as_ref() == "nvme0n1p5")
            .unwrap();
        let nvme = topology
            .devs
            .iter()
            .position(|dev| dev.name.as_ref() == "nvme0n1")
            .unwrap();
        assert_eq!(topology.devs[p5].parent, Some(nvme));
        let dm = topology
            .devs
            .iter()
            .find(|dev| dev.name.as_ref() == "dm-0")
            .unwrap();
        assert_eq!(dm.label.as_deref(), Some("crypted"));
        assert_eq!(dm.slaves, vec![p5]);
        assert_eq!(
            topology
                .devs
                .iter()
                .find(|dev| dev.name.as_ref() == "dm-0")
                .unwrap()
                .kind,
            DevKind::Dm
        );
        let broken = topology
            .devs
            .iter()
            .find(|dev| dev.name.as_ref() == "broken")
            .unwrap();
        assert_eq!((broken.major, broken.minor), (0, 0));
    }
}
