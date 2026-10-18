//! Linux inet_diag TCP dump codec.
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::time::Instant;

use super::netlink::{AttributeIter, MessageIter, NetlinkMessage, NetlinkSocket};

// SOCK_DIAG_BY_FAMILY and INET_DIAG_INFO: include/uapi/linux/inet_diag.h.
const SOCK_DIAG_BY_FAMILY: u16 = 20;
const INET_DIAG_INFO: u16 = 2;
const NLMSG_DONE: u16 = libc::NLMSG_DONE as u16;
const NLMSG_ERROR: u16 = libc::NLMSG_ERROR as u16;
const NLM_F_DUMP_INTR: u16 = libc::NLM_F_DUMP_INTR as u16;
const REQUEST_FLAGS: u16 = (libc::NLM_F_REQUEST | libc::NLM_F_DUMP) as u16;
const AF_INET_U8: u8 = libc::AF_INET as u8;
const AF_INET6_U8: u8 = libc::AF_INET6 as u8;
const IPPROTO_TCP_U8: u8 = libc::IPPROTO_TCP as u8;
const NLMSG_HEADER_LEN: usize = 16;
const INET_DIAG_REQ_V2_LEN: usize = 56;
const INET_DIAG_MSG_LEN: usize = 72;
const TCP_INFO_KIND: u16 = 2;
const TCP_INFO_RTT_OFFSET: usize = 68;
const TCP_INFO_RTTVAR_OFFSET: usize = 72;
const TCP_INFO_SND_CWND_OFFSET: usize = 80;
const TCP_INFO_RETRANS_OFFSET: usize = 36;
const TCP_INFO_TOTAL_RETRANS_OFFSET: usize = 100;
const TCP_INFO_BYTES_ACKED_OFFSET: usize = 120;
const TCP_INFO_BYTES_RECEIVED_OFFSET: usize = 128;
const TCP_INFO_LAST_DATA_RECV_OFFSET: usize = 52;
pub const MAX_TCP_SOCKETS: usize = 4_096;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddressFamily {
    Ipv4,
    Ipv6,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TcpInfo {
    pub state: Option<u8>,
    pub rtt_us: Option<u32>,
    pub rttvar_us: Option<u32>,
    pub snd_cwnd: Option<u32>,
    pub retrans: Option<u32>,
    pub total_retrans: Option<u32>,
    pub bytes_acked: Option<u64>,
    pub bytes_received: Option<u64>,
    pub last_data_recv_ms: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TcpSock {
    pub family: AddressFamily,
    pub state: u8,
    pub source: IpAddr,
    pub destination: IpAddr,
    pub source_port: u16,
    pub destination_port: u16,
    pub inode: u32,
    pub uid: u32,
    pub rqueue: u32,
    pub wqueue: u32,
    pub info: Option<TcpInfo>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DumpStats {
    pub malformed: usize,
    pub truncated: bool,
}

/// Encodes inet_diag_req_v2 asking the kernel to dump TCP sockets in `states`.
pub fn encode_request(
    bytes: &mut [u8; NLMSG_HEADER_LEN + INET_DIAG_REQ_V2_LEN],
    family: AddressFamily,
    states: u32,
    sequence: u32,
) {
    bytes.fill(0);
    let request_len = bytes.len() as u32;
    put_u32(bytes, 0, request_len);
    put_u16(bytes, 4, SOCK_DIAG_BY_FAMILY);
    put_u16(bytes, 6, REQUEST_FLAGS);
    put_u32(bytes, 8, sequence);
    let req = NLMSG_HEADER_LEN;
    bytes[req] = match family {
        AddressFamily::Ipv4 => AF_INET_U8,
        AddressFamily::Ipv6 => AF_INET6_U8,
    };
    bytes[req + 1] = IPPROTO_TCP_U8;
    bytes[req + 2] = (1 << (INET_DIAG_INFO - 1)) as u8;
    put_u32(bytes, req + 4, states);
    // Zero ports/addresses/interface select every matching socket; cookies are all-ones.
    put_u32(bytes, req + 48, u32::MAX);
    put_u32(bytes, req + 52, u32::MAX);
}

/// Decodes one inet_diag_msg and any INET_DIAG_INFO attribute into a reusable output row.
pub fn decode_message(message: &NetlinkMessage<'_>) -> io::Result<TcpSock> {
    let payload = message.payload;
    if payload.len() < INET_DIAG_MSG_LEN {
        return Err(invalid("truncated inet_diag_msg"));
    }
    let family = match payload[0] {
        AF_INET_U8 => AddressFamily::Ipv4,
        AF_INET6_U8 => AddressFamily::Ipv6,
        _ => return Err(invalid("unsupported inet_diag family")),
    };
    let source_port = u16::from_be_bytes([payload[4], payload[5]]);
    let destination_port = u16::from_be_bytes([payload[6], payload[7]]);
    let (source, destination) = match family {
        AddressFamily::Ipv4 => (
            IpAddr::V4(Ipv4Addr::new(
                payload[8],
                payload[9],
                payload[10],
                payload[11],
            )),
            IpAddr::V4(Ipv4Addr::new(
                payload[24],
                payload[25],
                payload[26],
                payload[27],
            )),
        ),
        AddressFamily::Ipv6 => (
            IpAddr::V6(Ipv6Addr::from(
                <[u8; 16]>::try_from(&payload[8..24]).map_err(|_| invalid("bad IPv6 source"))?,
            )),
            IpAddr::V6(Ipv6Addr::from(
                <[u8; 16]>::try_from(&payload[24..40])
                    .map_err(|_| invalid("bad IPv6 destination"))?,
            )),
        ),
    };
    let mut info = None;
    for attribute in AttributeIter::new(&payload[INET_DIAG_MSG_LEN..]) {
        let Ok(attribute) = attribute else {
            info = None;
            break;
        };
        if attribute.kind == TCP_INFO_KIND {
            info = Some(decode_tcp_info(attribute.payload));
        }
    }
    Ok(TcpSock {
        family,
        state: payload[1],
        source,
        destination,
        source_port,
        destination_port,
        rqueue: get_u32(payload, 56).ok_or_else(|| invalid("missing receive queue"))?,
        wqueue: get_u32(payload, 60).ok_or_else(|| invalid("missing write queue"))?,
        uid: get_u32(payload, 64).ok_or_else(|| invalid("missing uid"))?,
        inode: get_u32(payload, 68).ok_or_else(|| invalid("missing inode"))?,
        info,
    })
}

/// Dumps IPv4 and IPv6 TCP sockets. `out` retains its allocation across calls.
pub fn dump_tcp(
    socket: &mut NetlinkSocket,
    out: &mut Vec<TcpSock>,
    deadline: Instant,
) -> io::Result<DumpStats> {
    out.clear();
    let mut stats = DumpStats::default();
    for family in [AddressFamily::Ipv4, AddressFamily::Ipv6] {
        let sequence = socket.next_sequence();
        let mut request = [0_u8; NLMSG_HEADER_LEN + INET_DIAG_REQ_V2_LEN];
        encode_request(&mut request, family, u32::MAX, sequence);
        socket.send(&request)?;
        loop {
            socket.recv(deadline)?;
            // Malformed rows are skipped and counted for the caller to report.
            let done = consume_and_record(socket.received(), sequence, out, &mut stats)?;
            if done {
                break;
            }
        }
    }
    Ok(stats)
}

fn consume_and_record(
    bytes: &[u8],
    sequence: u32,
    out: &mut Vec<TcpSock>,
    stats: &mut DumpStats,
) -> io::Result<bool> {
    let (done, malformed, truncated) = consume_batch(bytes, sequence, out)?;
    stats.malformed = stats.malformed.saturating_add(malformed);
    stats.truncated |= truncated;
    Ok(done)
}

fn consume_batch(
    bytes: &[u8],
    sequence: u32,
    out: &mut Vec<TcpSock>,
) -> io::Result<(bool, usize, bool)> {
    let mut messages = MessageIter::for_sequence(bytes, sequence);
    let mut malformed = 0;
    let mut truncated = false;
    for message in &mut messages {
        let message = message?;
        if message.header.sequence != sequence {
            continue;
        }
        if message.header.kind == NLMSG_DONE {
            if message.header.flags & NLM_F_DUMP_INTR != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "netlink dump interrupted",
                ));
            }
            if message.payload.len() >= 4 {
                let error = i32::from_ne_bytes([
                    message.payload[0],
                    message.payload[1],
                    message.payload[2],
                    message.payload[3],
                ]);
                if error < 0 {
                    return Err(io::Error::from_raw_os_error(error.saturating_neg()));
                }
            }
            return Ok((true, malformed, truncated));
        }
        if message.header.kind == NLMSG_ERROR {
            if message.payload.len() < 4 {
                return Err(invalid("truncated NLMSG_ERROR"));
            }
            let error = i32::from_ne_bytes([
                message.payload[0],
                message.payload[1],
                message.payload[2],
                message.payload[3],
            ]);
            if error < 0 {
                return Err(io::Error::from_raw_os_error(error.saturating_neg()));
            }
            continue;
        }
        if message.header.kind == SOCK_DIAG_BY_FAMILY {
            match decode_message(&message) {
                Ok(row) if out.len() < MAX_TCP_SOCKETS => out.push(row),
                Ok(_) => truncated = true,
                Err(_) => malformed += 1,
            }
        }
    }
    Ok((false, malformed, truncated))
}

