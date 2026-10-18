use super::*;
use std::collections::HashMap;

#[derive(Default)]
struct FakeSysfs {
    files: HashMap<PathBuf, Vec<u8>>,
}
impl FakeSysfs {
    fn put(&mut self, path: PathBuf, value: &str) {
        self.files.insert(path, value.as_bytes().to_vec());
    }
}
impl SysfsView for FakeSysfs {
    fn read(&mut self, path: &Path) -> io::Result<Vec<u8>> {
        self.files
            .get(path)
            .cloned()
            .ok_or_else(|| io::ErrorKind::NotFound.into())
    }
    fn list(&mut self, path: &Path) -> io::Result<Vec<PathBuf>> {
        let mut children = self
            .files
            .keys()
            .filter_map(|file| {
                file.strip_prefix(path)
                    .ok()?
                    .components()
                    .next()
                    .map(|part| path.join(part))
            })
            .collect::<Vec<_>>();
        children.sort();
        children.dedup();
        if children.is_empty() {
            Err(io::ErrorKind::NotFound.into())
        } else {
            Ok(children)
        }
    }
}

fn set_cpu(
    fs: &mut FakeSysfs,
    root: &Path,
    id: u16,
    core_id: u16,
    package: u16,
    siblings: &str,
    l3: &str,
) {
    let topology = root.join(format!("devices/system/cpu/cpu{id}/topology"));
    fs.put(
        topology.join("physical_package_id"),
        &format!("{package}\n"),
    );
    fs.put(topology.join("die_id"), "0\n");
    fs.put(topology.join("core_id"), &format!("{core_id}\n"));
    fs.put(
        topology.join("thread_siblings_list"),
        &format!("{siblings}\n"),
    );
    fs.put(
        root.join(format!(
            "devices/system/cpu/cpu{id}/cache/index3/shared_cpu_list"
        )),
        &format!("{l3}\n"),
    );
}

fn topology_fixture(kind: &str) -> (FakeSysfs, PathBuf) {
    let root = PathBuf::from("/sys-fake");
    let mut fs = FakeSysfs::default();
    let (count, online) = if kind == "amd" {
        (24, "0-23")
    } else {
        (4, "0-3")
    };
    fs.put(
        root.join("devices/system/cpu/online"),
        &format!("{online}\n"),
    );
    for cpu in 0..count {
        let (siblings, core, l3) = if kind == "amd" {
            if cpu < 4 || (12..16).contains(&cpu) {
                (
                    format!("{},{}", cpu % 12, cpu % 12 + 12),
                    if cpu < 4 { cpu } else { cpu - 12 },
                    "0-3,12-15",
                )
            } else {
                (
                    format!(
                        "{},{}",
                        if cpu < 12 { cpu } else { cpu - 12 },
                        if cpu < 12 { cpu + 12 } else { cpu }
                    ),
                    if cpu < 12 { cpu + 4 } else { cpu - 8 },
                    "4-11,16-23",
                )
            }
        } else if cpu < 2 {
            ("0-1".to_owned(), cpu, "0-1")
        } else {
            ("2-3".to_owned(), cpu - 2, "2-3")
        };
        set_cpu(&mut fs, &root, cpu, core, 0, &siblings, l3);
    }
    match kind {
        "intel" => {
            fs.put(root.join("devices/cpu_core/cpus"), "0-1\n");
            fs.put(root.join("devices/cpu_atom/cpus"), "2-3\n");
        }
        "arm" => {
            for cpu in 0..4 {
                fs.put(
                    root.join(format!("devices/system/cpu/cpu{cpu}/cpu_capacity")),
                    if cpu < 2 { "1024\n" } else { "512\n" },
                );
            }
        }
        "amd" => {
            let cpufreq = root.join("devices/system/cpu/cpufreq");
            for cpu in 0..24 {
                let (freq, perf) = if cpu < 4 || (12..16).contains(&cpu) {
                    ("5157895", "196")
                } else {
                    ("3289474", "125")
                };
                let path = cpufreq.join(format!("policy{cpu}"));
                fs.put(path.join("related_cpus"), &format!("{cpu}\n"));
                fs.put(path.join("cpuinfo_max_freq"), &format!("{freq}\n"));
                fs.put(path.join("amd_pstate_highest_perf"), &format!("{perf}\n"));
            }
        }
        _ => (),
    }
    (fs, root)
}

