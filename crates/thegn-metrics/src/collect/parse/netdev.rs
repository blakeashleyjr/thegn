//! Header-keyed parser for `/proc/net/dev`.
use super::{fields, parse_u64};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NetdevCounters {
    pub rx_bytes: Option<u64>,
    pub rx_packets: Option<u64>,
    pub rx_errs: Option<u64>,
    pub rx_drop: Option<u64>,
    pub rx_fifo: Option<u64>,
    pub rx_frame: Option<u64>,
    pub rx_multicast: Option<u64>,
    pub tx_bytes: Option<u64>,
    pub tx_packets: Option<u64>,
    pub tx_errs: Option<u64>,
    pub tx_drop: Option<u64>,
    pub tx_fifo: Option<u64>,
    pub tx_colls: Option<u64>,
    pub tx_carrier: Option<u64>,
}

const RX: [&[u8]; 7] = [
    b"bytes",
    b"packets",
    b"errs",
    b"drop",
    b"fifo",
    b"frame",
    b"multicast",
];
const TX: [&[u8]; 7] = [
    b"bytes", b"packets", b"errs", b"drop", b"fifo", b"colls", b"carrier",
];

fn header_slots(input: &[u8]) -> Option<([Option<u8>; 32], usize)> {
    let mut lines = input.split(|byte| *byte == b'\n');
    lines.next()?;
    let header = lines.next()?;
    let rx = header.iter().position(|byte| *byte == b'|')?;
    let tx = rx + 1 + header[rx + 1..].iter().position(|byte| *byte == b'|')?;
    let mut slots = [None; 32];
    let mut count = 0;
    for name in fields(&header[rx + 1..tx]) {
        if count == slots.len() {
            break;
        }
        slots[count] = RX
            .iter()
            .position(|known| *known == name)
            .and_then(|slot| u8::try_from(slot).ok());
        count += 1;
    }
    for name in fields(&header[tx + 1..]) {
        if count == slots.len() {
            break;
        }
        slots[count] = TX
            .iter()
            .position(|known| *known == name)
            .and_then(|slot| u8::try_from(slot + 7).ok());
        count += 1;
    }
    Some((slots, count))
}

fn counters(values: &[Option<u64>; 32], slots: &[Option<u8>; 32], count: usize) -> NetdevCounters {
    let mut result = NetdevCounters::default();
    for (value, slot) in values.iter().zip(slots).take(count) {
        let (Some(value), Some(slot)) = (value, slot) else {
            continue;
        };
        let target = match slot {
            0 => &mut result.rx_bytes,
            1 => &mut result.rx_packets,
            2 => &mut result.rx_errs,
            3 => &mut result.rx_drop,
            4 => &mut result.rx_fifo,
            5 => &mut result.rx_frame,
            6 => &mut result.rx_multicast,
            7 => &mut result.tx_bytes,
            8 => &mut result.tx_packets,
            9 => &mut result.tx_errs,
            10 => &mut result.tx_drop,
            11 => &mut result.tx_fifo,
            12 => &mut result.tx_colls,
            13 => &mut result.tx_carrier,
            _ => continue,
        };
        *target = Some(*value);
    }
    result
}

/// Visit valid rows and return their count. Malformed rows and absent columns are skipped.
pub fn parse(input: &[u8], mut visit: impl FnMut(&[u8], &NetdevCounters)) -> Option<usize> {
    let (slots, slot_count) = header_slots(input)?;
    let mut rows = 0;
    for line in input.split(|byte| *byte == b'\n').skip(2) {
        let Some(colon) = line.iter().rposition(|byte| *byte == b':') else {
            continue;
        };
        let name = trim(&line[..colon]);
        if name.is_empty() {
            continue;
        }
        let mut values = [None; 32];
        let mut value_count = 0;
        let mut valid = true;
        for field in fields(&line[colon + 1..]) {
            let Some(value) = parse_u64(field) else {
                valid = false;
                break;
            };
            if value_count < values.len() {
                values[value_count] = Some(value);
            }
            value_count += 1;
        }
        if !valid || value_count == 0 {
            continue;
        }
        let counters = counters(&values, &slots, slot_count);
        visit(name, &counters);
        rows += 1;
    }
    Some(rows)
}

fn trim(mut value: &[u8]) -> &[u8] {
    while value.first().is_some_and(u8::is_ascii_whitespace) {
        value = &value[1..];
    }
    while value.last().is_some_and(u8::is_ascii_whitespace) {
        value = &value[..value.len() - 1];
    }
    value
}

#[cfg(test)]
mod tests {
    use super::{NetdevCounters, parse};
    const INPUT: &[u8] = b"Inter-| Receive | Transmit\n face |bytes packets errs drop fifo frame compressed multicast|bytes packets errs drop fifo colls carrier compressed\nwlan0: 4294967297 2 3 4 5 6 7 8 9 10 11 12 13 14 0\nwg0-mullvad:66432107634 2 3 4 5 6 7 8 9 10 11 12 13 14 0\neth0:alias: 1 2 3 4 5 6 7 8 9 10 11 12 13 14 0\n";

    #[test]
    fn maps_reordered_headers_unknown_columns_and_names_with_colons() {
        let input = b"Inter-| Receive | Transmit\nface |packets bytes mystery errs drop fifo frame multicast|carrier bytes packets errs drop fifo colls mystery\nwlan0: 2 4294967297 88 3 4 5 6 7 14 8 9 10 11 12 13 99\n";
        let mut seen = Vec::new();
        assert_eq!(
            parse(input, |name, row| seen.push((name.to_vec(), row.clone()))),
            Some(1)
        );
        assert_eq!(seen[0].0, b"wlan0");
        assert_eq!(seen[0].1.rx_bytes, Some(4_294_967_297));
        assert_eq!(seen[0].1.tx_bytes, Some(8));
        assert_eq!(seen[0].1.tx_carrier, Some(14));
    }

    #[test]
    fn missing_header_and_malformed_lines_are_safe() {
        assert_eq!(parse(b"", |_, _| {}), None);
        assert_eq!(parse(b"header\nno separators\nlo: xx\n", |_, _| {}), None);
        let mut row = NetdevCounters::default();
        let mut alias_seen = false;
        assert_eq!(
            parse(INPUT, |name, counters| {
                if name == b"wg0-mullvad" {
                    row = counters.clone();
                }
                if name == b"eth0:alias" {
                    alias_seen = true;
                }
            }),
            Some(3)
        );
        assert_eq!(row.rx_bytes, Some(66_432_107_634));
        assert!(alias_seen);
        assert!(
            parse(
                b"Inter-| Receive | Transmit\nface |bytes|bytes\nlo: 4\n",
                |_, _| {}
            )
            .is_some()
        );
    }
}
