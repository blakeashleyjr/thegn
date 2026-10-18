//! Allocation-free `/proc/softirqs` parser.
use super::{MAX_CPUS, fields, parse_u64};
pub const TYPES: [&[u8]; 10] = [
    b"HI",
    b"TIMER",
    b"NET_TX",
    b"NET_RX",
    b"BLOCK",
    b"IRQ_POLL",
    b"TASKLET",
    b"SCHED",
    b"HRTIMER",
    b"RCU",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SoftirqTable {
    pub cpu_count: usize,
    pub cpu_ids: [u16; MAX_CPUS],
    pub counts: [[u32; MAX_CPUS]; 10],
}
impl Default for SoftirqTable {
    fn default() -> Self {
        Self {
            cpu_count: 0,
            cpu_ids: [0; MAX_CPUS],
            counts: [[0; MAX_CPUS]; 10],
        }
    }
}

pub fn parse(bytes: &[u8], out: &mut SoftirqTable) -> bool {
    out.cpu_count = 0;
    out.counts.iter_mut().for_each(|row| row.fill(0));
    let mut lines = bytes.split(|byte| *byte == b'\n');
    let Some(header) = lines.next() else {
        return false;
    };
    for field in fields(header) {
        if let Some(id) = field.strip_prefix(b"CPU").and_then(parse_u64)
            && id < MAX_CPUS as u64
            && out.cpu_count < MAX_CPUS
        {
            out.cpu_ids[out.cpu_count] = id as u16;
            out.cpu_count += 1;
        }
    }
    if out.cpu_count == 0 {
        return false;
    }
    for line in lines {
        let Some(colon) = line.iter().position(|byte| *byte == b':') else {
            continue;
        };
        let name = trim(&line[..colon]);
        let Some(row) = TYPES.iter().position(|known| *known == name) else {
            continue;
        };
        let mut count = 0;
        for value in fields(&line[colon + 1..]) {
            let Some(value) = parse_u64(value).and_then(|v| u32::try_from(v).ok()) else {
                return false;
            };
            if count >= out.cpu_count || count >= MAX_CPUS {
                return false;
            }
            out.counts[row][count] = value;
            count += 1;
        }
        if count != out.cpu_count {
            return false;
        }
    }
    true
}

fn trim(mut bytes: &[u8]) -> &[u8] {
    while bytes.first().is_some_and(u8::is_ascii_whitespace) {
        bytes = &bytes[1..];
    }
    while bytes.last().is_some_and(u8::is_ascii_whitespace) {
        bytes = &bytes[..bytes.len() - 1];
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::{SoftirqTable, parse};
    const CAPTURE: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/cpu/softirqs_24c.txt"
    ));
    #[test]
    fn parses_real_cpu_columns_and_rejects_short_rows() {
        let mut table = SoftirqTable::default();
        assert!(parse(CAPTURE, &mut table));
        assert_eq!(table.cpu_count, 24);
        assert_eq!(table.cpu_ids[0], 0);
        assert_eq!(table.cpu_ids[23], 23);
        assert_eq!(table.counts[1][0], 15_461_207);
        assert_eq!(table.counts[3][23], 895_249);
        assert!(!parse(b" CPU0 CPU1\n TIMER: 1\n", &mut table));
    }
}
