//! Paired `/proc/net/snmp` header/value parser and selected counters.
use super::fields;

pub fn for_each(input: &[u8], mut visit: impl FnMut(&[u8], &[u8], Option<i64>)) {
    let mut lines = input.split(|byte| *byte == b'\n');
    let mut pending = None;
    while let Some(header) = pending.take().or_else(|| lines.next()) {
        let Some(values) = lines.next() else { break };
        let Some((section, names)) = split_section(header) else {
            continue;
        };
        let Some((value_section, numbers)) = split_section(values) else {
            continue;
        };
        if section != value_section {
            pending = Some(values);
            continue;
        }
        for (name, number) in fields(names).zip(fields(numbers)) {
            visit(section, name, parse_i64(number));
        }
    }
}

pub(super) fn non_negative(value: Option<i64>) -> Option<u64> {
    value.and_then(|number| u64::try_from(number).ok())
}

fn split_section(line: &[u8]) -> Option<(&[u8], &[u8])> {
    let colon = line.iter().position(|byte| *byte == b':')?;
    let section = &line[..colon];
    (!section.is_empty()).then_some((section, &line[colon + 1..]))
}

fn parse_i64(value: &[u8]) -> Option<i64> {
    let (negative, digits) = value
        .strip_prefix(b"-")
        .map_or((false, value), |v| (true, v));
    if digits.is_empty() {
        return None;
    }
    let magnitude = digits.iter().try_fold(0_i64, |number, digit| {
        if !digit.is_ascii_digit() {
            return None;
        }
        number
            .checked_mul(10)?
            .checked_add(i64::from(*digit - b'0'))
    })?;
    if negative {
        magnitude.checked_neg()
    } else {
        Some(magnitude)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SnmpCounters {
    pub ip_forwarding: Option<u64>,
    pub ip_in_receives: Option<u64>,
    pub ip_in_discards: Option<u64>,
    pub ip_out_requests: Option<u64>,
    pub ip_out_no_routes: Option<u64>,
    pub icmp_in_msgs: Option<u64>,
    pub icmp_out_msgs: Option<u64>,
    pub icmp_in_dest_unreachs: Option<u64>,
    pub icmp_out_dest_unreachs: Option<u64>,
    pub tcp_active_opens: Option<u64>,
    pub tcp_passive_opens: Option<u64>,
    pub tcp_attempt_fails: Option<u64>,
    pub tcp_estab_resets: Option<u64>,
    pub tcp_curr_estab: Option<u64>,
    pub tcp_in_segs: Option<u64>,
    pub tcp_out_segs: Option<u64>,
    pub tcp_retrans_segs: Option<u64>,
    pub tcp_in_errs: Option<u64>,
    pub tcp_out_rsts: Option<u64>,
    pub udp_in_datagrams: Option<u64>,
    pub udp_no_ports: Option<u64>,
    pub udp_in_errors: Option<u64>,
    pub udp_out_datagrams: Option<u64>,
    pub udp_rcvbuf_errors: Option<u64>,
    pub udp_sndbuf_errors: Option<u64>,
}

impl SnmpCounters {
    pub fn parse_into(input: &[u8], out: &mut Self) {
        *out = Self::default();
        for_each(input, |section, key, value| {
            let value = non_negative(value);
            match (section, key) {
                (b"Ip", b"Forwarding") => out.ip_forwarding = value,
                (b"Ip", b"InReceives") => out.ip_in_receives = value,
                (b"Ip", b"InDiscards") => out.ip_in_discards = value,
                (b"Ip", b"OutRequests") => out.ip_out_requests = value,
                (b"Ip", b"OutNoRoutes") => out.ip_out_no_routes = value,
                (b"Icmp", b"InMsgs") => out.icmp_in_msgs = value,
                (b"Icmp", b"OutMsgs") => out.icmp_out_msgs = value,
                (b"Icmp", b"InDestUnreachs") => out.icmp_in_dest_unreachs = value,
                (b"Icmp", b"OutDestUnreachs") => out.icmp_out_dest_unreachs = value,
                (b"Tcp", b"ActiveOpens") => out.tcp_active_opens = value,
                (b"Tcp", b"PassiveOpens") => out.tcp_passive_opens = value,
                (b"Tcp", b"AttemptFails") => out.tcp_attempt_fails = value,
                (b"Tcp", b"EstabResets") => out.tcp_estab_resets = value,
                (b"Tcp", b"CurrEstab") => out.tcp_curr_estab = value,
                (b"Tcp", b"InSegs") => out.tcp_in_segs = value,
                (b"Tcp", b"OutSegs") => out.tcp_out_segs = value,
                (b"Tcp", b"RetransSegs") => out.tcp_retrans_segs = value,
                (b"Tcp", b"InErrs") => out.tcp_in_errs = value,
                (b"Tcp", b"OutRsts") => out.tcp_out_rsts = value,
                (b"Udp", b"InDatagrams") => out.udp_in_datagrams = value,
                (b"Udp", b"NoPorts") => out.udp_no_ports = value,
                (b"Udp", b"InErrors") => out.udp_in_errors = value,
                (b"Udp", b"OutDatagrams") => out.udp_out_datagrams = value,
                (b"Udp", b"RcvbufErrors") => out.udp_rcvbuf_errors = value,
                (b"Udp", b"SndbufErrors") => out.udp_sndbuf_errors = value,
                _ => {}
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::{SnmpCounters, for_each};
    #[test]
    fn paired_lines_zip_by_position_and_keep_negative_values() {
        let input = b"Ip: Forwarding InReceives NewField InDiscards OutRequests OutNoRoutes\nIp: 1 2 77 4 5 6\nTcp: MaxConn ActiveOpens PassiveOpens\nTcp: -1 7\nUdp: InDatagrams NoPorts InErrors OutDatagrams RcvbufErrors SndbufErrors\nUdp: 10 11 12 13 14 15\n";
        let mut negative = None;
        for_each(input, |section, key, value| {
            if section == b"Tcp" && key == b"MaxConn" {
                negative = value;
            }
        });
        assert_eq!(negative, Some(-1));
        let mut counts = SnmpCounters::default();
        SnmpCounters::parse_into(input, &mut counts);
        assert_eq!(counts.ip_in_discards, Some(4));
        assert_eq!(counts.tcp_active_opens, Some(7));
        assert_eq!(counts.tcp_passive_opens, None);
        assert_eq!(counts.udp_sndbuf_errors, Some(15));
    }

    #[test]
    fn mismatched_pair_reuses_second_line_as_next_header() {
        let input = b"Ip: Forwarding\nTcp: MaxConn ActiveOpens\nTcp: -1 7\n";
        let mut counters = SnmpCounters::default();
        SnmpCounters::parse_into(input, &mut counters);
        assert_eq!(counters.tcp_active_opens, Some(7));
    }
}
