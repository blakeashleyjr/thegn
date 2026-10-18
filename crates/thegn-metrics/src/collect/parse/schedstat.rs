//! Allocation-free `/proc/schedstat` parser for versions 15 through 17.
use super::{fields, parse_u64};

pub const MAX_CPUS: usize = 512;
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CpuSchedRaw {
    pub run_delay_ns: u64,
    pub run_time_ns: u64,
    pub timeslices: u64,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseError {
    Unsupported,
    Invalid,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchedstatTable {
    pub version: u64,
    pub cpu_count: usize,
    pub cpus: [Option<CpuSchedRaw>; MAX_CPUS],
}
impl Default for SchedstatTable {
    fn default() -> Self {
        Self {
            version: 0,
            cpu_count: 0,
            cpus: [None; MAX_CPUS],
        }
    }
}

pub fn parse(bytes: &[u8], out: &mut SchedstatTable) -> Result<(), ParseError> {
    out.version = 0;
    out.cpu_count = 0;
    out.cpus.fill(None);
    let mut lines = bytes.split(|byte| *byte == b'\n');
    let Some(version_line) = lines.next() else {
        return Err(ParseError::Invalid);
    };
    let mut version_fields = fields(version_line);
    if version_fields.next() != Some(b"version".as_slice()) {
        return Err(ParseError::Invalid);
    }
    let Some(version) = version_fields.next().and_then(parse_u64) else {
        return Err(ParseError::Invalid);
    };
    if !(15..=17).contains(&version) {
        return Err(ParseError::Unsupported);
    }
    out.version = version;
    for line in lines {
        let mut words = fields(line);
        let Some(name) = words.next() else { continue };
        let Some(cpu_id) = name.strip_prefix(b"cpu").and_then(parse_u64) else {
            continue;
        };
        if cpu_id >= MAX_CPUS as u64 {
            continue;
        }
        let mut values = [0_u64; 9];
        let mut count = 0;
        for word in words {
            let Some(value) = parse_u64(word) else { break };
            if count == values.len() {
                break;
            }
            values[count] = value;
            count += 1;
        }
        if count < 9 {
            continue;
        }
        out.cpus[cpu_id as usize] = Some(CpuSchedRaw {
            run_delay_ns: values[6],
            run_time_ns: values[7],
            timeslices: values[8],
        });
        out.cpu_count = out.cpu_count.max(cpu_id as usize + 1);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{ParseError, SchedstatTable, parse};
    const V17: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/cpu/schedstat_v17.txt"
    ));
    const V99: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/cpu/schedstat_v99.txt"
    ));
    #[test]
    fn parses_all_v17_cpus_and_hides_unknown_version() {
        let mut table = SchedstatTable::default();
        assert_eq!(parse(V17, &mut table), Ok(()));
        assert_eq!(table.version, 17);
        assert_eq!(table.cpu_count, 24);
        assert!(table.cpus[..24].iter().all(Option::is_some));
        let cpu0 = table.cpus[0].unwrap();
        assert_eq!(cpu0.run_delay_ns, 101_612_179_810_580);
        assert_eq!(cpu0.run_time_ns, 221_999_373_497_909);
        assert_eq!(cpu0.timeslices, 663_603_432);
        assert_eq!(parse(V99, &mut table), Err(ParseError::Unsupported));
        for version in [15, 16] {
            let text = format!("version {version}\ncpu0 0 0 0 0 0 0 1 2 3\n");
            assert_eq!(parse(text.as_bytes(), &mut table), Ok(()));
        }
    }
}
