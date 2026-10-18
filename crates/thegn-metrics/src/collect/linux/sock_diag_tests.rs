use super::{AddressFamily, TcpInfo, decode_message, encode_request};
use crate::collect::linux::netlink::{MessageIter, NetlinkHeader, NetlinkMessage};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

fn response(family: AddressFamily, info_len: usize) -> Vec<u8> {
    let mut payload = vec![0_u8; 72];
    payload[0] = match family {
        AddressFamily::Ipv4 => 2,
        AddressFamily::Ipv6 => 10,
    };
    payload[1] = 1;
    payload[4..6].copy_from_slice(&443_u16.to_be_bytes());
    payload[6..8].copy_from_slice(&8080_u16.to_be_bytes());
    match family {
        AddressFamily::Ipv4 => {
            payload[8..12].copy_from_slice(&[127, 0, 0, 1]);
            payload[24..28].copy_from_slice(&[10, 1, 2, 3]);
        }
        AddressFamily::Ipv6 => {
            payload[8..24].copy_from_slice(&Ipv6Addr::LOCALHOST.octets());
            payload[24..40].copy_from_slice(&"2001:db8::1".parse::<Ipv6Addr>().unwrap().octets());
        }
    }
    payload[56..60].copy_from_slice(&17_u32.to_ne_bytes());
    payload[60..64].copy_from_slice(&19_u32.to_ne_bytes());
    payload[64..68].copy_from_slice(&1000_u32.to_ne_bytes());
    payload[68..72].copy_from_slice(&9876_u32.to_ne_bytes());
    if info_len > 0 {
        let attr_len = 4 + info_len;
        payload.extend_from_slice(&(attr_len as u16).to_ne_bytes());
        payload.extend_from_slice(&2_u16.to_ne_bytes());
        let mut info = vec![0_u8; info_len];
        info[0] = 1;
        if info_len >= 40 {
            info[36..40].copy_from_slice(&7_u32.to_ne_bytes());
        }
        if info_len >= 76 {
            info[68..72].copy_from_slice(&321_u32.to_ne_bytes());
            info[72..76].copy_from_slice(&654_u32.to_ne_bytes());
        }
        payload.extend_from_slice(&info);
        payload.resize((payload.len() + 3) & !3, 0);
    }
    payload
}

fn decode(payload: &[u8]) -> super::TcpSock {
    let len = 16 + payload.len();
    let header = NetlinkHeader {
        len: len as u32,
        kind: 20,
        flags: 0,
        sequence: 1,
        pid: 0,
    };
    decode_message(&NetlinkMessage { header, payload }).unwrap()
}

#[test]
fn encoder_builds_v4_and_v6_dump_requests_with_tcp_info_ext() {
    for family in [AddressFamily::Ipv4, AddressFamily::Ipv6] {
        let mut request = [0_u8; 72];
        encode_request(&mut request, family, 0x1234, 9);
        assert_eq!(
            u32::from_ne_bytes(request[0..4].try_into().unwrap()) as usize,
            request.len()
        );
        assert_eq!(
            request[16],
            match family {
                AddressFamily::Ipv4 => 2,
                AddressFamily::Ipv6 => 10,
            }
        );
        assert_eq!(request[17], 6);
        assert_eq!(request[18], 2);
        assert_eq!(u32::from_ne_bytes(request[8..12].try_into().unwrap()), 9);
        assert_eq!(
            u32::from_ne_bytes(request[20..24].try_into().unwrap()),
            0x1234
        );
    }
}

#[test]
fn decoder_round_trips_ipv4_and_ipv6_endpoints_and_ports() {
    let v4 = decode(&response(AddressFamily::Ipv4, 0));
    assert_eq!(v4.source, IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)));
    assert_eq!(v4.destination, IpAddr::V4(Ipv4Addr::new(10, 1, 2, 3)));
    assert_eq!((v4.source_port, v4.destination_port), (443, 8080));
    assert_eq!(
        (v4.rqueue, v4.wqueue, v4.uid, v4.inode),
        (17, 19, 1000, 9876)
    );

    let v6 = decode(&response(AddressFamily::Ipv6, 0));
    assert_eq!(v6.source, IpAddr::V6(Ipv6Addr::LOCALHOST));
    assert_eq!(v6.destination, IpAddr::V6("2001:db8::1".parse().unwrap()));
    assert_eq!((v6.source_port, v6.destination_port), (443, 8080));
}

