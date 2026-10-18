//! `/proc/interrupts` parser; output storage belongs to caller.
use super::{MAX_CPUS, fields, parse_u64};

pub const MAX_ROWS: usize = 512;
pub const MAX_NAMED: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IrqRow {
    pub irq: u32,
    pub counts: Vec<u32>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedRow {
    pub hash: u64,
    pub counts: Vec<u32>,
}
#[derive(Debug)]
pub struct InterruptsTable {
    pub cpu_count: usize,
    pub cpu_ids: [u16; MAX_CPUS],
    pub numeric: Vec<IrqRow>,
    pub named: Vec<NamedRow>,
}
impl InterruptsTable {
    pub fn with_capacity() -> Self {
        Self {
            cpu_count: 0,
            cpu_ids: [0; MAX_CPUS],
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
    let mut numeric_rows = 0;
    let mut named_rows = 0;
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
        let line = trim(line);
        if line.is_empty() {
            continue;
        }
        let (label, tail) = match line.iter().position(|byte| *byte == b':') {
            Some(colon) => (trim(&line[..colon]), &line[colon + 1..]),
            None => continue,
        };
        if let Some(irq) = parse_u64(label) {
            if irq > u64::from(u32::MAX) || numeric_rows >= MAX_ROWS {
                continue;
            }
            if numeric_rows == out.numeric.len() {
                out.numeric.push(IrqRow {
                    irq: irq as u32,
                    counts: Vec::with_capacity(out.cpu_count),
                });
            }
            let row = &mut out.numeric[numeric_rows];
            row.irq = irq as u32;
            row.counts.clear();
            for token in fields(tail) {
                let Some(value) = parse_u64(token).and_then(|v| u32::try_from(v).ok()) else {
                    break;
                };
                if row.counts.len() == out.cpu_count {
                    break;
                }
                row.counts.push(value);
            }
            if row.counts.len() == out.cpu_count {
                numeric_rows += 1;
            }
            continue;
        }
        if named_rows >= MAX_NAMED {
            continue;
        }
        if named_rows == out.named.len() {
            out.named.push(NamedRow {
                hash: name_hash(label),
                counts: Vec::with_capacity(out.cpu_count),
            });
        }
        let row = &mut out.named[named_rows];
        row.hash = name_hash(label);
        row.counts.clear();
        for token in fields(tail) {
            let Some(value) = parse_u64(token).and_then(|v| u32::try_from(v).ok()) else {
                break;
            };
            if row.counts.len() == out.cpu_count {
                break;
            }
            row.counts.push(value);
        }
        if row.counts.len() >= out.cpu_count.saturating_sub(1) {
            named_rows += 1;
        }
    }
    out.numeric.truncate(numeric_rows);
    out.named.truncate(named_rows);
    true
}

pub fn name_hash(name: &[u8]) -> u64 {
    name.iter().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100_0000_01b3)
    })
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
    use super::{InterruptsTable, name_hash, parse};
    const CAPTURE: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/cpu/interrupts_24c.txt"
    ));
    #[test]
    fn parses_arch_neutral_ipis_numeric_rows_and_short_named_rows() {
        let mut table = InterruptsTable::with_capacity();
        assert!(parse(CAPTURE, &mut table));
        assert_eq!(table.cpu_count, 24);
        assert_eq!(table.cpu_ids[0], 0);
        assert_eq!(table.cpu_ids[23], 23);
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
                table.named.iter().any(|row| row.hash == name_hash(name)),
                "missing {name:?}"
            );
        }
        let cell = |name: &[u8], cpu: usize| {
            table
                .named
                .iter()
                .find(|row| row.hash == name_hash(name))
                .unwrap()
                .counts[cpu]
        };
        assert_eq!(
            (cell(b"LOC", 0), cell(b"LOC", 1), cell(b"LOC", 23)),
            (362_865_162, 327_749_704, 270_900_762)
        );
        assert_eq!(
            (cell(b"RES", 0), cell(b"RES", 23)),
            (30_831_258, 18_551_363)
        );
        assert_eq!(
            (cell(b"CAL", 0), cell(b"CAL", 23)),
            (645_504_627, 356_775_613)
        );
        assert_eq!(
            (cell(b"TLB", 0), cell(b"TLB", 23)),
            (166_525_239, 127_948_430)
        );
        assert_eq!(table.numeric.len(), 65);
        assert!(parse(
            b" CPU0 CPU1 CPU2\nNMI: 1 2 description\nLOC: 4 5 6 local\nXYZ: 6 7 8 custom\n7: 9 8 7 dev\n",
            &mut table
        ));
        assert_eq!(table.named.len(), 3);
        assert_eq!(table.named[1].counts.as_slice(), &[4, 5, 6]);
        assert_eq!(table.named[2].counts.as_slice(), &[6, 7, 8]);
        assert!(table.named.iter().any(|row| row.hash == name_hash(b"XYZ")));
        assert_eq!(table.numeric.len(), 1);
    }
}
