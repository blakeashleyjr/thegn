//! Collector registry, scheduling, health mirroring, and bounded retries.
use super::{
    COLLECTORS, CollectError, Collector, CollectorId, CollectorSet, TickCtx,
    env::Env,
    frame::{MonitorFrame, Parts},
    health::Health,
    schedule::{self, Cadence, Mode},
    stub::NotImplemented,
};

pub struct Registry {
    slots: Vec<Box<dyn Collector>>,
    cadences: Vec<(CollectorId, Cadence)>,
    phases: [u16; COLLECTORS],
    health: [Health; COLLECTORS],
    env: Env,
    backup: Parts,
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Rollup {
    pub ok: u8,
    pub warming: u8,
    pub absent: u8,
    pub failing: u8,
}
impl Registry {
    pub fn new(collectors: Vec<Box<dyn Collector>>) -> Self {
        let mut slots: Vec<Option<Box<dyn Collector>>> = (0..COLLECTORS).map(|_| None).collect();
        for collector in collectors {
            let index = collector.id().index();
            if slots[index].is_none() {
                slots[index] = Some(collector);
            }
        }
        let slots: Vec<Box<dyn Collector>> = CollectorId::ALL
            .into_iter()
            .enumerate()
            .map(|(index, id)| {
                slots[index]
                    .take()
                    .unwrap_or_else(|| Box::new(NotImplemented(id)))
            })
            .collect();
        let cadences: Vec<_> = slots
            .iter()
            .map(|collector| (collector.id(), collector.cadence()))
            .collect();
        let phases = schedule::phases(&cadences);
        Self {
            slots,
            cadences,
            phases,
            health: [Health::Warming; COLLECTORS],
            env: Env::default(),
            backup: Parts::default(),
        }
    }
    pub fn discover(&mut self, env: &Env) {
        self.env = env.clone();
        for collector in &mut self.slots {
            self.health[collector.id().index()] = collector.discover(env);
        }
    }
    pub fn due(&self, mode: Mode, demand: CollectorSet, tick: u64) -> CollectorSet {
        let scheduled = schedule::due_set(mode, demand, &self.cadences, &self.phases, tick);
        let mut result = CollectorSet::empty();
        for id in scheduled.iter() {
            match self.health[id.index()] {
                Health::Absent(_) => {}
                Health::Failing { .. } if !self.health[id.index()].retry_due(tick) => {}
                _ => result.insert(id),
            }
        }
        result
    }
    pub fn tick(
        &mut self,
        mode: Mode,
        demand: CollectorSet,
        ctx: &TickCtx,
        frame: &mut MonitorFrame,
    ) -> CollectorSet {
        let due = self.due(mode, demand, ctx.tick);
        for id in due.iter() {
            copy_part(&mut self.backup, &frame.parts, id);
            let result = self.slots[id.index()].sample(ctx, frame);
            match result {
                Ok(()) => self.health[id.index()] = self.health[id.index()].after_ok(),
                Err(CollectError::Gone) => {
                    copy_part(&mut frame.parts, &self.backup, id);
                    self.slots[id.index()].close();
                    self.health[id.index()] = failure_health(self.health[id.index()], ctx.tick);
                    let discovered = self.slots[id.index()].discover(&self.env);
                    if let Health::Absent(absent) = discovered {
                        self.health[id.index()] = Health::Absent(absent);
                    }
                }
                Err(_) => {
                    copy_part(&mut frame.parts, &self.backup, id);
                    self.health[id.index()] = failure_health(self.health[id.index()], ctx.tick);
                }
            }
        }
        frame.health = self.health;
        due
    }
    pub fn close_all(&mut self) {
        for collector in &mut self.slots {
            collector.close();
        }
    }
    pub fn health(&self, id: CollectorId) -> Health {
        self.health[id.index()]
    }
    pub fn healths(&self) -> [Health; COLLECTORS] {
        self.health
    }
    pub fn live(&self, demand: CollectorSet) -> CollectorSet {
        demand
            .iter()
            .filter(|id| !matches!(self.health[id.index()], Health::Absent(_)))
            .fold(CollectorSet::empty(), |mut live, id| {
                live.insert(id);
                live
            })
    }
    pub fn rollup(&self) -> Rollup {
        let mut r = Rollup::default();
        for health in self.health {
            match health {
                Health::Ok => r.ok = r.ok.saturating_add(1),
                Health::Warming => r.warming = r.warming.saturating_add(1),
                Health::Absent(_) => r.absent = r.absent.saturating_add(1),
                Health::Failing { .. } => r.failing = r.failing.saturating_add(1),
            }
        }
        r
    }
}

fn failure_health(previous: Health, tick: u64) -> Health {
    let since_tick = match previous {
        Health::Failing { since_tick, .. } => since_tick,
        _ => tick,
    };
    match previous.after_error(tick.saturating_add(1)) {
        Health::Failing { backoff, .. } => Health::Failing {
            since_tick,
            backoff,
            retry_at: tick.saturating_add(1).saturating_add(u64::from(backoff)),
        },
        health => health,
    }
}

fn copy_part(target: &mut Parts, source: &Parts, id: CollectorId) {
    match id {
        CollectorId::Loadavg => target.loadavg.clone_from(&source.loadavg),
        CollectorId::Uptime => target.uptime.clone_from(&source.uptime),
        CollectorId::Psi => target.psi.clone_from(&source.psi),
        CollectorId::CpuStat => target.cpu_stat.clone_from(&source.cpu_stat),
        CollectorId::CpuFreq => target.cpu_freq.clone_from(&source.cpu_freq),
        CollectorId::Hwmon => target.hwmon.clone_from(&source.hwmon),
        CollectorId::Cgroup => target.cgroup.clone_from(&source.cgroup),
        CollectorId::CgroupSlice => target.cgroup_slice.clone_from(&source.cgroup_slice),
        CollectorId::AmdSmu => target.amd_smu.clone_from(&source.amd_smu),
        CollectorId::Meminfo => target.meminfo.clone_from(&source.meminfo),
        CollectorId::Vmstat => target.vmstat.clone_from(&source.vmstat),
        CollectorId::Netdev => target.netdev.clone_from(&source.netdev),
        CollectorId::NetWifi => target.net_wifi.clone_from(&source.net_wifi),
        CollectorId::Diskstats => target.diskstats.clone_from(&source.diskstats),
        CollectorId::Powercap => target.powercap.clone_from(&source.powercap),
        CollectorId::PowerSupply => target.power_supply.clone_from(&source.power_supply),
        CollectorId::CpuIrq => target.cpu_irq.clone_from(&source.cpu_irq),
        CollectorId::CpuSched => target.cpu_sched.clone_from(&source.cpu_sched),
        CollectorId::ThermalZone => target.thermal_zone.clone_from(&source.thermal_zone),
        CollectorId::CpuIdle => target.cpu_idle.clone_from(&source.cpu_idle),
        CollectorId::CpuTopology => target.cpu_topology.clone_from(&source.cpu_topology),
        CollectorId::Filesystems => target.filesystems.clone_from(&source.filesystems),
        CollectorId::GpuDevices => target.gpu_devices.clone_from(&source.gpu_devices),
        CollectorId::GpuEngines => target.gpu_engines.clone_from(&source.gpu_engines),
    }
}

#[cfg(test)]
mod tests {
    use super::super::health::Absent;
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    struct Fake {
        id: CollectorId,
        cadence: Cadence,
        discovered: Health,
        result: Result<(), CollectError>,
        count: Arc<AtomicUsize>,
        closes: Arc<AtomicUsize>,
        discoveries: Arc<AtomicUsize>,
    }
    impl Collector for Fake {
        fn id(&self) -> CollectorId {
            self.id
        }
        fn cadence(&self) -> Cadence {
            self.cadence
        }
        fn discover(&mut self, _env: &Env) -> Health {
            self.discoveries.fetch_add(1, Ordering::SeqCst);
            self.discovered
        }
        fn sample(&mut self, _ctx: &TickCtx, frame: &mut MonitorFrame) -> Result<(), CollectError> {
            self.count.fetch_add(1, Ordering::SeqCst);
            if self.result.is_ok() {
                frame.sampled.insert(self.id);
            }
            self.result.clone()
        }
        fn close(&mut self) {
            self.closes.fetch_add(1, Ordering::SeqCst);
        }
    }
    fn fake(
        id: CollectorId,
        cadence: Cadence,
        health: Health,
        result: Result<(), CollectError>,
    ) -> (
        Box<dyn Collector>,
        Arc<AtomicUsize>,
        Arc<AtomicUsize>,
        Arc<AtomicUsize>,
    ) {
        let count = Arc::new(AtomicUsize::new(0));
        let closes = Arc::new(AtomicUsize::new(0));
        let discoveries = Arc::new(AtomicUsize::new(0));
        (
            Box::new(Fake {
                id,
                cadence,
                discovered: health,
                result,
                count: count.clone(),
                closes: closes.clone(),
                discoveries: discoveries.clone(),
            }),
            count,
            closes,
            discoveries,
        )
    }
    fn demand(id: CollectorId) -> CollectorSet {
        let mut set = CollectorSet::empty();
        set.insert(id);
        set
    }

