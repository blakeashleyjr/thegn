//! Selected extended network counters from `/proc/net/netstat`.
use super::snmp;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NetstatCounters {
    pub tcp_timeouts: Option<u64>,
    pub tcp_syn_retrans: Option<u64>,
    pub tcp_lost_retransmit: Option<u64>,
    pub listen_overflows: Option<u64>,
    pub listen_drops: Option<u64>,
    pub tcp_rcvq_drop: Option<u64>,
    pub tcp_backlog_drop: Option<u64>,
    pub tcp_memory_pressures: Option<u64>,
    pub ip_in_octets: Option<u64>,
    pub ip_out_octets: Option<u64>,
}

impl NetstatCounters {
    pub fn parse_into(input: &[u8], out: &mut Self) {
        *out = Self::default();
        snmp::for_each(input, |section, key, number| {
            let value = snmp::non_negative(number);
            match (section, key) {
                (b"TcpExt", b"TCPTimeouts") => out.tcp_timeouts = value,
                (b"TcpExt", b"TCPSynRetrans") => out.tcp_syn_retrans = value,
                (b"TcpExt", b"TCPLostRetransmit") => out.tcp_lost_retransmit = value,
                (b"TcpExt", b"ListenOverflows") => out.listen_overflows = value,
                (b"TcpExt", b"ListenDrops") => out.listen_drops = value,
                (b"TcpExt", b"TCPRcvQDrop") => out.tcp_rcvq_drop = value,
                (b"TcpExt", b"TCPBacklogDrop") => out.tcp_backlog_drop = value,
                (b"TcpExt", b"TCPMemoryPressures") => out.tcp_memory_pressures = value,
                (b"IpExt", b"InOctets") => out.ip_in_octets = value,
                (b"IpExt", b"OutOctets") => out.ip_out_octets = value,
                _ => {}
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::NetstatCounters;
    #[test]
    fn live_extended_rows_find_known_columns_after_insertions() {
        let input = b"TcpExt: SyncookiesSent TCPTimeouts TCPSynRetrans TCPLostRetransmit ListenOverflows ListenDrops TCPRcvQDrop TCPBacklogDrop TCPMemoryPressures\nTcpExt: 1 2 3 4 5 6 7 8 9\nIpExt: InOctets Extra OutOctets\nIpExt: 100 7 200\n";
        let mut counters = NetstatCounters::default();
        NetstatCounters::parse_into(input, &mut counters);
        assert_eq!(counters.tcp_timeouts, Some(2));
        assert_eq!(counters.tcp_memory_pressures, Some(9));
        assert_eq!(counters.ip_out_octets, Some(200));
        NetstatCounters::parse_into(b"TcpExt: TCPTimeouts\nTcpExt: 9\n", &mut counters);
        assert_eq!(counters.tcp_timeouts, Some(9));
        assert_eq!(counters.tcp_syn_retrans, None);
    }
}
