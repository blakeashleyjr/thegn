use super::{
    Interface, Phy, RateInfo, Station, consume_family_batch, decode_family_reply, decode_interface,
    decode_rate, decode_station, mask_bssid,
};
use crate::collect::linux::netlink::AttributeIter;
use std::time::{Duration, Instant};

fn attr(kind: u16, payload: &[u8]) -> Vec<u8> {
    let len = 4 + payload.len();
    let mut bytes = Vec::with_capacity((len + 3) & !3);
    bytes.extend_from_slice(&(len as u16).to_ne_bytes());
    bytes.extend_from_slice(&kind.to_ne_bytes());
    bytes.extend_from_slice(payload);
    bytes.resize((bytes.len() + 3) & !3, 0);
    bytes
}

fn nested_attr(kind: u16, payload: &[u8]) -> Vec<u8> {
    attr(kind | 0x8000, payload)
}

fn genl(command: u8, payload: &[u8]) -> Vec<u8> {
    let mut bytes = vec![command, 1, 0, 0];
    bytes.extend_from_slice(payload);
    bytes
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
    bytes.resize((bytes.len() + 3) & !3, 0);
    bytes
}

#[test]
fn family_resolve_reply_fixture_extracts_the_kernel_assigned_id() {
    let mut attrs = attr(2, b"nl80211\0");
    attrs.extend_from_slice(&attr(99, b"future attribute"));
    attrs.extend_from_slice(&attr(1, &0x31_u16.to_ne_bytes()));
    assert_eq!(decode_family_reply(&genl(1, &attrs)).unwrap(), Some(0x31));
}

#[test]
fn interface_decoder_reads_fields_and_skips_unknown_attributes() {
    let mut attrs = attr(3, &7_u32.to_ne_bytes());
    attrs.extend_from_slice(&attr(4, b"wlan0\0"));
    attrs.extend_from_slice(&attr(5, &2_u32.to_ne_bytes()));
    attrs.extend_from_slice(&attr(52, b"sample"));
    attrs.extend_from_slice(&attr(1, &4_u32.to_ne_bytes()));
    attrs.extend_from_slice(&attr(38, &5180_u32.to_ne_bytes()));
    attrs.extend_from_slice(&attr(159, &3_u32.to_ne_bytes()));
    attrs.extend_from_slice(&attr(160, &5210_u32.to_ne_bytes()));
    attrs.extend_from_slice(&attr(98, &2000_i32.to_ne_bytes()));
    attrs.extend_from_slice(&attr(777, b"ignored"));
    assert_eq!(
        decode_interface(&attrs),
        Interface {
            ifindex: Some(7),
            name: Some("wlan0".to_owned()),
            iftype: Some(2),
            ssid: Some(b"sample".to_vec()),
            wiphy: Some(4),
            frequency_mhz: Some(5180),
            channel_width: Some(3),
            center_frequency1_mhz: Some(5210),
            tx_power_mbm: Some(2000),
        }
    );
}

#[test]
fn nested_station_bitrate_is_decoded_and_unknown_fields_are_ignored() {
    let mut rate = attr(5, &866_u32.to_ne_bytes());
    rate.extend_from_slice(&attr(6, &[9]));
    rate.extend_from_slice(&attr(7, &[2]));
    rate.extend_from_slice(&attr(8, &[]));
    rate.extend_from_slice(&attr(1, &54_u16.to_ne_bytes()));
    rate.extend_from_slice(&attr(777, &[1, 2]));
    let rx_rate = attr(1, &54_u16.to_ne_bytes());
    let mut info = attr(7, &[(-47_i8) as u8]);
    info.extend_from_slice(&attr(13, &[(-51_i8) as u8]));
    info.extend_from_slice(&nested_attr(8, &rate));
    info.extend_from_slice(&nested_attr(14, &rx_rate));
    info.extend_from_slice(&attr(1, &250_u32.to_ne_bytes()));
    info.extend_from_slice(&attr(11, &6_u32.to_ne_bytes()));
    info.extend_from_slice(&attr(12, &12_u32.to_ne_bytes()));
    info.extend_from_slice(&attr(16, &99_u32.to_ne_bytes()));
    info.extend_from_slice(&attr(18, &2_u32.to_ne_bytes()));
    info.extend_from_slice(&attr(27, &120_000_u32.to_ne_bytes()));
    let mut payload = attr(6, &[0x10, 0x20, 0x30, 0x40, 0x50, 0x60]);
    payload.extend_from_slice(&nested_attr(21, &info));
    payload.extend_from_slice(&attr(777, b"unknown"));
    assert_eq!(
        decode_station(&payload),
        Station {
            bssid: Some([0x10, 0x20, 0x30, 0x40, 0x50, 0x60]),
            signal_dbm: Some(-47),
            signal_avg_dbm: Some(-51),
            tx_bitrate: Some(RateInfo {
                bitrate_100kbps: Some(866),
                mcs: Some(9),
                nss: Some(2),
                short_gi: false,
                width_mhz: Some(80),
                phy: Some(Phy::Vht),
                guard_interval: None,
            }),
            rx_bitrate: Some(RateInfo {
                bitrate_100kbps: Some(54),
                width_mhz: Some(20),
                phy: Some(Phy::Legacy),
                ..RateInfo::default()
            }),
            expected_throughput_kbps: Some(120_000),
            tx_retries: Some(6),
            tx_failed: Some(12),
            beacon_loss: Some(2),
            connected_time_secs: Some(99),
            inactive_time_ms: Some(250),
        }
    );
}

