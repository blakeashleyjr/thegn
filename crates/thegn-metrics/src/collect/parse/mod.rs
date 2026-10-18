//! Allocation-free byte parsers shared by collector readers.
pub mod amd_gpu_metrics;
pub mod cgroup;
pub mod cpufreq;
pub mod diskstats;
pub mod interrupts;
pub mod loadavg;
pub mod meminfo;
pub mod mountinfo;
pub mod netdev;
pub mod netstat;
pub mod psi;
pub mod schedstat;
pub mod snmp;
pub mod softirqs;
pub mod softnet;
pub mod stat;
pub mod swaps;
pub mod upower_history;
pub mod uptime;
pub mod vmstat;
pub mod zram;

pub const MAX_CPUS: usize = 512;

pub fn fields(input: &[u8]) -> impl Iterator<Item = &[u8]> {
    input
        .split(u8::is_ascii_whitespace)
        .filter(|field| !field.is_empty())
}

pub fn parse_u64(input: &[u8]) -> Option<u64> {
    if input.is_empty() {
        return None;
    }
    input.iter().try_fold(0_u64, |value, digit| {
        if !digit.is_ascii_digit() {
            return None;
        }
        value.checked_mul(10)?.checked_add(u64::from(*digit - b'0'))
    })
}

pub fn kv_lines(input: &[u8]) -> impl Iterator<Item = (&[u8], &[u8])> {
    input.split(|byte| *byte == b'\n').filter_map(|line| {
        let colon = line.iter().position(|byte| *byte == b':')?;
        let key = &line[..colon];
        if key.is_empty() {
            return None;
        }
        Some((key, trim_ascii(&line[colon + 1..])))
    })
}

pub fn kv_u64(value: &[u8]) -> Option<u64> {
    parse_u64(fields(value).next()?)
}

pub fn find_line<'a>(input: &'a [u8], prefix: &[u8]) -> Option<&'a [u8]> {
    input
        .split(|byte| *byte == b'\n')
        .find_map(|line| line.strip_prefix(prefix))
}

fn trim_ascii(input: &[u8]) -> &[u8] {
    let start = input
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(input.len());
    let end = input
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .map_or(start, |index| index + 1);
    &input[start..end]
}

#[cfg(test)]
mod tests {
    use super::{fields, find_line, kv_lines, kv_u64, parse_u64};
    #[test]
    fn helper_edges_do_not_allocate_or_accept_partial_numbers() {
        assert_eq!(parse_u64(b"18446744073709551616"), None);
        assert_eq!(parse_u64(b"12x"), None);
        assert_eq!(kv_u64(b"123 kB"), Some(123));
        assert_eq!(find_line(b"prefix X: 2\nX: 3\n", b"X:"), Some(&b" 3"[..]));
        assert_eq!(fields(b"  \t\n").count(), 0);
        assert_eq!(kv_lines(b"").count(), 0);
    }
}
