//! Allocation-free cgroup v2 parsers. Malformed and unknown fields are ignored.

use super::{fields, parse_u64};
use crate::collect::model::cgroup::{CpuStat, IoTotals, Limit, MemEvents, MemStat, Pressure};

fn value_for(input: &[u8], key: &[u8]) -> Option<u64> {
    input.split(|byte| *byte == b'\n').find_map(|line| {
        let mut words = fields(line);
        (words.next()? == key).then(|| parse_u64(words.next()?))?
    })
}

pub fn cpu_stat(input: &[u8]) -> Option<CpuStat> {
    Some(CpuStat {
        usage_usec: value_for(input, b"usage_usec")?,
        user_usec: value_for(input, b"user_usec")?,
        system_usec: value_for(input, b"system_usec")?,
        nr_periods: value_for(input, b"nr_periods"),
        nr_throttled: value_for(input, b"nr_throttled"),
        throttled_usec: value_for(input, b"throttled_usec"),
    })
}

pub fn mem_stat(input: &[u8]) -> MemStat {
    MemStat {
        anon: value_for(input, b"anon"),
        file: value_for(input, b"file"),
        kernel: value_for(input, b"kernel"),
        sock: value_for(input, b"sock"),
        shmem: value_for(input, b"shmem"),
        slab: value_for(input, b"slab"),
        pagetables: value_for(input, b"pagetables"),
        file_dirty: value_for(input, b"file_dirty"),
    }
}

pub fn mem_events(input: &[u8]) -> MemEvents {
    MemEvents {
        low: value_for(input, b"low"),
        high: value_for(input, b"high"),
        max: value_for(input, b"max"),
        oom: value_for(input, b"oom"),
        oom_kill: value_for(input, b"oom_kill"),
    }
}

pub fn single_u64(input: &[u8]) -> Option<u64> {
    parse_u64(fields(input).next()?)
}

pub fn pids_max(input: &[u8]) -> Option<Limit> {
    let value = fields(input).next()?;
    if value == b"max" {
        Some(Limit::Max)
    } else {
        parse_u64(value).map(Limit::Value)
    }
}

/// Parse memory.{max,high}; malformed or empty values remain absent.
pub fn memory_limit(input: &[u8]) -> Option<Limit> {
    let value = fields(input).next()?;
    if value == b"max" {
        Some(Limit::Max)
    } else {
        parse_u64(value).map(Limit::Value)
    }
}

pub fn maj_min(input: &[u8]) -> Option<(u32, u32)> {
    let colon = input.iter().position(|byte| *byte == b':')?;
    let major = u32::try_from(parse_u64(trim_ascii(&input[..colon])?)?).ok()?;
    let minor = u32::try_from(parse_u64(trim_ascii(&input[colon + 1..])?)?).ok()?;
    Some((major, minor))
}

fn trim_ascii(input: &[u8]) -> Option<&[u8]> {
    let start = input.iter().position(|byte| !byte.is_ascii_whitespace())?;
    let end = input.iter().rposition(|byte| !byte.is_ascii_whitespace())?;
    Some(&input[start..=end])
}

pub fn io_stat_sum(input: &[u8], counted: &[(u32, u32)]) -> IoTotals {
    let mut totals = IoTotals::default();
    for (index, device) in counted.iter().enumerate() {
        if counted[..index].contains(device) {
            continue;
        }
        let Some(line) = input.split(|byte| *byte == b'\n').find(|line| {
            fields(line)
                .next()
                .and_then(maj_min)
                .is_some_and(|candidate| candidate == *device)
        }) else {
            continue;
        };
        for field in fields(line).skip(1) {
            let Some(separator) = field.iter().position(|byte| *byte == b'=') else {
                continue;
            };
            let (key, value) = (&field[..separator], &field[separator + 1..]);
            let Some(value) = parse_u64(value) else {
                continue;
            };
            match key {
                b"rbytes" => totals.rbytes = totals.rbytes.saturating_add(value),
                b"wbytes" => totals.wbytes = totals.wbytes.saturating_add(value),
                b"rios" => totals.rios = totals.rios.saturating_add(value),
                b"wios" => totals.wios = totals.wios.saturating_add(value),
                _ => {}
            }
        }
    }
    totals
}

pub fn pressure(input: &[u8]) -> Option<Pressure> {
    let parsed = super::psi::parse(input);
    if parsed.some.is_none() && parsed.full.is_none() {
        return None;
    }
    Some(Pressure {
        some_avg10: parsed.some.map(|line| line.avg10 as f32 / 100.0),
        some_total_us: parsed.some.map(|line| line.total_us),
        full_avg10: parsed.full.map(|line| line.avg10 as f32 / 100.0),
        full_total_us: parsed.full.map(|line| line.total_us),
    })
}

