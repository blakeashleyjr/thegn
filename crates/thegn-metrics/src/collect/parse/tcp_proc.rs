//! Pure parser for `/proc/net/tcp` and `/proc/net/tcp6` rows.
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TcpProcRow {
    pub local: IpAddr,
    pub remote: IpAddr,
    pub local_port: u16,
    pub remote_port: u16,
    pub state: u8,
    pub tx_queue: u32,
    pub rx_queue: u32,
    pub uid: u32,
    pub inode: u32,
}

/// Parses valid rows into a reusable vector and returns the number of malformed data rows.
/// The heading is skipped; empty lines are ignored.
pub fn parse(input: &[u8], ipv6: bool, out: &mut Vec<TcpProcRow>) -> usize {
    out.clear();
    let mut malformed = 0;
    for (index, line) in input.split(|byte| *byte == b'\n').enumerate() {
        if index == 0 || line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        match parse_row(line, ipv6) {
            Some(row) => out.push(row),
            None => malformed += 1,
        }
    }
    malformed
}

fn parse_row(line: &[u8], ipv6: bool) -> Option<TcpProcRow> {
    let mut fields = line
        .split(u8::is_ascii_whitespace)
        .filter(|field| !field.is_empty());
    fields.next()?;
    let (local, local_port) = parse_endpoint(fields.next()?, ipv6)?;
    let (remote, remote_port) = parse_endpoint(fields.next()?, ipv6)?;
    let state = u8::try_from(parse_hex(fields.next()?)?).ok()?;
    let queues = fields.next()?;
    let colon = queues.iter().position(|byte| *byte == b':')?;
    let (tx_queue, rx_queue) = queues.split_at(colon);
    let rx_queue = &rx_queue[1..];
    fields.next()?;
    fields.next()?;
    let uid = fields.next()?;
    let _timeout = fields.next()?;
    let inode = fields.next()?;
    Some(TcpProcRow {
        local,
        remote,
        local_port,
        remote_port,
        state,
        tx_queue: u32::try_from(parse_hex(tx_queue)?).ok()?,
        rx_queue: u32::try_from(parse_hex(rx_queue)?).ok()?,
        uid: u32::try_from(parse_decimal(uid)?).ok()?,
        inode: u32::try_from(parse_decimal(inode)?).ok()?,
    })
}

fn parse_endpoint(input: &[u8], ipv6: bool) -> Option<(IpAddr, u16)> {
    let colon = input.iter().rposition(|byte| *byte == b':')?;
    let (address_hex, port_hex) = input.split_at(colon);
    let port = u16::try_from(parse_hex(&port_hex[1..])?).ok()?;
    let address = if ipv6 {
        if address_hex.len() != 32 {
            return None;
        }
        let mut octets = [0_u8; 16];
        for (index, word) in address_hex.chunks_exact(8).enumerate() {
            let value = u32::try_from(parse_hex(word)?).ok()?;
            octets[index * 4..index * 4 + 4].copy_from_slice(&value.to_ne_bytes());
        }
        IpAddr::V6(Ipv6Addr::from(octets))
    } else {
        if address_hex.len() != 8 {
            return None;
        }
        let value = u32::try_from(parse_hex(address_hex)?).ok()?;
        IpAddr::V4(Ipv4Addr::from(value.to_ne_bytes()))
    };
    Some((address, port))
}

fn parse_hex(input: &[u8]) -> Option<u64> {
    if input.is_empty() {
        return None;
    }
    input.iter().try_fold(0_u64, |value, byte| {
        let digit = match byte {
            b'0'..=b'9' => u64::from(*byte - b'0'),
            b'a'..=b'f' => u64::from(*byte - b'a' + 10),
            b'A'..=b'F' => u64::from(*byte - b'A' + 10),
            _ => return None,
        };
        value.checked_mul(16)?.checked_add(digit)
    })
}

fn parse_decimal(input: &[u8]) -> Option<u64> {
    if input.is_empty() {
        return None;
    }
    input.iter().try_fold(0_u64, |value, byte| {
        if !byte.is_ascii_digit() {
            return None;
        }
        value.checked_mul(10)?.checked_add(u64::from(*byte - b'0'))
    })
}

#[cfg(test)]
#[path = "tcp_proc_tests.rs"]
mod tests;