#[test]
fn truncated_tcp_info_keeps_complete_rtt_fields_only() {
    let row = decode(&response(AddressFamily::Ipv4, 76));
    assert_eq!(
        row.info,
        Some(TcpInfo {
            state: Some(1),
            rtt_us: Some(321),
            rttvar_us: Some(654),
            snd_cwnd: None,
            retrans: Some(7),
            total_retrans: None,
            bytes_acked: None,
            bytes_received: None,
        })
    );
}

#[test]
fn non_word_aligned_tcp_info_cut_keeps_only_complete_fields() {
    let row = decode(&response(AddressFamily::Ipv4, 70));
    let info = row.info.unwrap();
    assert_eq!(info.retrans, Some(7));
    assert_eq!(info.rtt_us, None);
    assert_eq!(info.rttvar_us, None);
    assert_eq!(info.snd_cwnd, None);
    assert_eq!(info.total_retrans, None);
    assert_eq!(info.bytes_acked, None);
    assert_eq!(info.bytes_received, None);
}

#[test]
fn truncated_trailing_attribute_degrades_tcp_info_only() {
    let mut payload = response(AddressFamily::Ipv4, 0);
    payload.extend_from_slice(&[8, 0, 2, 0, 1, 2, 3, 4]);
    payload.extend_from_slice(&[8, 0, 2, 0, 1, 2, 3]);
    let row = decode(&payload);
    assert_eq!(row.info, None);
}

fn frame(kind: u16, sequence: u32, payload: &[u8]) -> Vec<u8> {
    let len = 16 + payload.len();
    let mut bytes = Vec::with_capacity((len + 3) & !3);
    bytes.extend_from_slice(&(len as u32).to_ne_bytes());
    bytes.extend_from_slice(&kind.to_ne_bytes());
    bytes.extend_from_slice(&0_u16.to_ne_bytes());
    bytes.extend_from_slice(&sequence.to_ne_bytes());
    bytes.extend_from_slice(&0_u32.to_ne_bytes());
    bytes.extend_from_slice(payload);
    bytes.resize((len + 3) & !3, 0);
    bytes
}

#[test]
fn framed_response_iterates_through_message_iter_before_decode() {
    let mut bytes = frame(20, 4, &response(AddressFamily::Ipv4, 0));
    bytes.extend_from_slice(&frame(libc::NLMSG_DONE as u16, 4, &[]));
    let mut messages = MessageIter::new(&bytes);
    let message = messages.next().unwrap().unwrap();
    let row = decode_message(&message).unwrap();
    assert_eq!(row.source, IpAddr::V4(Ipv4Addr::LOCALHOST));
    assert!(messages.next().is_none());
    assert!(messages.is_done());
}

#[test]
fn stale_done_does_not_end_current_sequence_dump() {
    let mut bytes = frame(libc::NLMSG_DONE as u16, 3, &[]);
    bytes.extend_from_slice(&frame(20, 4, &response(AddressFamily::Ipv4, 0)));
    bytes.extend_from_slice(&frame(libc::NLMSG_DONE as u16, 4, &[]));
    let mut rows = Vec::new();
    let (done, malformed) = super::consume_batch(&bytes, 4, &mut rows).unwrap();
    assert!(done);
    assert_eq!(malformed, 0);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].source, IpAddr::V4(Ipv4Addr::LOCALHOST));
}

#[test]
fn negative_error_in_done_payload_is_reported() {
    let bytes = frame(libc::NLMSG_DONE as u16, 5, &(-13_i32).to_ne_bytes());
    let result = super::consume_batch(&bytes, 5, &mut Vec::new());
    assert!(matches!(result, Err(error) if error.raw_os_error() == Some(libc::EACCES)));
}

#[test]
fn malformed_diag_rows_are_counted_for_the_dump_result() {
    let mut bytes = frame(20, 7, &[1, 2, 3]);
    bytes.extend_from_slice(&frame(libc::NLMSG_DONE as u16, 7, &[]));
    let mut rows = Vec::new();
    let mut stats = super::DumpStats::default();
    let done = super::consume_and_record(&bytes, 7, &mut rows, &mut stats).unwrap();
    assert!(done);
    assert!(rows.is_empty());
    assert_eq!(stats.malformed, 1);
}