fn decode_tcp_info(bytes: &[u8]) -> TcpInfo {
    TcpInfo {
        state: bytes.first().copied(),
        rtt_us: get_u32(bytes, TCP_INFO_RTT_OFFSET),
        rttvar_us: get_u32(bytes, TCP_INFO_RTTVAR_OFFSET),
        snd_cwnd: get_u32(bytes, TCP_INFO_SND_CWND_OFFSET),
        retrans: get_u32(bytes, TCP_INFO_RETRANS_OFFSET),
        total_retrans: get_u32(bytes, TCP_INFO_TOTAL_RETRANS_OFFSET),
        bytes_acked: get_u64(bytes, TCP_INFO_BYTES_ACKED_OFFSET),
        bytes_received: get_u64(bytes, TCP_INFO_BYTES_RECEIVED_OFFSET),
        last_data_recv_ms: get_u32(bytes, TCP_INFO_LAST_DATA_RECV_OFFSET),
    }
}

fn get_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_ne_bytes(
        bytes.get(offset..offset.checked_add(4)?)?.try_into().ok()?,
    ))
}

fn get_u64(bytes: &[u8], offset: usize) -> Option<u64> {
    Some(u64::from_ne_bytes(
        bytes.get(offset..offset.checked_add(8)?)?.try_into().ok()?,
    ))
}

fn put_u16(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_ne_bytes());
}

fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_ne_bytes());
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
#[path = "sock_diag_tests.rs"]
mod tests;