#[cfg(test)]
mod tests {
    use super::{
        cpu_stat, io_stat_sum, maj_min, mem_events, mem_stat, memory_limit, pids_max, single_u64,
    };
    use crate::collect::model::cgroup::{IoTotals, Limit, Pressure};

    const CPU: &[u8] = b"usage_usec 100\nuser_usec 70\nsystem_usec 30\nnr_periods 9\nnr_throttled 2\nthrottled_usec 8\n";
    const MEM: &[u8] =
        b"anon 1\nfile 2\nkernel 3\nsock 4\nshmem 5\nslab 6\npagetables 7\nfile_dirty 8\n";
    const EVENTS: &[u8] = b"low 1\nhigh 2\nmax 3\noom 4\noom_kill 5\n";

    #[test]
    fn parses_complete_stats_and_keeps_missing_fields_absent() {
        let cpu = cpu_stat(CPU).unwrap();
        assert_eq!(
            (
                cpu.usage_usec,
                cpu.user_usec,
                cpu.system_usec,
                cpu.nr_periods,
                cpu.nr_throttled,
                cpu.throttled_usec
            ),
            (100, 70, 30, Some(9), Some(2), Some(8))
        );
        let mem = mem_stat(MEM);
        assert_eq!(
            (
                mem.anon,
                mem.file,
                mem.kernel,
                mem.sock,
                mem.shmem,
                mem.slab,
                mem.pagetables,
                mem.file_dirty
            ),
            (
                Some(1),
                Some(2),
                Some(3),
                Some(4),
                Some(5),
                Some(6),
                Some(7),
                Some(8)
            )
        );
        let events = mem_events(EVENTS);
        assert_eq!(
            (
                events.low,
                events.high,
                events.max,
                events.oom,
                events.oom_kill
            ),
            (Some(1), Some(2), Some(3), Some(4), Some(5))
        );
        assert_eq!(mem_stat(b"anon 0\n").file, None);
        let cpu = cpu_stat(b"usage_usec 2\nuser_usec 1\nsystem_usec 1\n").unwrap();
        assert_eq!(cpu.usage_usec, 2);
        assert_eq!(cpu.nr_periods, None);
        assert_eq!(single_u64(b"123\n"), Some(123));
        assert_eq!(single_u64(b"18446744073709551616"), None);
        assert_eq!(pids_max(b"max\n"), Some(Limit::Max));
        assert_eq!(pids_max(b"32\n"), Some(Limit::Value(32)));
        assert_eq!(memory_limit(b"8192\n"), Some(Limit::Value(8192)));
        assert_eq!(memory_limit(b"max\n"), Some(Limit::Max));
        assert_eq!(memory_limit(b""), None);
        assert_eq!(memory_limit(b"not-a-limit\n"), None);
    }

    #[test]
    fn parses_io_totals_and_rejects_bad_device_numbers() {
        assert_eq!(maj_min(b"259:0"), Some((259, 0)));
        assert_eq!(maj_min(b"259:0\n"), Some((259, 0)));
        assert_eq!(maj_min(b"999999999999:0"), None);
        assert_eq!(
            io_stat_sum(
                b"259:0 rbytes=5 wbytes=6 rios=7 wios=8\n259:0 rbytes=50\n254:0 rbytes=100\n",
                &[(259, 0)]
            ),
            IoTotals {
                rbytes: 5,
                wbytes: 6,
                rios: 7,
                wios: 8
            }
        );
    }

    #[test]
    fn every_fixture_prefix_and_garbage_is_panic_free() {
        for fixture in [CPU, MEM, EVENTS, b"garbage\n".as_slice()] {
            for end in 0..=fixture.len() {
                let bytes = &fixture[..end];
                let _cpu = cpu_stat(bytes);
                let _mem = mem_stat(bytes);
                let _events = mem_events(bytes);
                let _number = single_u64(bytes);
                let _limit = pids_max(bytes);
                let _io = io_stat_sum(bytes, &[(259, 0)]);
            }
        }
    }

    #[test]
    fn pressure_maps_some_and_full_and_rejects_empty_parse() {
        assert_eq!(
            super::pressure(b"some avg10=1.50 avg60=0.00 avg300=0.00 total=1234\nfull avg10=0.25 avg60=0.00 avg300=0.00 total=99\n"),
            Some(Pressure {
                some_avg10: Some(1.5),
                some_total_us: Some(1234),
                full_avg10: Some(0.25),
                full_total_us: Some(99),
            })
        );
        assert_eq!(super::pressure(b"garbage\n"), None);
        assert_eq!(super::pressure(b""), None);
    }
}
