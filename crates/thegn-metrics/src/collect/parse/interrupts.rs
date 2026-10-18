//! Allocation-free `/proc/interrupts` parser; output storage belongs to caller.
use super::{fields, parse_u64};

pub const MAX_CPUS: usize = 512;
pub const MAX_ROWS: usize = 512;
pub const MAX_NAMED: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IrqRow {
    pub irq: u32,
    pub counts: [u64; MAX_CPUS],
}
impl Default for IrqRow {
    fn default() -> Self {
        Self {
            irq: 0,
            counts: [0; MAX_CPUS],
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NamedRow {
    pub hash: u64,
    pub counts: [u64; MAX_CPUS],
}
impl Default for NamedRow {
    fn default() -> Self {
        Self {
            hash: 0,
            counts: [0; MAX_CPUS],
        }
    }
}
#[derive(Debug)]
pub struct InterruptsTable {
    pub cpu_count: usize,
    pub numeric: Vec<IrqRow>,
    pub named: Vec<NamedRow>,
}
impl InterruptsTable {
    pub fn with_capacity() -> Self {
        Self {
            cpu_count: 0,
            numeric: Vec::with_capacity(MAX_ROWS),
            named: Vec::with_capacity(MAX_NAMED),
        }
    }
}
impl Default for InterruptsTable {
    fn default() -> Self {
        Self::with_capacity()
    }
}

pub fn parse(bytes: &[u8], out: &mut InterruptsTable) -> bool {
    out.cpu_count = 0;
    out.numeric.clear();
    out.named.clear();
    let mut lines = bytes.split(|byte| *byte == b'\n');
    let Some(header) = lines.next() else {
        return false;
    };
    for field in fields(header) {
        if field
            .strip_prefix(b"CPU")
            .and_then(parse_u64)
            .is_some_and(|id| id < MAX_CPUS as u64)
        {
            out.cpu_count += 1;
        }
    }
    if out.cpu_count == 0 {
        return false;
    }
    for line in lines {
        let line = trim(line);
        if line.is_empty() {
            continue;
        }
        let (label, tail) = match line.iter().position(|byte| *byte == b':') {
            Some(colon) => (trim(&line[..colon]), &line[colon + 1..]),
            None => continue,
        };
        if let Some(irq) = parse_u64(label) {
            if irq > u64::from(u32::MAX)
                || out.numeric.len() >= MAX_ROWS
                || out.numeric.len() == out.numeric.capacity()
            {
                continue;
            }
            let mut row = IrqRow {
                irq: irq as u32,
                ..IrqRow::default()
            };
            let mut count = 0;
            for token in fields(tail) {
                let Some(value) = parse_u64(token) else { break };
                if count == out.cpu_count {
                    break;
                }
                row.counts[count] = value;
                count += 1;
            }
            if count == out.cpu_count {
                out.numeric.push(row);
            }
            continue;
        }
        if out.named.len() >= MAX_NAMED || out.named.len() == out.named.capacity() {
            continue;
        }
        let mut row = NamedRow {
            hash: name_hash(label),
            ..NamedRow::default()
        };
        let mut count = 0;
        for token in fields(tail) {
            let Some(value) = parse_u64(token) else { break };
            if count == out.cpu_count {
                break;
            }
            row.counts[count] = value;
            count += 1;
        }
        if count >= out.cpu_count.saturating_sub(1) {
            out.named.push(row);
        }
    }
    true
}

pub fn name_hash(name: &[u8]) -> u64 {
    name.iter().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100_0000_01b3)
    })
}
pub fn hash_name(name: &[u8]) -> u64 {
    name_hash(name)
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
    use super::{InterruptsTable, hash_name, parse};
    const CAPTURE: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/cpu/interrupts_24c.txt"
    ));
    #[test]
    fn parses_arch_neutral_ipis_numeric_rows_and_short_named_rows() {
        let mut table = InterruptsTable::with_capacity();
        assert!(parse(CAPTURE, &mut table));
        assert_eq!(table.cpu_count, 24);
        for name in [
            b"LOC".as_slice(),
            b"RES",
            b"CAL",
            b"TLB",
            b"THR",
            b"MCE",
            b"DFR",
        ] {
            assert!(
                table.named.iter().any(|row| row.hash == hash_name(name)),
                "missing {name:?}"
            );
        }
        assert_eq!(table.numeric.len(), 65);
        assert!(parse(
            b" CPU0 CPU1 CPU2\nNMI: 1 2 description\nLOC: 4 5 6 local\nXYZ: 6 7 8 custom\n7: 9 8 7 dev\n",
            &mut table
        ));
        assert_eq!(table.named.len(), 3);
        assert_eq!(table.named[0].counts[1], 2);
        assert!(table.named.iter().any(|row| row.hash == hash_name(b"XYZ")));
        assert_eq!(table.numeric.len(), 1);
    }
}
