//! Linux CPU topology discovery with online-set refreshes.

use crate::collect::frame::Part as _;
use std::{
    io,
    path::{Path, PathBuf},
};

use crate::collect::{
    CollectError, Collector, CollectorId, TickCtx,
    env::Env,
    frame::MonitorFrame,
    health::{Absent, Health},
    model::cpu_topology::{CpuTopologyEntry, CpuTopologyPart},
    policy::cpu_topology::{
        CoreType, CpuClassInput, classify_core_types, effective_cpus, l3_groups, parse_cpulist,
    },
    schedule::Cadence,
};

pub trait SysfsView: Send {
    fn read(&mut self, path: &Path) -> io::Result<Vec<u8>>;
    fn list(&mut self, path: &Path) -> io::Result<Vec<PathBuf>>;
}

pub struct RealSysfs;
impl SysfsView for RealSysfs {
    fn read(&mut self, path: &Path) -> io::Result<Vec<u8>> {
        std::fs::read(path)
    }
    fn list(&mut self, path: &Path) -> io::Result<Vec<PathBuf>> {
        std::fs::read_dir(path)?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect()
    }
}

pub struct CpuTopologyCollector<V: SysfsView = RealSysfs> {
    view: V,
    root: PathBuf,
    online: Vec<u16>,
    part: CpuTopologyPart,
}
impl CpuTopologyCollector<RealSysfs> {
    pub fn new() -> Self {
        Self::with_root("/sys")
    }
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        Self::with_view(RealSysfs, root)
    }
}
impl Default for CpuTopologyCollector<RealSysfs> {
    fn default() -> Self {
        Self::new()
    }
}
impl<V: SysfsView> CpuTopologyCollector<V> {
    pub fn with_view(view: V, root: impl Into<PathBuf>) -> Self {
        Self {
            view,
            root: root.into(),
            online: Vec::new(),
            part: CpuTopologyPart::default(),
        }
    }
    fn path(&self, suffix: &str) -> PathBuf {
        self.root.join(suffix)
    }
    fn text(&mut self, path: &Path) -> Option<String> {
        self.view
            .read(path)
            .ok()
            .and_then(|bytes| String::from_utf8(bytes).ok())
            .map(|value| value.trim().to_owned())
    }
    fn scan(&mut self, online: Vec<u16>) {
        let cpu_root = self.path("devices/system/cpu");
        let topology_root = cpu_root.clone();
        let mut classes = Vec::new();
        let core_path = self.path("devices/cpu_core/cpus");
        let atom_path = self.path("devices/cpu_atom/cpus");
        let core_list = self.text(&core_path);
        let atom_list = self.text(&atom_path);
        let core_cpus = core_list
            .as_deref()
            .and_then(parse_cpulist)
            .unwrap_or_default();
        let atom_cpus = atom_list
            .as_deref()
            .and_then(parse_cpulist)
            .unwrap_or_default();
        let mut policy_values = Vec::new();
        let policies = self
            .view
            .list(&cpu_root.join("cpufreq"))
            .unwrap_or_default();
        for policy in policies {
            let name = policy
                .file_name()
                .and_then(|part| part.to_str())
                .unwrap_or("");
            if !name.starts_with("policy") {
                continue;
            }
            let related = self
                .text(&policy.join("related_cpus"))
                .or_else(|| self.text(&policy.join("affected_cpus")));
            let members = related
                .as_deref()
                .and_then(parse_cpulist)
                .unwrap_or_default();
            let freq = self
                .text(&policy.join("cpuinfo_max_freq"))
                .and_then(|value| value.parse::<u64>().ok());
            let perf = self
                .text(&policy.join("amd_pstate_highest_perf"))
                .and_then(|value| value.parse::<u32>().ok());
            for cpu in members {
                policy_values.push((cpu, freq, perf));
            }
        }
        let mut topology_entries = Vec::new();
        let mut l3_input = Vec::new();
        for cpu in &online {
            let cpu_dir = topology_root.join(format!("cpu{cpu}"));
            let topology = cpu_dir.join("topology");
            let input = CpuClassInput {
                intel_core: core_cpus.contains(cpu),
                intel_atom: atom_cpus.contains(cpu),
                arm_capacity: self
                    .text(&cpu_dir.join("cpu_capacity"))
                    .and_then(|value| value.parse().ok()),
                amd_max_freq_khz: policy_values
                    .iter()
                    .find(|(id, _, _)| id == cpu)
                    .and_then(|(_, freq, _)| *freq),
                amd_highest_perf: policy_values
                    .iter()
                    .find(|(id, _, _)| id == cpu)
                    .and_then(|(_, _, perf)| *perf),
            };
            classes.push((*cpu, input));
            let cache_root = cpu_dir.join("cache");
            let caches = self.view.list(&cache_root).unwrap_or_default();
            for cache in caches {
                if cache.file_name().and_then(|part| part.to_str()) == Some("index3")
                    && let Some(list) = self
                        .text(&cache.join("shared_cpu_list"))
                        .and_then(|value| parse_cpulist(&value))
                {
                    l3_input.push((*cpu, list));
                }
            }
            topology_entries.push(CpuTopologyEntry {
                id: *cpu,
                package: self
                    .text(&topology.join("physical_package_id"))
                    .and_then(|value| value.parse().ok()),
                die: self
                    .text(&topology.join("die_id"))
                    .and_then(|value| value.parse().ok()),
                core: self
                    .text(&topology.join("core_id"))
                    .and_then(|value| value.parse().ok()),
                core_type: CoreType::Uniform,
                online: true,
                siblings: self
                    .text(&topology.join("thread_siblings_list"))
                    .and_then(|value| parse_cpulist(&value))
                    .unwrap_or_else(|| vec![*cpu]),
            });
        }
        let types = classify_core_types(&classes);
        for entry in &mut topology_entries {
            if let Some((_, kind)) = types.iter().find(|(cpu, _)| *cpu == entry.id) {
                entry.core_type = *kind;
            }
        }
        let affinity = std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(online.len());
        let mut package_groups = std::collections::BTreeMap::<u16, Vec<u16>>::new();
        let mut die_groups = std::collections::BTreeMap::<(u16, u16), Vec<u16>>::new();
        for entry in &topology_entries {
            if let Some(package) = entry.package {
                package_groups.entry(package).or_default().push(entry.id);
            }
            if let (Some(package), Some(die)) = (entry.package, entry.die) {
                die_groups.entry((package, die)).or_default().push(entry.id);
            }
        }
        self.part = CpuTopologyPart {
            cpus: topology_entries,
            packages: package_groups.into_values().collect(),
            dies: die_groups.into_values().collect(),
            l3_groups: l3_groups(&online, &l3_input),
            effective_cpus: effective_cpus(online.len(), affinity),
        };
        self.online = online;
    }
}
impl<V: SysfsView> Collector for CpuTopologyCollector<V> {
    fn id(&self) -> CollectorId {
        CollectorId::CpuTopology
    }
    fn cadence(&self) -> Cadence {
        Cadence::Every(30)
    }
    fn discover(&mut self, _env: &Env) -> Health {
        let online_path = self.path("devices/system/cpu/online");
        let online = self
            .text(&online_path)
            .and_then(|value| parse_cpulist(&value));
        match online {
            Some(cpus) if !cpus.is_empty() => {
                self.scan(cpus);
                Health::Ok
            }
            _ => {
                self.part.clear();
                self.online.clear();
                Health::Absent(Absent::NoHardware)
            }
        }
    }
    fn sample(&mut self, _ctx: &TickCtx, frame: &mut MonitorFrame) -> Result<(), CollectError> {
        let online_path = self.path("devices/system/cpu/online");
        if let Some(online) = self
            .text(&online_path)
            .and_then(|value| parse_cpulist(&value))
            && online != self.online
        {
            self.scan(online);
        }
        frame.parts.cpu_topology.clone_from(&self.part);
        frame.sampled.insert(CollectorId::CpuTopology);
        Ok(())
    }
    fn close(&mut self) {
        self.online.clear();
        self.part.clear();
    }
}

#[cfg(test)]
#[path = "cpu_topology_tests.rs"]
mod tests;
