//! Allocation-free parsers for Linux CPU frequency sysfs values.
//! Numeric values accept digits only, stricter than the old `str::parse` shim's leading `+`.
use super::super::model::cpu_policy::{CoreClass, CpuSet, SysToken, TokenList};

pub fn parse_khz(bytes: &[u8]) -> Option<u64> {
    let value = parse_u64_trimmed(bytes)?;
    (value > 0).then_some(value)
}
pub fn parse_u32(bytes: &[u8]) -> Option<u32> {
    u32::try_from(parse_u64_trimmed(bytes)?).ok()
}
/// Scheduler `cpu_capacity`: decimal digits in `1..=1024`, surrounding ASCII
/// whitespace tolerated. Empty, signed, hex, out-of-range and overflowing
/// values are rejected.
pub fn parse_capacity(bytes: &[u8]) -> Option<u16> {
    u16::try_from(parse_u64_trimmed(bytes)?)
        .ok()
        .filter(|value| (1..=1024).contains(value))
}
/// Classify one capacity against the smallest and largest capacity seen across
/// all policies. A machine with a single distinct capacity is homogeneous and
/// has no classes, so everything is `Unknown`; otherwise the maximum is
/// `Performance` and anything strictly below it is `Efficiency`.
pub fn classify_by_capacity(capacity: u16, min_seen: u16, max_seen: u16) -> CoreClass {
    if min_seen >= max_seen || capacity > max_seen {
        CoreClass::Unknown
    } else if capacity == max_seen {
        CoreClass::Performance
    } else {
        CoreClass::Efficiency
    }
}
pub fn parse_flag(bytes: &[u8]) -> Option<bool> {
    match trim(bytes) {
        b"1" | b"enabled" => Some(true),
        b"0" | b"disabled" => Some(false),
        _ => None,
    }
}
pub fn parse_cpu_list(bytes: &[u8], set: &mut CpuSet) -> bool {
    let input = trim(bytes);
    if input.is_empty() {
        return false;
    }
    let mut candidate = CpuSet::default();
    let mut saw = false;
    for group in input.split(|b| *b == b',') {
        let group = trim(group);
        if group.is_empty() {
            return false;
        }
        for piece in group
            .split(u8::is_ascii_whitespace)
            .filter(|piece| !piece.is_empty())
        {
            let mut ends = piece.split(|b| *b == b'-');
            let Some(first) = ends.next().and_then(parse_u32) else {
                return false;
            };
            let last = match ends.next() {
                Some(end) => match parse_u32(end) {
                    Some(value) => value,
                    None => return false,
                },
                None => first,
            };
            if ends.next().is_some() || last < first {
                return false;
            }
            if first < 512 {
                for cpu in first..=last.min(511) {
                    candidate.insert(cpu);
                }
            }
            candidate.truncated |= last >= 512;
            saw = true;
        }
    }
    if saw {
        *set = candidate;
    }
    saw
}
pub fn parse_token(bytes: &[u8]) -> Option<SysToken> {
    SysToken::new(trim(bytes))
}
pub fn parse_token_list(bytes: &[u8]) -> TokenList {
    let mut list = TokenList::default();
    for token in trim(bytes)
        .split(u8::is_ascii_whitespace)
        .filter(|token| !token.is_empty())
    {
        let Some(token) = SysToken::new(token) else {
            continue;
        };
        if !list.push(token) {
            break;
        }
    }
    list
}
fn parse_u64_trimmed(bytes: &[u8]) -> Option<u64> {
    let bytes = trim(bytes);
    if bytes.is_empty() {
        return None;
    }
    bytes.iter().try_fold(0_u64, |n, b| {
        if b.is_ascii_digit() {
            n.checked_mul(10)?.checked_add(u64::from(*b - b'0'))
        } else {
            None
        }
    })
}
fn trim(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|b| !b.is_ascii_whitespace())
        .map_or(start, |i| i + 1);
    &bytes[start..end]
}

#[cfg(test)]
mod tests {
    use super::{
        classify_by_capacity, parse_capacity, parse_cpu_list, parse_flag, parse_khz,
        parse_token_list, parse_u32,
    };
    use crate::collect::model::cpu_policy::{CoreClass, CpuSet};
    #[test]
    fn capacity_parser_accepts_terminated_and_unterminated_values() {
        for (input, expected) in [
            (b"1024".as_slice(), Some(1024)),
            (b"1024\n", Some(1024)),
            (b"512 ", Some(512)),
            (b"512 \n", Some(512)),
            (b"1", Some(1)),
            (b"1\n", Some(1)),
        ] {
            assert_eq!(parse_capacity(input), expected, "{input:?}");
        }
        for bad in [
            b"".as_slice(),
            b"\n",
            b"abc",
            b"abc\n",
            b"-5",
            b"+5",
            b"0x400",
            b"0",
            b"0\n",
            b"1025",
            b"99999999999",
            b"99999999999999999999999",
            b"5 12",
        ] {
            assert_eq!(parse_capacity(bad), None, "{bad:?}");
        }
    }
    #[test]
    fn capacity_classification_needs_two_distinct_values() {
        assert_eq!(
            classify_by_capacity(1024, 512, 1024),
            CoreClass::Performance
        );
        assert_eq!(classify_by_capacity(512, 512, 1024), CoreClass::Efficiency);
        assert_eq!(classify_by_capacity(768, 512, 1024), CoreClass::Efficiency);
        assert_eq!(classify_by_capacity(1024, 1024, 1024), CoreClass::Unknown);
        assert_eq!(classify_by_capacity(2000, 512, 1024), CoreClass::Unknown);
    }
    #[test]
    fn parsers_cover_frequency_flags_tokens_and_cpu_lists() {
        assert_eq!(parse_khz(b" \t1237661\r\n"), Some(1_237_661));
        assert_eq!(parse_khz(b"0"), None);
        assert_eq!(parse_khz(b"12x"), None);
        assert_eq!(parse_u32(b"4294967296"), None);
        assert_eq!(parse_flag(b"enabled"), Some(true));
        assert_eq!(parse_flag(b"0\n"), Some(false));
        assert_eq!(parse_flag(b"yes"), None);
        for (input, expected) in [
            (b"0".as_slice(), &[0][..]),
            (b"0-3,8,10-11", &[0, 1, 2, 3, 8, 10, 11]),
            (b"0 1 2", &[0, 1, 2]),
        ] {
            let mut set = CpuSet::default();
            assert!(parse_cpu_list(input, &mut set));
            assert_eq!(set.iter().collect::<Vec<_>>(), expected);
        }
        for bad in [b"".as_slice(), b"1-x", b"3-1", b"1,,2", b"1-2-3"] {
            assert!(!parse_cpu_list(bad, &mut CpuSet::default()));
        }
        assert_eq!(parse_token_list(b"performance powersave").len(), 2);
    }
}
