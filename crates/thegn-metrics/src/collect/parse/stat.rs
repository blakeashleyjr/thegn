//! Allocation-free parser for the shared `/proc/stat` reader.

use super::{fields, parse_u64};

pub const MAX_CPUS: usize = 512;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CpuTicks {
    pub user: u64,
    pub nice: u64,
    pub system: u64,
    pub idle: u64,
    pub iowait: u64,
    pub irq: u64,
    pub softirq: u64,
    pub steal: u64,
    pub guest: u64,
    pub guest_nice: u64,
}

impl CpuTicks {
    pub fn total(self) -> u64 {
        self.user
            .saturating_add(self.nice)
            .saturating_add(self.system)
            .saturating_add(self.idle)
            .saturating_add(self.iowait)
            .saturating_add(self.irq)
            .saturating_add(self.softirq)
            .saturating_add(self.steal)
    }

    pub fn corrected(self) -> Self {
        Self {
            user: self.user.saturating_sub(self.guest),
            nice: self.nice.saturating_sub(self.guest_nice),
            ..self
        }
    }

    pub fn checked_delta(self, previous: Self) -> Option<Self> {
        Some(Self {
            user: self.user.checked_sub(previous.user)?,
            nice: self.nice.checked_sub(previous.nice)?,
            system: self.system.checked_sub(previous.system)?,
            idle: self.idle.checked_sub(previous.idle)?,
            iowait: self.iowait.saturating_sub(previous.iowait),
            irq: self.irq.checked_sub(previous.irq)?,
            softirq: self.softirq.checked_sub(previous.softirq)?,
            steal: self.steal.checked_sub(previous.steal)?,
            guest: self.guest.checked_sub(previous.guest)?,
            guest_nice: self.guest_nice.checked_sub(previous.guest_nice)?,
        })
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct StatRaw {
    pub total: Option<CpuTicks>,
    pub cpus: Vec<(u16, CpuTicks)>,
    pub ctxt: Option<u64>,
    pub intr_total: Option<u64>,
    pub softirq_total: Option<u64>,
    pub processes: Option<u64>,
    pub btime: Option<u64>,
    pub procs_running: Option<u32>,
    pub procs_blocked: Option<u32>,
    pub truncated: bool,
}

pub fn parse_stat(buf: &[u8], out: &mut StatRaw) {
    out.total = None;
    out.cpus.clear();
    out.ctxt = None;
    out.intr_total = None;
    out.softirq_total = None;
    out.processes = None;
    out.btime = None;
    out.procs_running = None;
    out.procs_blocked = None;
    out.truncated = false;

    let body = match buf.iter().rposition(|byte| *byte == b'\n') {
        Some(index) => &buf[..=index],
        None => &[],
    };
    for line in body.split(|byte| *byte == b'\n') {
        let mut words = fields(line);
        let Some(name) = words.next() else { continue };
        if name == b"cpu" {
            out.total = parse_ticks(words);
            continue;
        }
        if let Some(suffix) = name.strip_prefix(b"cpu") {
            let Some(id) = parse_u64(suffix) else {
                continue;
            };
            if id >= MAX_CPUS as u64 || id > u64::from(u16::MAX) {
                if id >= MAX_CPUS as u64 {
                    out.truncated = true;
                }
                continue;
            }
            if let Some(ticks) = parse_ticks(words) {
                out.cpus.push((id as u16, ticks));
            }
            continue;
        }
        let Some(value) = words.next().and_then(parse_u64) else {
            continue;
        };
        if name == b"intr" {
            out.intr_total = Some(value);
        } else if name == b"ctxt" {
            out.ctxt = Some(value);
        } else if name == b"softirq" {
            out.softirq_total = Some(value);
        } else if name == b"processes" {
            out.processes = Some(value);
        } else if name == b"btime" {
            out.btime = Some(value);
        } else if name == b"procs_running" {
            out.procs_running = (value <= u64::from(u32::MAX)).then_some(value as u32);
        } else if name == b"procs_blocked" {
            out.procs_blocked = (value <= u64::from(u32::MAX)).then_some(value as u32);
        }
    }
}

fn parse_ticks<'a>(words: impl Iterator<Item = &'a [u8]>) -> Option<CpuTicks> {
    let mut tail = [0_u64; 10];
    let mut count = 0;
    for word in words.take(tail.len()) {
        tail[count] = parse_u64(word)?;
        count += 1;
    }
    if count < 4 {
        return None;
    }
    Some(CpuTicks {
        user: tail[0],
        nice: tail[1],
        system: tail[2],
        idle: tail[3],
        iowait: tail[4],
        irq: tail[5],
        softirq: tail[6],
        steal: tail[7],
        guest: tail[8],
        guest_nice: tail[9],
    })
}

#[cfg(test)]
mod tests {
    use super::{CpuTicks, MAX_CPUS, StatRaw, parse_stat};