    #[test]
    fn due_respects_mode_demand_phase_absent_and_retry_backoff() {
        let (ok, count, _, _) = fake(CollectorId::Loadavg, Cadence::Every(2), Health::Ok, Ok(()));
        let (absent, absent_count, _, _) = fake(
            CollectorId::Uptime,
            Cadence::Every(1),
            Health::Absent(Absent::NoHardware),
            Ok(()),
        );
        let (failing, fail_count, _, _) = fake(
            CollectorId::Psi,
            Cadence::Every(2),
            Health::Ok,
            Err(CollectError::Parse("test")),
        );
        let mut registry = Registry::new(vec![ok, absent, failing]);
        registry.discover(&Env::default());
        let all = [CollectorId::Loadavg, CollectorId::Uptime, CollectorId::Psi]
            .into_iter()
            .fold(CollectorSet::empty(), |mut set, id| {
                set.insert(id);
                set
            });
        assert!(registry.due(Mode::Paused, all, 0).is_empty());
        assert!(
            !registry
                .due(Mode::Open, demand(CollectorId::Loadavg), 1)
                .contains(CollectorId::Loadavg)
        );
        for tick in 0..2 {
            let mut frame = MonitorFrame::default();
            registry.tick(
                Mode::Open,
                all,
                &TickCtx {
                    tick,
                    now_ms: tick * 100,
                },
                &mut frame,
            );
        }
        assert_eq!(count.load(Ordering::SeqCst), 1);
        assert_eq!(absent_count.load(Ordering::SeqCst), 0);
        assert_eq!(fail_count.load(Ordering::SeqCst), 1);
        assert!(
            !registry
                .due(Mode::Open, demand(CollectorId::Psi), 1)
                .contains(CollectorId::Psi)
        );
        assert!(
            registry
                .due(Mode::Open, demand(CollectorId::Psi), 3)
                .contains(CollectorId::Psi)
        );
        let mut retry_frame = MonitorFrame::default();
        registry.tick(
            Mode::Open,
            demand(CollectorId::Psi),
            &TickCtx {
                tick: 3,
                now_ms: 300,
            },
            &mut retry_frame,
        );
        assert_eq!(fail_count.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn duplicate_keeps_first_gone_closes_and_health_rolls_up() {
        let (first, first_count, _, _) =
            fake(CollectorId::Loadavg, Cadence::Every(1), Health::Ok, Ok(()));
        let (second, second_count, _, _) =
            fake(CollectorId::Loadavg, Cadence::Every(1), Health::Ok, Ok(()));
        let (gone, gone_count, closes, discoveries) = fake(
            CollectorId::Uptime,
            Cadence::Every(1),
            Health::Ok,
            Err(CollectError::Gone),
        );
        let mut registry = Registry::new(vec![first, second, gone]);
        registry.discover(&Env::default());
        let mut frame = MonitorFrame::default();
        let demand = [CollectorId::Loadavg, CollectorId::Uptime]
            .into_iter()
            .fold(CollectorSet::empty(), |mut set, id| {
                set.insert(id);
                set
            });
        registry.tick(
            Mode::Open,
            demand,
            &TickCtx { tick: 0, now_ms: 1 },
            &mut frame,
        );
        assert_eq!(first_count.load(Ordering::SeqCst), 1);
        assert_eq!(second_count.load(Ordering::SeqCst), 0);
        assert_eq!(gone_count.load(Ordering::SeqCst), 1);
        assert_eq!(closes.load(Ordering::SeqCst), 1);
        assert_eq!(discoveries.load(Ordering::SeqCst), 2);
        assert!(frame.sampled.contains(CollectorId::Loadavg));
        assert_eq!(frame.health, registry.health);
        assert_eq!(
            registry.health(CollectorId::Uptime),
            Health::Failing {
                since_tick: 0,
                backoff: 1,
                retry_at: 2
            }
        );
        assert_eq!(registry.rollup().failing, 1);
        // Every collector except the Ok Loadavg and the failing Uptime.
        assert_eq!(
            registry.rollup().absent as usize,
            CollectorId::ALL.len() - 2
        );
    }

    struct WritesThenFails(u8);
    impl Collector for WritesThenFails {
        fn id(&self) -> CollectorId {
            CollectorId::Loadavg
        }
        fn cadence(&self) -> Cadence {
            Cadence::Every(1)
        }
        fn discover(&mut self, _env: &Env) -> Health {
            Health::Ok
        }
        fn sample(&mut self, _ctx: &TickCtx, frame: &mut MonitorFrame) -> Result<(), CollectError> {
            self.0 += 1;
            frame.parts.loadavg.value = Some(crate::collect::parse::loadavg::Loadavg {
                load1: if self.0 == 1 { 100 } else { 900 },
                ..Default::default()
            });
            if self.0 == 1 {
                Ok(())
            } else {
                Err(CollectError::Parse("injected"))
            }
        }
        fn close(&mut self) {}
    }

    #[test]
    fn failed_sample_restores_its_part_and_reports_attempt_and_anchored_retry() {
        let mut registry = Registry::new(vec![Box::new(WritesThenFails(0))]);
        registry.discover(&Env::default());
        let demand = demand(CollectorId::Loadavg);
        let mut frame = MonitorFrame::default();
        assert!(
            registry
                .tick(
                    Mode::Open,
                    demand,
                    &TickCtx { tick: 0, now_ms: 0 },
                    &mut frame
                )
                .contains(CollectorId::Loadavg)
        );
        assert_eq!(frame.parts.loadavg.value.unwrap().load1, 100);
        let attempted = registry.tick(
            Mode::Open,
            demand,
            &TickCtx { tick: 1, now_ms: 1 },
            &mut frame,
        );
        assert!(attempted.contains(CollectorId::Loadavg));
        assert_eq!(frame.parts.loadavg.value.unwrap().load1, 100);
        assert_eq!(
            registry.health(CollectorId::Loadavg),
            Health::Failing {
                since_tick: 1,
                backoff: 1,
                retry_at: 3,
            }
        );
        assert!(
            !registry
                .due(Mode::Open, demand, 2)
                .contains(CollectorId::Loadavg)
        );
    }

    #[test]
    fn live_excludes_absent_demand() {
        let (available, _, _, _) =
            fake(CollectorId::Loadavg, Cadence::Every(1), Health::Ok, Ok(()));
        let (absent, _, _, _) = fake(
            CollectorId::Uptime,
            Cadence::Every(1),
            Health::Absent(Absent::NoHardware),
            Ok(()),
        );
        let mut registry = Registry::new(vec![available, absent]);
        registry.discover(&Env::default());
        let requested = [CollectorId::Loadavg, CollectorId::Uptime]
            .into_iter()
            .fold(CollectorSet::empty(), |mut set, id| {
                set.insert(id);
                set
            });
        let live = registry.live(requested);
        assert_eq!(live.len(), 1);
        assert!(live.contains(CollectorId::Loadavg));
        assert!(!live.contains(CollectorId::Uptime));
        assert_eq!(
            registry.healths()[CollectorId::Uptime.index()],
            Health::Absent(Absent::NoHardware)
        );
    }
}
