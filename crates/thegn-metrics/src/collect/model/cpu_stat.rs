//! Interval CPU mode shares and rates derived from `/proc/stat` counters.

use crate::collect::{
    delta::Counter,
    frame::Part,
    parse::stat::{CpuTicks, MAX_CPUS, StatRaw},
};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ModeShare {
    pub user: u16,
    pub nice: u16,
    pub system: u16,
    pub iowait: u16,
    pub irq: u16,
    pub softirq: u16,
    pub steal: u16,
    pub guest: u16,
    pub idle: u16,
    pub busy: u16,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CoreSample {
    pub id: u16,
    pub online: bool,
    pub share: Option<ModeShare>,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct CpuStatPart {
    pub total: Option<ModeShare>,
    pub cores: Vec<CoreSample>,
    pub ctxt_per_s: Option<f64>,
    pub intr_per_s: Option<f64>,
    pub softirq_per_s: Option<f64>,
    pub forks_per_s: Option<f64>,
    pub procs_running: Option<u32>,
    pub procs_blocked: Option<u32>,
    pub btime: Option<u64>,
    pub steal_seen: bool,
    pub truncated: bool,
}

impl Part for CpuStatPart {
    fn clear(&mut self) {
        self.total = None;
        self.cores.clear();
        self.ctxt_per_s = None;
        self.intr_per_s = None;
        self.softirq_per_s = None;
        self.forks_per_s = None;
        self.procs_running = None;
        self.procs_blocked = None;
        self.btime = None;
        self.steal_seen = false;
        self.truncated = false;
    }
}

#[derive(Debug, Default, Clone)]
pub struct CpuStatDelta {
    previous_total: Option<CpuTicks>,
    previous_cpus: Vec<Option<CpuTicks>>,
    seen_cpus: Vec<bool>,
    ctxt: Counter,
    intr: Counter,
    softirq: Counter,
    processes: Counter,
    steal_seen: bool,
}

impl CpuStatDelta {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn update(&mut self, raw: &StatRaw, now_ms: u64, out: &mut CpuStatPart) {
        out.total = raw.total.and_then(|ticks| {
            let previous = self.previous_total.replace(ticks)?;
            let delta = tick_delta(ticks, previous)?;
            if delta.steal > 0 {
                self.steal_seen = true;
            }
            let share = mode_share(delta);
            (delta.total() > 0).then_some(share)
        });
        if raw.total.is_none() {
            self.previous_total = None;
        }

        out.cores.clear();
        let max_id = raw
            .cpus
            .iter()
            .map(|(id, _)| usize::from(*id))
            .filter(|id| *id < MAX_CPUS)
            .max();
        let highest = max_id
            .map_or(self.previous_cpus.len(), |id| id.saturating_add(1))
            .max(self.previous_cpus.len())
            .min(MAX_CPUS);
        if self.previous_cpus.len() < highest {
            self.previous_cpus.resize(highest, None);
            self.seen_cpus.resize(highest, false);
        }
        out.truncated = raw.truncated;

        let mut cpu_index = 0;
        for id in 0..highest {
            while cpu_index < raw.cpus.len() && usize::from(raw.cpus[cpu_index].0) < id {
                cpu_index += 1;
            }
            let current = raw
                .cpus
                .get(cpu_index)
                .filter(|(cpu_id, _)| usize::from(*cpu_id) == id)
                .map(|(_, ticks)| *ticks);
            if let Some(ticks) = current {
                self.seen_cpus[id] = true;
                let previous = self.previous_cpus[id].replace(ticks);
                let share = previous
                    .and_then(|old| tick_delta(ticks, old))
                    .and_then(|delta| {
                        let share = mode_share(delta);
                        if delta.steal > 0 {
                            self.steal_seen = true;
                        }
                        (delta.total() > 0).then_some(share)
                    });
                out.cores.push(CoreSample {
                    id: id as u16,
                    online: true,
                    share,
                });
            } else if self.seen_cpus[id] {
                self.previous_cpus[id] = None;
                out.cores.push(CoreSample {
                    id: id as u16,
                    online: false,
                    share: None,
                });
            }
        }

        out.ctxt_per_s = raw.ctxt.and_then(|value| self.ctxt.rate(value, now_ms));
        out.intr_per_s = raw
            .intr_total
            .and_then(|value| self.intr.rate(value, now_ms));
        out.softirq_per_s = raw
            .softirq_total
            .and_then(|value| self.softirq.rate(value, now_ms));
        out.forks_per_s = raw
            .processes
            .and_then(|value| self.processes.rate(value, now_ms));
        out.procs_running = raw.procs_running;
        out.procs_blocked = raw.procs_blocked;
        out.btime = raw.btime;
        out.steal_seen = self.steal_seen;
    }
}

fn tick_delta(current: CpuTicks, previous: CpuTicks) -> Option<CpuTicks> {
    current.checked_delta(previous)
}

fn mode_share(ticks: CpuTicks) -> ModeShare {
    let corrected = ticks.corrected();
    let total = ticks.total();
    let busy = total
        .saturating_sub(ticks.idle)
        .saturating_sub(ticks.iowait);
    ModeShare {
        user: permille(corrected.user, total),
        nice: permille(corrected.nice, total),
        system: permille(ticks.system, total),
        iowait: permille(ticks.iowait, total),
        irq: permille(ticks.irq, total),
        softirq: permille(ticks.softirq, total),
        steal: permille(ticks.steal, total),
        guest: permille(ticks.guest.saturating_add(ticks.guest_nice), total),
        idle: permille(ticks.idle, total),
        busy: permille(busy, total),
    }
}

fn permille(value: u64, total: u64) -> u16 {
    if total == 0 {
        0
    } else {
        (((u128::from(value) * 1000 + u128::from(total) / 2) / u128::from(total)).min(1000)) as u16
    }
}

#[cfg(test)]
mod tests {
    use super::{CpuStatDelta, CpuStatPart};
    use crate::collect::{
        frame::Part,
        parse::stat::{CpuTicks, StatRaw, parse_stat},
    };

    const STAT_24C: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/cpu/stat_24c"
    ));
    const STAT_GAP: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/cpu/stat_hotplug_gap"
    ));
    const STAT_BACKWARDS: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/cpu/stat_backwards"
    ));
    const STAT_STEAL: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/cpu/stat_vm_steal"
    ));

    fn parse(bytes: &[u8]) -> StatRaw {
        let mut raw = StatRaw::default();
        parse_stat(bytes, &mut raw);
        raw
    }

    #[test]
    fn first_sample_is_unprimed_and_guest_ticks_are_split_from_user() {
        let first = StatRaw {
            total: Some(CpuTicks::default()),
            cpus: vec![(0, CpuTicks::default())],
            ..StatRaw::default()
        };
        let second = StatRaw {
            total: Some(CpuTicks {
                user: 100,
                guest: 40,
                ..CpuTicks::default()
            }),
            cpus: vec![(
                0,
                CpuTicks {
                    user: 100,
                    guest: 40,
                    ..CpuTicks::default()
                },
            )],
            ..StatRaw::default()
        };
        let mut delta = CpuStatDelta::new();
        let mut part = CpuStatPart::default();
        delta.update(&first, 1000, &mut part);
        assert!(part.total.is_none());
        assert_eq!(part.cores[0].share, None);
        delta.update(&second, 2000, &mut part);
        let total = part.total.unwrap();
        let share = part.cores[0].share.unwrap();
        assert_eq!(total.user, 600);
        assert_eq!(total.guest, 400);
        assert_eq!(total.busy, 1000);
        assert_eq!(share.user, 600);
        assert_eq!(share.guest, 400);
        assert_eq!(share.busy, 1000);
    }

    #[test]
    fn backward_cpu_reprimes_only_that_cpu_then_recovers() {
        let first = parse(STAT_24C);
        let backwards = parse(STAT_BACKWARDS);
        let mut delta = CpuStatDelta::new();
        let mut part = CpuStatPart::default();
        delta.update(&first, 1000, &mut part);
        delta.update(&backwards, 2000, &mut part);
        assert_eq!(part.cores[0].share, None);
        assert!(part.cores[1].share.is_some());
        let mut next = backwards.clone();
        for (_, ticks) in &mut next.cpus {
            ticks.user += 1;
            ticks.idle += 1;
        }
        if let Some(total) = &mut next.total {
            total.user += 1;
            total.idle += 1;
        }
        delta.update(&next, 3000, &mut part);
        assert!(part.cores[0].share.is_some());
    }

    #[test]
    fn hotplug_marks_offline_and_reappearance_primes_again() {
        let first = parse(STAT_24C);
        let gap = parse(STAT_GAP);
        let mut delta = CpuStatDelta::new();
        let mut part = CpuStatPart::default();
        delta.update(&first, 1000, &mut part);
        delta.update(&gap, 2000, &mut part);
        assert!(
            part.cores
                .iter()
                .any(|core| core.id == 7 && !core.online && core.share.is_none())
        );
        delta.update(&first, 3000, &mut part);
        assert!(
            part.cores
                .iter()
                .any(|core| core.id == 7 && core.online && core.share.is_none())
        );
    }

    #[test]
    fn vm_steal_is_reported_and_latched_and_counter_rates_use_deltas() {
        let mut first = parse(STAT_24C);
        first.ctxt = Some(10_000);
        first.intr_total = Some(20_000);
        first.softirq_total = Some(30_000);
        first.processes = Some(4_000);
        let mut second = parse(STAT_STEAL);
        second.ctxt = Some(11_000);
        second.intr_total = Some(20_200);
        second.softirq_total = Some(30_300);
        second.processes = Some(4_100);
        let mut delta = CpuStatDelta::new();
        let mut part = CpuStatPart::default();
        delta.update(&first, 1000, &mut part);
        delta.update(&second, 2000, &mut part);
        assert_eq!(part.cores[0].share.map(|share| share.steal), Some(100));
        assert_eq!(part.total.map(|share| share.steal), Some(100));
        assert!(part.steal_seen);
        assert_eq!(part.ctxt_per_s, Some(1000.0));
        assert_eq!(part.intr_per_s, Some(200.0));
        assert_eq!(part.softirq_per_s, Some(300.0));
        assert_eq!(part.forks_per_s, Some(100.0));
        let mut zero_steal = second.clone();
        for (_, ticks) in &mut zero_steal.cpus {
            ticks.user += 1;
            ticks.idle += 1;
        }
        delta.update(&zero_steal, 3000, &mut part);
        assert!(part.steal_seen);
    }

    #[test]
    fn clear_keeps_core_capacity_and_resets_values() {
        let mut part = CpuStatPart {
            cores: vec![Default::default(); 24],
            ..CpuStatPart::default()
        };
        let capacity = part.cores.capacity();
        part.total = Some(Default::default());
        part.steal_seen = true;
        part.clear();
        assert!(part.cores.is_empty());
        assert!(part.cores.capacity() >= capacity);
        assert!(part.total.is_none());
        assert!(!part.steal_seen);
    }

    #[test]
    fn iowait_decrease_is_clamped_but_other_counter_decreases_reprimes() {
        let first = StatRaw {
            total: Some(CpuTicks {
                user: 100,
                idle: 100,
                iowait: 20,
                ..CpuTicks::default()
            }),
            cpus: vec![(
                0,
                CpuTicks {
                    user: 100,
                    idle: 100,
                    iowait: 20,
                    ..CpuTicks::default()
                },
            )],
            ..StatRaw::default()
        };
        let second = StatRaw {
            total: Some(CpuTicks {
                user: 110,
                idle: 110,
                iowait: 10,
                ..CpuTicks::default()
            }),
            cpus: vec![(
                0,
                CpuTicks {
                    user: 110,
                    idle: 110,
                    iowait: 10,
                    ..CpuTicks::default()
                },
            )],
            ..StatRaw::default()
        };
        let mut delta = CpuStatDelta::new();
        let mut part = CpuStatPart::default();
        delta.update(&first, 1000, &mut part);
        delta.update(&second, 2000, &mut part);
        assert!(part.total.is_some());
        assert_eq!(part.cores[0].share.map(|share| share.iowait), Some(0));

        let mut backwards = second;
        backwards.cpus[0].1.user -= 1;
        backwards.total.as_mut().unwrap().user -= 1;
        delta.update(&backwards, 3000, &mut part);
        assert!(part.total.is_none());
        assert_eq!(part.cores[0].share, None);
    }

    #[test]
    fn truncated_parser_input_is_exposed_in_the_part() {
        let raw = parse(b"cpu 1 2 3 4\ncpu600 1 2 3 4\n");
        let mut delta = CpuStatDelta::new();
        let mut part = CpuStatPart::default();
        delta.update(&raw, 1000, &mut part);
        assert!(part.truncated);
    }
}