#[test]
fn fake_sysfs_classifies_intel_arm_amd_and_uniform_without_cpufreq() {
    for (fixture, expected) in [
        (
            "intel",
            vec![
                CoreType::Performance,
                CoreType::Performance,
                CoreType::Efficiency,
                CoreType::Efficiency,
            ],
        ),
        (
            "arm",
            vec![
                CoreType::Performance,
                CoreType::Performance,
                CoreType::Efficiency,
                CoreType::Efficiency,
            ],
        ),
        ("vm", vec![CoreType::Uniform; 4]),
    ] {
        let (fs, root) = topology_fixture(fixture);
        let mut collector = CpuTopologyCollector::with_view(fs, &root);
        assert_eq!(collector.discover(&Env::default()), Health::Ok);
        assert_eq!(collector.cadence(), Cadence::Every(30));
        let mut frame = MonitorFrame::default();
        collector
            .sample(
                &TickCtx {
                    tick: 1,
                    now_ms: 1000,
                },
                &mut frame,
            )
            .unwrap();
        assert_eq!(
            frame
                .parts
                .cpu_topology
                .cpus
                .iter()
                .map(|cpu| cpu.core_type)
                .collect::<Vec<_>>(),
            expected,
            "{fixture}"
        );
        assert_eq!(
            frame.parts.cpu_topology.l3_groups,
            vec![vec![0, 1], vec![2, 3]]
        );
        assert_eq!(frame.parts.cpu_topology.packages, vec![vec![0, 1, 2, 3]]);
        assert_eq!(frame.parts.cpu_topology.dies, vec![vec![0, 1, 2, 3]]);
        if fixture == "vm" {
            assert_eq!(frame.parts.cpu_topology.cpus[0].siblings, vec![0, 1]);
            assert_eq!(
                frame.parts.cpu_topology.cpus[0].core_type,
                CoreType::Uniform
            );
        }
    }
    let (fs, root) = topology_fixture("amd");
    let mut collector = CpuTopologyCollector::with_view(fs, &root);
    assert_eq!(collector.discover(&Env::default()), Health::Ok);
    assert_eq!(collector.cadence(), Cadence::Every(30));
    let mut frame = MonitorFrame::default();
    collector
        .sample(
            &TickCtx {
                tick: 1,
                now_ms: 1000,
            },
            &mut frame,
        )
        .unwrap();
    let performance: Vec<_> = frame
        .parts
        .cpu_topology
        .cpus
        .iter()
        .filter(|cpu| cpu.core_type == CoreType::Performance)
        .map(|cpu| cpu.id)
        .collect();
    let efficiency: Vec<_> = frame
        .parts
        .cpu_topology
        .cpus
        .iter()
        .filter(|cpu| cpu.core_type == CoreType::Efficiency)
        .map(|cpu| cpu.id)
        .collect();
    assert_eq!(performance, vec![0, 1, 2, 3, 12, 13, 14, 15]);
    assert_eq!(frame.parts.cpu_topology.cpus[0].siblings, vec![0, 12]);
    assert_eq!(
        efficiency,
        vec![4, 5, 6, 7, 8, 9, 10, 11, 16, 17, 18, 19, 20, 21, 22, 23]
    );
    assert_eq!(
        frame.parts.cpu_topology.l3_groups,
        vec![
            vec![0, 1, 2, 3, 12, 13, 14, 15],
            vec![4, 5, 6, 7, 8, 9, 10, 11, 16, 17, 18, 19, 20, 21, 22, 23]
        ]
    );
}

#[test]
fn online_change_rescans_and_removes_cpu_from_topology_and_l3() {
    let (fs, root) = topology_fixture("amd");
    let online = root.join("devices/system/cpu/online");
    let mut collector = CpuTopologyCollector::with_view(fs, &root);
    assert_eq!(collector.discover(&Env::default()), Health::Ok);
    collector.view.files.insert(online, b"0-22\n".to_vec());
    let mut frame = MonitorFrame::default();
    collector
        .sample(
            &TickCtx {
                tick: 30,
                now_ms: 30_000,
            },
            &mut frame,
        )
        .unwrap();
    assert_eq!(
        frame
            .parts
            .cpu_topology
            .cpus
            .iter()
            .map(|cpu| cpu.id)
            .collect::<Vec<_>>(),
        (0..23).collect::<Vec<_>>()
    );
    assert_eq!(
        frame.parts.cpu_topology.l3_groups,
        vec![
            vec![0, 1, 2, 3, 12, 13, 14, 15],
            (4..12).chain(16..23).collect()
        ]
    );
}
