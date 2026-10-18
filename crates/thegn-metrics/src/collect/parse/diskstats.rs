//! Pure parser for Linux `/proc/diskstats` rows.
use super::{fields, parse_u64};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskstatsLine<'a> {
    pub major: u32,
    pub minor: u32,
    pub name: &'a [u8],
    pub reads: u64,
    pub reads_merged: u64,
    pub sectors_read: u64,
    pub ms_reading: u64,
    pub writes: u64,
    pub writes_merged: u64,
    pub sectors_written: u64,
    pub ms_writing: u64,
    pub in_flight: u64,
    pub io_ticks: u64,
    pub weighted_ms: u64,
    pub discard: Option<[u64; 4]>,
    pub flush: Option<[u64; 2]>,
}

pub fn parse_line(line: &[u8]) -> Option<DiskstatsLine<'_>> {
    let count = fields(line).count();
    if count < 14 || (count > 14 && count < 18) || count == 19 {
        return None;
    }
    let mut values = fields(line);
    let major = u32::try_from(parse_u64(values.next()?)?).ok()?;
    let minor = u32::try_from(parse_u64(values.next()?)?).ok()?;
    let name = values.next()?;
    if name.is_empty() {
        return None;
    }
    let reads = parse_u64(values.next()?)?;
    let reads_merged = parse_u64(values.next()?)?;
    let sectors_read = parse_u64(values.next()?)?;
    let ms_reading = parse_u64(values.next()?)?;
    let writes = parse_u64(values.next()?)?;
    let writes_merged = parse_u64(values.next()?)?;
    let sectors_written = parse_u64(values.next()?)?;
    let ms_writing = parse_u64(values.next()?)?;
    let in_flight = parse_u64(values.next()?)?;
    let io_ticks = parse_u64(values.next()?)?;
    let weighted_ms = parse_u64(values.next()?)?;
    let discard = if count >= 18 {
        Some([
            parse_u64(values.next()?)?,
            parse_u64(values.next()?)?,
            parse_u64(values.next()?)?,
            parse_u64(values.next()?)?,
        ])
    } else {
        None
    };
    let flush = if count >= 20 {
        Some([parse_u64(values.next()?)?, parse_u64(values.next()?)?])
    } else {
        None
    };
    Some(DiskstatsLine {
        major,
        minor,
        name,
        reads,
        reads_merged,
        sectors_read,
        ms_reading,
        writes,
        writes_merged,
        sectors_written,
        ms_writing,
        in_flight,
        io_ticks,
        weighted_ms,
        discard,
        flush,
    })
}

pub fn lines(input: &[u8]) -> impl Iterator<Item = DiskstatsLine<'_>> {
    input.split(|byte| *byte == b'\n').filter_map(parse_line)
}

pub fn is_virtual_name(name: &[u8]) -> bool {
    [b"dm-".as_slice(), b"md", b"loop", b"zram", b"ram", b"nbd"]
        .iter()
        .any(|prefix| name.starts_with(prefix))
}

#[cfg(test)]
mod tests {
    use super::{is_virtual_name, lines, parse_line};
    #[test]
    fn parses_counter_shapes_and_rejects_malformed_rows() {
        let row = parse_line(b"259 0 nvme0n1 1 2 10946927635 4 5 6 7 8 9 10 11 12 13 14 15 16 17")
            .unwrap();
        assert_eq!(row.sectors_read, 10946927635);
        assert_eq!(row.discard, Some([12, 13, 14, 15]));
        assert_eq!(row.flush, Some([16, 17]));
        let fourteen = parse_line(b"8 0 sda 1 2 3 4 5 6 7 8 9 10 11").unwrap();
        assert_eq!(fourteen.discard, None);
        assert_eq!(fourteen.flush, None);
        let eighteen = parse_line(b"8 0 sda 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15").unwrap();
        assert_eq!(eighteen.discard, Some([12, 13, 14, 15]));
        assert_eq!(eighteen.flush, None);
        for count in [15, 17, 19] {
            let counters = vec!["1"; count - 3].join(" ");
            let malformed = format!("8 0 sda {counters}");
            assert!(parse_line(malformed.as_bytes()).is_none());
        }
        for bad in [
            b"".as_slice(),
            b"259 0",
            b"259 0 x 1 2 3 4 5 6 7 8 9 10 11 12 13",
            b"259 0 x 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18",
            b"259 0 x 1 2 3 4 5 6 7 8 9 10 11 x",
            b"259 0 x 18446744073709551616 2 3 4 5 6 7 8 9 10 11",
        ] {
            assert!(parse_line(bad).is_none(), "{bad:?}");
        }
        assert_eq!(lines(b"bad\n8 0 sda 1 2 3 4 5 6 7 8 9 10 11\n").count(), 1);
    }
    #[test]
    fn virtual_prefixes_match_owner_rule() {
        for name in [
            b"dm-0".as_slice(),
            b"md127",
            b"loop7",
            b"zram0",
            b"ram0",
            b"nbd0",
        ] {
            assert!(is_virtual_name(name));
        }
        for name in [b"nvme0n1".as_slice(), b"sda", b"mmcblk0"] {
            assert!(!is_virtual_name(name));
        }
    }

    #[test]
    fn parses_the_box_20_field_capture() {
        let rows: Vec<_> =
            lines(include_bytes!("../../../tests/fixtures/disk/diskstats_20f")).collect();
        assert_eq!(rows.len(), 18);
        assert_eq!(rows[0].name, b"nvme0n1");
        assert_eq!(rows[0].sectors_read, 10_980_218_339);
        assert!(rows[0].flush.is_some());
    }
}
