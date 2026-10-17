//! Allocation-free parser for the shared `/proc/stat` reader.

use super::{fields, parse_u64};

const MAX_CPUS: u64 = 512;

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

    pub fn nondecreasing_from(self, previous: Self) -> bool {
        self.user >= previous.user
            && self.nice >= previous.nice
            && self.system >= previous.system
            && self.idle >= previous.idle
            && self.iowait >= previous.iowait
            && self.irq >= previous.irq
            && self.softirq >= previous.softirq
            && self.steal >= previous.steal
            && self.guest >= previous.guest
            && self.guest_nice >= previous.guest_nice
    }

    pub fn delta_from(self, previous: Self) -> Self {
        Self {
            user: self.user - previous.user,
            nice: self.nice - previous.nice,
            system: self.system - previous.system,
            idle: self.idle - previous.idle,
            iowait: self.iowait - previous.iowait,
            irq: self.irq - previous.irq,
            softirq: self.softirq - previous.softirq,
            steal: self.steal - previous.steal,
            guest: self.guest - previous.guest,
            guest_nice: self.guest_nice - previous.guest_nice,
        }
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

    for line in buf.split(|byte| *byte == b'\n') {
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
            if id >= MAX_CPUS || id > u64::from(u16::MAX) {
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
    for word in words {
        if count == tail.len() {
            return None;
        }
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
    use super::{CpuTicks, StatRaw, parse_stat};

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
                user: 1100,
                nice: 120,
                system: 520,
                idle: 8200,
                iowait: 220,
                irq: 22,
                softirq: 33,
                steal: 11,
                guest: 44,
                guest_nice: 6,
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
        for end in 0..=STAT_24C.len() {
            parse_stat(&STAT_24C[..end], &mut raw);
            assert!(raw.cpus.iter().all(|(_, ticks)| ticks.total() > 0));
        }
        parse_stat(
            b"cpu x y\ncpuZZ 1 2 3 4\nctxt\n\xff\ncpu0 1 2 3\n",
            &mut raw,
        );
        assert!(raw.total.is_none());
        assert!(raw.cpus.is_empty());
        assert!(raw.ctxt.is_none());
    }
}
