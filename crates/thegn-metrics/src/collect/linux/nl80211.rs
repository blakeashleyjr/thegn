//! Generic-netlink codec for Linux nl80211 interface and station data.
use std::io;
use std::time::Instant;

use super::netlink::{AttributeIter, MessageIter, NetlinkMessage, NetlinkSocket};

// Generic-netlink control constants from include/uapi/linux/genetlink.h.
const GENL_ID_CTRL: u16 = 0x10;
const CTRL_CMD_GETFAMILY: u8 = 3;
const CTRL_ATTR_FAMILY_ID: u16 = 1;
const CTRL_ATTR_FAMILY_NAME: u16 = 2;

// Values below come from the nl80211_commands, nl80211_attrs, nl80211_sta_info,
// and nl80211_rate_info enums in include/uapi/linux/nl80211.h.
const CMD_GET_INTERFACE: u8 = 5;
const CMD_NEW_INTERFACE: u8 = 7;
const CMD_GET_STATION: u8 = 17;
const ATTR_WIPHY: u16 = 1;
const ATTR_IFINDEX: u16 = 3;
const ATTR_IFNAME: u16 = 4;
const ATTR_IFTYPE: u16 = 5;
const ATTR_MAC: u16 = 6;
const ATTR_WIPHY_FREQ: u16 = 38;
const ATTR_SSID: u16 = 52;
const ATTR_CHANNEL_WIDTH: u16 = 159;
const ATTR_CENTER_FREQ1: u16 = 160;
const ATTR_WIPHY_TX_POWER_LEVEL: u16 = 98;
const ATTR_STA_INFO: u16 = 21;
const STA_INFO_INACTIVE_TIME: u16 = 1;
const STA_INFO_SIGNAL: u16 = 7;
const STA_INFO_TX_BITRATE: u16 = 8;
const STA_INFO_TX_RETRIES: u16 = 11;
const STA_INFO_TX_FAILED: u16 = 12;
const STA_INFO_SIGNAL_AVG: u16 = 13;
const STA_INFO_RX_BITRATE: u16 = 14;
const STA_INFO_CONNECTED_TIME: u16 = 16;
const STA_INFO_BEACON_LOSS: u16 = 18;
const STA_INFO_EXPECTED_THROUGHPUT: u16 = 27;
const RATE_INFO_BITRATE: u16 = 1;
const RATE_INFO_MCS: u16 = 2;
const RATE_INFO_40_MHZ_WIDTH: u16 = 3;
const RATE_INFO_SHORT_GI: u16 = 4;
const RATE_INFO_BITRATE32: u16 = 5;
const RATE_INFO_VHT_MCS: u16 = 6;
const RATE_INFO_VHT_NSS: u16 = 7;
const RATE_INFO_80_MHZ_WIDTH: u16 = 8;
const RATE_INFO_80P80_MHZ_WIDTH: u16 = 9;
const RATE_INFO_160_MHZ_WIDTH: u16 = 10;
const RATE_INFO_10_MHZ_WIDTH: u16 = 11;
const RATE_INFO_5_MHZ_WIDTH: u16 = 12;
const RATE_INFO_HE_MCS: u16 = 13;
const RATE_INFO_HE_NSS: u16 = 14;
const RATE_INFO_HE_GI: u16 = 15;
const RATE_INFO_HE_DCM: u16 = 16;
const RATE_INFO_320_MHZ_WIDTH: u16 = 18;
const RATE_INFO_EHT_MCS: u16 = 19;
const RATE_INFO_EHT_NSS: u16 = 20;
const RATE_INFO_EHT_GI: u16 = 21;
const GENL_HEADER_LEN: usize = 4;
const NLA_HEADER_LEN: usize = 4;
// NLA_F_NESTED and NLA_F_NET_BYTEORDER are type flags, not attribute IDs.
const NLA_TYPE_MASK: u16 = 0x3fff;
const NLMSG_DONE: u16 = libc::NLMSG_DONE as u16;
const NLM_F_DUMP_INTR: u16 = libc::NLM_F_DUMP_INTR as u16;
const REQUEST: u16 = libc::NLM_F_REQUEST as u16;
const DUMP: u16 = (libc::NLM_F_REQUEST | libc::NLM_F_DUMP) as u16;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Interface {
    pub ifindex: Option<u32>,
    // TODO(THE-887): replace these per-dump allocations with inline buffers before
    // wiring the net_wifi collector (monitor spec §6.2).
    pub name: Option<String>,
    pub iftype: Option<u32>,
    pub ssid: Option<Vec<u8>>,
    pub wiphy: Option<u32>,
    pub frequency_mhz: Option<u32>,
    pub channel_width: Option<u32>,
    pub center_frequency1_mhz: Option<u32>,
    pub tx_power_mbm: Option<i32>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RateInfo {
    /// nl80211 uses 100 kbit/s units, including RATE_INFO_BITRATE32.
    pub bitrate_100kbps: Option<u32>,
    pub mcs: Option<u8>,
    pub nss: Option<u8>,
    pub short_gi: bool,
    pub width_mhz: Option<u16>,
    pub phy: Option<Phy>,
    pub guard_interval: Option<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phy {
    Legacy,
    Ht,
    Vht,
    He,
    Eht,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Station {
    pub bssid: Option<[u8; 6]>,
    pub signal_dbm: Option<i8>,
    pub signal_avg_dbm: Option<i8>,
    pub tx_bitrate: Option<RateInfo>,
    pub rx_bitrate: Option<RateInfo>,
    pub expected_throughput_kbps: Option<u32>,
    pub tx_retries: Option<u32>,
    pub tx_failed: Option<u32>,
    pub beacon_loss: Option<u32>,
    pub connected_time_secs: Option<u32>,
    pub inactive_time_ms: Option<u32>,
}

/// Resolves the nl80211 family ID through CTRL_CMD_GETFAMILY.
pub fn resolve_family(socket: &mut NetlinkSocket, deadline: Instant) -> io::Result<u16> {
    let sequence = socket.next_sequence();
    let mut request = Vec::with_capacity(64);
    push_header(&mut request, GENL_ID_CTRL, REQUEST, sequence);
    push_genl(&mut request, CTRL_CMD_GETFAMILY, 1);
    push_attr(&mut request, CTRL_ATTR_FAMILY_NAME, b"nl80211\0");
    set_message_len(&mut request);
    socket.send(&request)?;

    loop {
        socket.recv(deadline)?;
        let (family_id, done) = consume_family_batch(socket.received(), sequence)?;
        if let Some(family_id) = family_id {
            return Ok(family_id);
        }
        if done {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "nl80211 family not found",
            ));
        }
    }
}

/// Dumps nl80211 interfaces into `out`; existing allocation is retained.
pub fn dump_interfaces(
    socket: &mut NetlinkSocket,
    family_id: u16,
    out: &mut Vec<Interface>,
    deadline: Instant,
) -> io::Result<()> {
    out.clear();
    let sequence = socket.next_sequence();
    let mut request = Vec::with_capacity(20);
    push_header(&mut request, family_id, DUMP, sequence);
    push_genl(&mut request, CMD_GET_INTERFACE, 1);
    set_message_len(&mut request);
    socket.send(&request)?;
    receive_dump(socket, sequence, deadline, out, |message, out| {
        if message.header.kind == family_id {
            let (command, payload) = genl_parts(message.payload)?;
            if command == CMD_NEW_INTERFACE {
                out.push(decode_interface(payload));
            }
        }
        Ok(())
    })
}

/// Gets station statistics for the peer at `bssid` on the given interface.
pub fn get_station(
    socket: &mut NetlinkSocket,
    family_id: u16,
    ifindex: u32,
    bssid: [u8; 6],
    deadline: Instant,
) -> io::Result<Option<Station>> {
    let sequence = socket.next_sequence();
    let mut request = Vec::with_capacity(40);
    push_header(&mut request, family_id, REQUEST, sequence);
    push_genl(&mut request, CMD_GET_STATION, 1);
    push_attr(&mut request, ATTR_IFINDEX, &ifindex.to_ne_bytes());
    push_attr(&mut request, ATTR_MAC, &bssid);
    set_message_len(&mut request);
    socket.send(&request)?;

    loop {
        socket.recv(deadline)?;
        let mut messages = MessageIter::for_sequence(socket.received(), sequence);
        for item in &mut messages {
            let message = item?;
            if message.header.sequence != sequence {
                continue;
            }
            if message.header.kind == NLMSG_DONE {
                return Ok(None);
            }
            if message.header.kind == family_id {
                let (_, payload) = genl_parts(message.payload)?;
                let station = decode_station(payload);
                // GET_STATION is a unicast request keyed by IFINDEX and MAC. Some
                // kernels omit ATTR_MAC in the reply, so accept that reply as the
                // requested peer's station record.
                if station.bssid == Some(bssid) || station.bssid.is_none() {
                    return Ok(Some(station));
                }
            }
        }
    }
}

/// Masks every octet of a MAC/BSSID.
pub fn mask_bssid(_bssid: [u8; 6]) -> String {
    "xx:xx:xx:xx:xx:xx".to_owned()
}

fn decode_interface(payload: &[u8]) -> Interface {
    let mut row = Interface::default();
    for item in AttributeIter::new(payload) {
        let Ok(attribute) = item else { break };
        match attribute.kind & NLA_TYPE_MASK {
            ATTR_IFINDEX => row.ifindex = get_u32(attribute.payload, 0),
            ATTR_IFNAME => {
                row.name = attribute
                    .payload
                    .split(|byte| *byte == 0)
                    .next()
                    .map(|name| String::from_utf8_lossy(name).into_owned())
            }
            ATTR_IFTYPE => row.iftype = get_u32(attribute.payload, 0),
            ATTR_SSID if attribute.payload.len() <= 32 => {
                row.ssid = Some(attribute.payload.to_vec())
            }
            ATTR_WIPHY => row.wiphy = get_u32(attribute.payload, 0),
            ATTR_WIPHY_FREQ => row.frequency_mhz = get_u32(attribute.payload, 0),
            ATTR_CHANNEL_WIDTH => row.channel_width = get_u32(attribute.payload, 0),
            ATTR_CENTER_FREQ1 => row.center_frequency1_mhz = get_u32(attribute.payload, 0),
            ATTR_WIPHY_TX_POWER_LEVEL => row.tx_power_mbm = get_i32(attribute.payload, 0),
            _ => {}
        }
    }
    row
}

fn decode_station(payload: &[u8]) -> Station {
    let mut row = Station::default();
    let mut sta_info = None;
    for item in AttributeIter::new(payload) {
        let Ok(attribute) = item else { break };
        match attribute.kind & NLA_TYPE_MASK {
            ATTR_MAC => row.bssid = <[u8; 6]>::try_from(attribute.payload).ok(),
            ATTR_STA_INFO => sta_info = Some(attribute.payload),
            _ => {}
        }
    }
    if let Some(sta_info) = sta_info {
        for item in AttributeIter::new(sta_info) {
            let Ok(attribute) = item else { break };
            match attribute.kind & NLA_TYPE_MASK {
                STA_INFO_INACTIVE_TIME => row.inactive_time_ms = get_u32(attribute.payload, 0),
                STA_INFO_SIGNAL => {
                    row.signal_dbm = attribute.payload.first().copied().map(|value| value as i8)
                }
                STA_INFO_SIGNAL_AVG => {
                    row.signal_avg_dbm = attribute.payload.first().copied().map(|value| value as i8)
                }
                STA_INFO_TX_BITRATE => row.tx_bitrate = Some(decode_rate(attribute.payload)),
                STA_INFO_RX_BITRATE => row.rx_bitrate = Some(decode_rate(attribute.payload)),
                STA_INFO_EXPECTED_THROUGHPUT => {
                    row.expected_throughput_kbps = get_u32(attribute.payload, 0)
                }
                STA_INFO_TX_RETRIES => row.tx_retries = get_u32(attribute.payload, 0),
                STA_INFO_TX_FAILED => row.tx_failed = get_u32(attribute.payload, 0),
                STA_INFO_BEACON_LOSS => row.beacon_loss = get_u32(attribute.payload, 0),
                STA_INFO_CONNECTED_TIME => row.connected_time_secs = get_u32(attribute.payload, 0),
                _ => {}
            }
        }
    }
    row
}

fn decode_rate(payload: &[u8]) -> RateInfo {
    let mut row = RateInfo::default();
    let mut phy = None;
    for item in AttributeIter::new(payload) {
        let Ok(attribute) = item else { break };
        match attribute.kind & NLA_TYPE_MASK {
            RATE_INFO_BITRATE if row.bitrate_100kbps.is_none() => {
                row.bitrate_100kbps = get_u16(attribute.payload, 0).map(u32::from)
            }
            RATE_INFO_BITRATE32 => row.bitrate_100kbps = get_u32(attribute.payload, 0),
            RATE_INFO_MCS => {
                row.mcs = attribute.payload.first().copied();
                phy = Some(Phy::Ht);
            }
            RATE_INFO_VHT_MCS => {
                row.mcs = attribute.payload.first().copied();
                phy = Some(Phy::Vht);
            }
            RATE_INFO_VHT_NSS => {
                row.nss = attribute.payload.first().copied();
                phy = Some(Phy::Vht);
            }
            RATE_INFO_HE_MCS => {
                row.mcs = attribute.payload.first().copied();
                phy = Some(Phy::He);
            }
            RATE_INFO_HE_NSS => {
                row.nss = attribute.payload.first().copied();
                phy = Some(Phy::He);
            }
            RATE_INFO_HE_GI => {
                row.guard_interval = attribute.payload.first().copied();
                phy = Some(Phy::He);
            }
            RATE_INFO_HE_DCM => {
                phy = Some(Phy::He);
            }
            RATE_INFO_EHT_MCS => {
                row.mcs = attribute.payload.first().copied();
                phy = Some(Phy::Eht);
            }
            RATE_INFO_EHT_NSS => {
                row.nss = attribute.payload.first().copied();
                phy = Some(Phy::Eht);
            }
            RATE_INFO_EHT_GI => {
                row.guard_interval = attribute.payload.first().copied();
                phy = Some(Phy::Eht);
            }
            RATE_INFO_SHORT_GI => row.short_gi = true,
            RATE_INFO_40_MHZ_WIDTH => row.width_mhz = Some(40),
            RATE_INFO_80_MHZ_WIDTH => row.width_mhz = Some(80),
            RATE_INFO_80P80_MHZ_WIDTH => row.width_mhz = Some(160),
            RATE_INFO_160_MHZ_WIDTH => row.width_mhz = Some(160),
            RATE_INFO_320_MHZ_WIDTH => row.width_mhz = Some(320),
            RATE_INFO_10_MHZ_WIDTH => row.width_mhz = Some(10),
            RATE_INFO_5_MHZ_WIDTH => row.width_mhz = Some(5),
            _ => {}
        }
    }
    if row.mcs.is_none() && phy.is_none() {
        phy = Some(Phy::Legacy);
    }
    if row.width_mhz.is_none() && row.bitrate_100kbps.is_some() {
        row.width_mhz = Some(20);
    }
    row.phy = phy;
    row
}

fn receive_dump<T>(
    socket: &mut NetlinkSocket,
    sequence: u32,
    deadline: Instant,
    out: &mut Vec<T>,
    mut decode: impl FnMut(&NetlinkMessage<'_>, &mut Vec<T>) -> io::Result<()>,
) -> io::Result<()> {
    loop {
        socket.recv(deadline)?;
        let mut messages = MessageIter::for_sequence(socket.received(), sequence);
        for item in &mut messages {
            let message = item?;
            if message.header.sequence != sequence {
                continue;
            }
            if message.header.kind == NLMSG_DONE {
                if message.header.flags & NLM_F_DUMP_INTR != 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::Interrupted,
                        "nl80211 dump interrupted",
                    ));
                }
                if let Some(error) = get_i32(message.payload, 0).filter(|error| *error < 0) {
                    return Err(io::Error::from_raw_os_error(error.saturating_neg()));
                }
                return Ok(());
            }
            decode(&message, out)?;
        }
    }
}

