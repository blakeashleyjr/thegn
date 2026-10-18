//! Static CPU package, cache, and core classification facts.

use super::super::policy::cpu_topology::CoreType;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CpuTopologyEntry {
    pub id: u16,
    pub package: Option<u16>,
    pub die: Option<u16>,
    pub core: Option<u16>,
    pub core_type: CoreType,
    pub online: bool,
    pub siblings: Vec<u16>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CpuTopologyPart {
    pub cpus: Vec<CpuTopologyEntry>,
    pub packages: Vec<Vec<u16>>,
    pub dies: Vec<Vec<u16>>,
    pub l3_groups: Vec<Vec<u16>>,
    pub effective_cpus: usize,
}

impl crate::collect::frame::Part for CpuTopologyPart {
    fn clear(&mut self) {
        self.cpus.clear();
        self.packages.clear();
        self.dies.clear();
        self.l3_groups.clear();
        self.effective_cpus = 0;
    }
}
