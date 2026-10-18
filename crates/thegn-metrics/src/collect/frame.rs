//! Reusable monitor frame and fixed-size frame pool.
use super::{COLLECTORS, CollectorSet, health::Health};
use std::collections::VecDeque;

pub trait Part: Default {
    fn clear(&mut self);
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Parts {
    pub loadavg: super::model::loadavg::LoadavgPart,
    pub uptime: super::model::uptime::UptimePart,
    pub psi: super::model::psi::PsiPart,
    pub cpu_stat: super::model::cpu_stat::CpuStatPart,
    pub cpu_freq: super::model::cpu_freq::CpuFreqPart,
    pub hwmon: super::model::hwmon::HwmonPart,
    pub cgroup: super::model::cgroup::CgroupPart,
    pub cgroup_slice: super::model::cgroup_slice::CgroupSlicePart,
    pub amd_smu: super::model::amd_smu::AmdSmuPart,
    pub meminfo: super::model::meminfo::MeminfoPart,
    pub vmstat: super::model::vmstat::VmstatPart,
    pub netdev: super::model::netdev::NetdevPart,
    pub diskstats: super::model::diskstats::DiskstatsPart,
    pub powercap: super::model::powercap::PowercapPart,
    pub power_supply: super::model::power_supply::PowerSupplyPart,
    pub cpu_irq: super::model::cpu_irq::CpuIrqPart,
    pub cpu_sched: super::model::cpu_sched::CpuSchedPart,
}
impl Parts {
    pub fn clear(&mut self) {
        self.loadavg.clear();
        self.uptime.clear();
        self.psi.clear();
        self.cpu_stat.clear();
        self.cpu_freq.clear();
        self.hwmon.clear();
        self.cgroup.clear();
        self.cgroup_slice.clear();
        self.amd_smu.clear();
        self.meminfo.clear();
        self.vmstat.clear();
        self.netdev.clear();
        self.diskstats.clear();
        self.powercap.clear();
        self.power_supply.clear();
        self.cpu_irq.clear();
        self.cpu_sched.clear();
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct MonitorFrame {
    pub seq: u64,
    pub at_ms: u64,
    pub dt_ms: u32,
    pub demand_gen: u64,
    pub sampled: CollectorSet,
    pub health: [Health; COLLECTORS],
    pub parts: Parts,
}
impl Default for MonitorFrame {
    fn default() -> Self {
        Self {
            seq: 0,
            at_ms: 0,
            dt_ms: 0,
            demand_gen: 0,
            sampled: CollectorSet::empty(),
            health: [Health::default(); COLLECTORS],
            parts: Parts::default(),
        }
    }
}
impl MonitorFrame {
    pub fn reset(&mut self) {
        self.seq = 0;
        self.at_ms = 0;
        self.dt_ms = 0;
        self.demand_gen = 0;
        self.sampled = CollectorSet::empty();
        self.parts.clear();
    }
}

pub const FRAME_POOL: usize = 3;
#[derive(Debug)]
pub struct FramePool {
    free: VecDeque<Box<MonitorFrame>>,
}
impl FramePool {
    pub fn new() -> Self {
        Self {
            free: (0..FRAME_POOL)
                .map(|_| Box::new(MonitorFrame::default()))
                .collect(),
        }
    }
    pub fn take(&mut self) -> Option<Box<MonitorFrame>> {
        self.free.pop_front()
    }
    pub fn recycle(&mut self, mut frame: Box<MonitorFrame>) {
        frame.reset();
        if self.free.len() < FRAME_POOL {
            self.free.push_back(frame);
        }
    }
    pub fn len(&self) -> usize {
        self.free.len()
    }
    pub fn is_empty(&self) -> bool {
        self.free.is_empty()
    }
}
impl Default for FramePool {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::{FRAME_POOL, FramePool, Part};
    #[test]
    fn pool_reuses_three_reset_allocations_and_drops_excess() {
        let mut pool = FramePool::new();
        let first = pool.take().unwrap();
        let second = pool.take().unwrap();
        let third = pool.take().unwrap();
        assert!(pool.take().is_none());
        let address = &*first as *const _;
        let mut first = first;
        first.seq = 42;
        first.at_ms = 9;
        first.dt_ms = 4;
        first.demand_gen = 8;
        first.health[0] = super::super::health::Health::Ok;
        first.parts.loadavg.clear();
        pool.recycle(first);
        pool.recycle(second);
        pool.recycle(third);
        assert_eq!(pool.len(), FRAME_POOL);
        let reused = pool.take().unwrap();
        assert!(std::ptr::eq(&*reused, address));
        assert_eq!(reused.seq, 0);
        assert_eq!(reused.at_ms, 0);
        assert_eq!(reused.dt_ms, 0);
        assert_eq!(reused.demand_gen, 0);
        assert_eq!(reused.health[0], super::super::health::Health::Ok);
        pool.recycle(reused);
        pool.recycle(Box::default());
        assert_eq!(pool.len(), FRAME_POOL);
    }
}