fn decode_family_reply(payload: &[u8]) -> io::Result<Option<u16>> {
    let attrs = genl_payload(payload)?;
    let mut family_id = None;
    let mut family_name = None;
    for item in AttributeIter::new(attrs) {
        let Ok(attribute) = item else { break };
        match attribute.kind & NLA_TYPE_MASK {
            CTRL_ATTR_FAMILY_ID => family_id = get_u16(attribute.payload, 0),
            CTRL_ATTR_FAMILY_NAME => {
                family_name = attribute
                    .payload
                    .split(|byte| *byte == 0)
                    .next()
                    .map(|name| name == b"nl80211");
            }
            _ => {}
        }
    }
    match (family_id, family_name) {
        (Some(id), Some(true)) => Ok(Some(id)),
        (Some(_), Some(false)) => Err(invalid("generic-netlink resolved the wrong family")),
        (None, _) => Err(invalid("truncated CTRL_ATTR_FAMILY_ID")),
        (Some(_), None) => Ok(None),
    }
}

fn consume_family_batch(bytes: &[u8], sequence: u32) -> io::Result<(Option<u16>, bool)> {
    let mut messages = MessageIter::for_sequence(bytes, sequence);
    for item in &mut messages {
        let message = item?;
        if message.header.sequence != sequence {
            continue;
        }
        if message.header.kind == GENL_ID_CTRL
            && let Some(family_id) = decode_family_reply(message.payload)?
        {
            return Ok((Some(family_id), false));
        }
    }
    Ok((None, messages.is_done()))
}