    const STAT_24C: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/cpu/stat_24c"
    ));
    const STAT_GAP: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/cpu/stat_hotplug_gap"
    ));

    #[test]
    fn parses_all_cpus_and_system_counters_from_the_fixture() {
        let mut raw = StatRaw::default();
        parse_stat(STAT_24C, &mut raw);
        assert_eq!(raw.cpus.len(), 24);
        assert_eq!(raw.cpus.first().map(|(id, _)| *id), Some(0));
        assert_eq!(raw.cpus.last().map(|(id, _)| *id), Some(23));
        assert!(
            raw.cpus
                .iter()
                .enumerate()
                .all(|(index, (id, _))| usize::from(*id) == index)
        );
        assert_eq!(
            raw.total,
            Some(CpuTicks {
                user: 2676,
                nice: 516,
                system: 1476,
                idle: 19476,
                iowait: 756,
                irq: 324,
                softirq: 348,
                steal: 300,
                guest: 1236,
                guest_nice: 396,
            })
        );
        assert_eq!(
            raw.cpus.first().map(|(_, ticks)| *ticks),
            Some(CpuTicks {
                user: 100,
                nice: 10,
                system: 50,
                idle: 800,
                iowait: 20,
                irq: 2,
                softirq: 3,
                steal: 1,
                guest: 40,
                guest_nice: 5,
            })
        );
        assert_eq!(
            raw.cpus.last().map(|(_, ticks)| *ticks),
            Some(CpuTicks {
                user: 123,
                nice: 33,
                system: 73,
                idle: 823,
                iowait: 43,
                irq: 25,
                softirq: 26,
                steal: 24,
                guest: 63,
                guest_nice: 28,
            })
        );
        assert_eq!(raw.ctxt, Some(123456));
        assert_eq!(raw.intr_total, Some(98765));
        assert_eq!(raw.softirq_total, Some(7654));
        assert_eq!(raw.processes, Some(4321));
        assert_eq!(raw.btime, Some(1700000000));
        assert_eq!(raw.procs_running, Some(7));
        assert_eq!(raw.procs_blocked, Some(2));
    }

    #[test]
    fn cpu_ids_keep_kernel_numbers_across_hotplug_gaps() {
        let mut raw = StatRaw::default();
        parse_stat(STAT_GAP, &mut raw);
        assert_eq!(raw.cpus.len(), 23);
        assert!(raw.cpus.iter().all(|(id, _)| *id != 7));
        assert_eq!(raw.cpus.get(7).map(|(id, _)| *id), Some(8));
    }

    #[test]
    fn accepts_old_four_field_lines_and_corrects_guest_accounting() {
        let mut raw = StatRaw::default();
        parse_stat(b"cpu 100 20 30 400\ncpu0 100 20 30 400\n", &mut raw);
        let ticks = raw.cpus[0].1;
        assert_eq!(ticks.steal, 0);
        assert_eq!(ticks.guest, 0);
        assert_eq!(ticks.guest_nice, 0);
        assert_eq!(ticks.corrected().user, 100);
        assert_eq!(ticks.total(), 550);

        let guest_exceeds_user = CpuTicks {
            user: 10,
            guest: 20,
            ..CpuTicks::default()
        };
        assert_eq!(guest_exceeds_user.corrected().user, 0);
        assert_eq!(
            CpuTicks {
                user: 10,
                guest: 20,
                ..CpuTicks::default()
            }
            .total(),
            10
        );
    }

    #[test]
    fn every_truncated_prefix_and_garbage_line_is_safe_and_skips_partial_cpus() {
        let mut raw = StatRaw::default();
        let full = {
            let mut parsed = StatRaw::default();
            parse_stat(STAT_24C, &mut parsed);
            parsed
        };
        for end in 0..=STAT_24C.len() {
            parse_stat(&STAT_24C[..end], &mut raw);
            assert!(raw.cpus.iter().all(|(id, ticks)| {
                full.cpus
                    .iter()
                    .find(|(full_id, _)| full_id == id)
                    .is_some_and(|(_, full_ticks)| ticks == full_ticks)
            }));
            assert!(raw.total.is_none() || raw.total == full.total);
        }
        parse_stat(
            b"cpu x y\ncpuZZ 1 2 3 4\nctxt\n\xff\ncpu0 1 2 3\n",
            &mut raw,
        );
        assert!(raw.total.is_none());
        assert!(raw.cpus.is_empty());
        assert!(raw.ctxt.is_none());
    }

    #[test]
    fn ignores_fields_after_the_kernel_tick_fields() {
        let mut raw = StatRaw::default();
        parse_stat(
            b"cpu 1 2 3 4 5 6 7 8 9 10 11\ncpu0 1 2 3 4 5 6 7 8 9 10 11\n",
            &mut raw,
        );
        assert_eq!(raw.total.map(|ticks| ticks.guest_nice), Some(10));
        assert_eq!(raw.cpus[0].1.guest_nice, 10);
    }

    #[test]
    fn reports_cpu_ids_outside_the_model_limit() {
        let mut raw = StatRaw::default();
        parse_stat(b"cpu 1 2 3 4\ncpu600 1 2 3 4\n", &mut raw);
        assert!(raw.truncated);
        assert!(raw.cpus.is_empty());
        assert_eq!(MAX_CPUS, 512);
    }

    #[test]
    fn checked_delta_rejects_decreases_except_iowait() {
        let previous = CpuTicks {
            user: 2,
            iowait: 3,
            ..CpuTicks::default()
        };
        let current = CpuTicks {
            user: 1,
            iowait: 2,
            ..CpuTicks::default()
        };
        assert!(current.checked_delta(previous).is_none());
        let current = CpuTicks {
            user: 3,
            iowait: 2,
            ..CpuTicks::default()
        };
        assert_eq!(current.checked_delta(previous).unwrap().iowait, 0);
    }
}
