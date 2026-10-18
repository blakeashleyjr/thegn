use super::{TcpProcRow, parse};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

#[test]
fn parses_ipv4_skips_header_and_counts_malformed_rows() {
    let input = b"  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt uid timeout inode\n\
0: 0100007F:1F90 0200007F:01BB 01 00000003:00000004 00:00000000 00000000 1000 0 7654321\n\
bad row\n";
    let mut rows: Vec<TcpProcRow> = Vec::new();
    assert_eq!(parse(input, false, &mut rows), 1);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].local, IpAddr::V4(Ipv4Addr::LOCALHOST));
    assert_eq!(rows[0].remote, IpAddr::V4(Ipv4Addr::new(127, 0, 0, 2)));
    assert_eq!((rows[0].local_port, rows[0].remote_port), (8080, 443));
    assert_eq!(
        (rows[0].state, rows[0].tx_queue, rows[0].rx_queue),
        (1, 3, 4)
    );
    assert_eq!((rows[0].uid, rows[0].inode), (1000, 7_654_321));
}

#[test]
fn parses_ipv6_as_four_little_endian_words() {
    let input = b"sl local_address rem_address st tx_queue rx_queue tr tm->when retrnsmt uid timeout inode\n\
0: 00000000000000000000000001000000:1F90 00000000000000000000000000000000:0050 0A 00000000:00000002 00:00000000 0 42 0 99\n";
    let mut rows = Vec::new();
    assert_eq!(parse(input, true, &mut rows), 0);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].local, IpAddr::V6(Ipv6Addr::LOCALHOST));
    assert_eq!(rows[0].remote, IpAddr::V6(Ipv6Addr::UNSPECIFIED));
    assert_eq!((rows[0].local_port, rows[0].remote_port), (8080, 80));
    assert_eq!((rows[0].state, rows[0].uid, rows[0].inode), (10, 42, 99));
}