#[test]
fn eht_rate_attributes_include_320_mhz_and_use_uapi_enum_values() {
    let mut rate = attr(18, &[]);
    rate.extend_from_slice(&attr(19, &[11]));
    rate.extend_from_slice(&attr(20, &[2]));
    rate.extend_from_slice(&attr(21, &[1]));
    assert_eq!(
        decode_rate(&rate),
        RateInfo {
            mcs: Some(11),
            nss: Some(2),
            width_mhz: Some(320),
            phy: Some(Phy::Eht),
            guard_interval: Some(1),
            ..RateInfo::default()
        }
    );
}

#[test]
fn truncated_values_degrade_only_the_affected_fields() {
    let mut interface = attr(3, &5_u32.to_ne_bytes());
    interface.extend_from_slice(&attr(38, &[0x34, 0x12]));
    let decoded = decode_interface(&interface);
    assert_eq!(decoded.ifindex, Some(5));
    assert_eq!(decoded.frequency_mhz, None);

    let rate = attr(5, &[0x01, 0x02]);
    assert_eq!(decode_rate(&rate).bitrate_100kbps, None);

    let mut station_info = attr(7, &[-60_i8 as u8]);
    station_info.extend_from_slice(&attr(11, &[1, 2]));
    let decoded = decode_station(&nested_attr(21, &station_info));
    assert_eq!(decoded.signal_dbm, Some(-60));
    assert_eq!(decoded.tx_retries, None);
}

#[test]
fn malformed_trailing_attribute_preserves_prior_fields() {
    let mut attrs = attr(3, &9_u32.to_ne_bytes());
    attrs.extend_from_slice(&[8, 0, 38, 0, 1, 2]);
    let decoded = decode_interface(&attrs);
    assert_eq!(decoded.ifindex, Some(9));
    assert_eq!(decoded.frequency_mhz, None);
}

#[test]
fn stale_sequence_message_is_ignored_before_current_reply() {
    let mut stale_attrs = attr(2, b"nl80211\0");
    stale_attrs.extend_from_slice(&attr(1, &0x99_u16.to_ne_bytes()));
    let mut current_attrs = attr(2, b"nl80211\0");
    current_attrs.extend_from_slice(&attr(1, &0x42_u16.to_ne_bytes()));
    let mut bytes = frame(0x10, 6, &genl(1, &stale_attrs));
    bytes.extend_from_slice(&frame(0x10, 7, &genl(1, &current_attrs)));
    assert_eq!(
        consume_family_batch(&bytes, 7).unwrap(),
        (Some(0x42), false)
    );
}

#[test]
fn malformed_trailing_family_attribute_preserves_resolved_id() {
    let mut attrs = attr(2, b"nl80211\0");
    attrs.extend_from_slice(&attr(1, &0x42_u16.to_ne_bytes()));
    attrs.extend_from_slice(&[8, 0, 99, 0, 1, 2]);
    assert_eq!(decode_family_reply(&genl(1, &attrs)).unwrap(), Some(0x42));
}

#[test]
fn unknown_attributes_are_skipped_by_checked_attribute_iterator() {
    let mut attrs = attr(65500, b"future");
    attrs.extend_from_slice(&attr(3, &11_u32.to_ne_bytes()));
    let decoded = decode_interface(&attrs);
    assert_eq!(decoded.ifindex, Some(11));
    assert_eq!(AttributeIter::new(&attrs).count(), 2);
}

#[test]
fn bssid_mask_hides_every_octet() {
    let bssid = [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff];
    let masked = mask_bssid(bssid);
    assert_eq!(masked, "xx:xx:xx:xx:xx:xx");
    for octet in bssid {
        assert!(!masked.contains(&format!("{octet:02x}")));
    }
}

#[test]
fn live_family_resolution_skips_only_expected_kernel_absence_errors() {
    use crate::collect::linux::netlink::NetlinkSocket;

    let mut socket = match NetlinkSocket::open(libc::NETLINK_GENERIC) {
        Ok(socket) => socket,
        Err(error) if is_live_skip_error(&error) => return,
        Err(error) => panic!("opening generic netlink failed unexpectedly: {error}"),
    };
    match super::resolve_family(&mut socket, Instant::now() + Duration::from_secs(2)) {
        Ok(family_id) => assert_ne!(family_id, 0),
        Err(error)
            if is_live_skip_error(&error) || error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => panic!("resolving nl80211 failed unexpectedly: {error}"),
    }
}

fn is_live_skip_error(error: &std::io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(libc::EPERM | libc::EACCES | libc::EAFNOSUPPORT | libc::EPROTONOSUPPORT)
    )
}