fn genl_payload(payload: &[u8]) -> io::Result<&[u8]> {
    payload
        .get(GENL_HEADER_LEN..)
        .ok_or_else(|| invalid("truncated generic-netlink header"))
}

fn genl_parts(payload: &[u8]) -> io::Result<(u8, &[u8])> {
    if payload.len() < GENL_HEADER_LEN {
        return Err(invalid("truncated generic-netlink header"));
    }
    Ok((payload[0], &payload[GENL_HEADER_LEN..]))
}

fn push_header(bytes: &mut Vec<u8>, kind: u16, flags: u16, sequence: u32) {
    bytes.extend_from_slice(&0_u32.to_ne_bytes());
    bytes.extend_from_slice(&kind.to_ne_bytes());
    bytes.extend_from_slice(&flags.to_ne_bytes());
    bytes.extend_from_slice(&sequence.to_ne_bytes());
    bytes.extend_from_slice(&0_u32.to_ne_bytes());
}

fn push_genl(bytes: &mut Vec<u8>, command: u8, version: u8) {
    bytes.extend_from_slice(&[command, version, 0, 0]);
}

fn push_attr(bytes: &mut Vec<u8>, kind: u16, payload: &[u8]) {
    let len = NLA_HEADER_LEN + payload.len();
    bytes.extend_from_slice(&(len as u16).to_ne_bytes());
    bytes.extend_from_slice(&kind.to_ne_bytes());
    bytes.extend_from_slice(payload);
    while bytes.len() % 4 != 0 {
        bytes.push(0);
    }
}

fn set_message_len(bytes: &mut [u8]) {
    let len = bytes.len() as u32;
    bytes[..4].copy_from_slice(&len.to_ne_bytes());
}

fn get_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_ne_bytes(
        bytes.get(offset..offset.checked_add(2)?)?.try_into().ok()?,
    ))
}

fn get_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_ne_bytes(
        bytes.get(offset..offset.checked_add(4)?)?.try_into().ok()?,
    ))
}

fn get_i32(bytes: &[u8], offset: usize) -> Option<i32> {
    Some(i32::from_ne_bytes(
        bytes.get(offset..offset.checked_add(4)?)?.try_into().ok()?,
    ))
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
#[path = "nl80211_tests.rs"]
mod tests;
