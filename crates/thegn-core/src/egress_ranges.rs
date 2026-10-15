//! Address policy for egress destinations.
//!
//! The proxy will call [`is_forbidden_destination`] on every resolved address.
//! This is packet/address policy, not a hostname test.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

// Conservative special-address exclusions, not an implementation of IANA's
// is-global column. The two global 192.0.0.{9,10} anycasts remain excluded.
// 168.63.129.16 is Azure's platform/WireServer VIP despite public numbering.
// Sources: IANA IPv4 Special-Purpose Address Registry; Microsoft Azure
// "What is IP address 168.63.129.16?" (reviewed 2026-09-11).
pub const SPECIAL_DENY: &[(Ipv4Addr, u8)] = &[
    (Ipv4Addr::new(0, 0, 0, 0), 8),
    (Ipv4Addr::new(10, 0, 0, 0), 8),
    (Ipv4Addr::new(100, 64, 0, 0), 10),
    (Ipv4Addr::new(127, 0, 0, 0), 8),
    (Ipv4Addr::new(168, 63, 129, 16), 32),
    (Ipv4Addr::new(169, 254, 0, 0), 16),
    (Ipv4Addr::new(172, 16, 0, 0), 12),
    (Ipv4Addr::new(192, 0, 0, 0), 24),
    (Ipv4Addr::new(192, 0, 2, 0), 24),
    (Ipv4Addr::new(192, 88, 99, 0), 24),
    (Ipv4Addr::new(192, 168, 0, 0), 16),
    (Ipv4Addr::new(198, 18, 0, 0), 15),
    (Ipv4Addr::new(198, 51, 100, 0), 24),
    (Ipv4Addr::new(203, 0, 113, 0), 24),
    (Ipv4Addr::new(224, 0, 0, 0), 4),
    (Ipv4Addr::new(240, 0, 0, 0), 4),
];

/// Shared typed IPv4 exclusions for independently reviewed address policies.
pub fn special_ipv4_ranges() -> &'static [(Ipv4Addr, u8)] {
    SPECIAL_DENY
}

pub fn is_special_ipv4(address: Ipv4Addr) -> bool {
    SPECIAL_DENY.iter().any(|&(network, prefix)| {
        let mask = u32::MAX.checked_shl(32 - u32::from(prefix)).unwrap_or(0);
        u32::from(address) & mask == u32::from(network)
    })
}

// The global-unicast envelope and explicit exclusions are based on the IANA
// IPv6 Special-Purpose Address Registry (reviewed 2026-10-15). The 3fff::/20
// documentation block is RFC 9637. Non-global entries remain listed so range
// consumers and edge tests can inspect the complete deny table.
pub const SPECIAL_DENY_V6: &[(Ipv6Addr, u8)] = &[
    (Ipv6Addr::UNSPECIFIED, 128),
    (Ipv6Addr::LOCALHOST, 128),
    (Ipv6Addr::new(0, 0, 0xffff, 0, 0, 0, 0, 0), 96),
    (Ipv6Addr::new(0x0064, 0xff9b, 0, 0, 0, 0, 0, 0), 96),
    (Ipv6Addr::new(0x0064, 0xff9b, 0x0001, 0, 0, 0, 0, 0), 48),
    (Ipv6Addr::new(0x0100, 0, 0, 0, 0, 0, 0, 0), 64),
    (Ipv6Addr::new(0x5f00, 0, 0, 0, 0, 0, 0, 0), 16),
    (Ipv6Addr::new(0x2001, 0, 0, 0, 0, 0, 0, 0), 23),
    (Ipv6Addr::new(0x2001, 0x0db8, 0, 0, 0, 0, 0, 0), 32),
    (Ipv6Addr::new(0x2002, 0, 0, 0, 0, 0, 0, 0), 16),
    (Ipv6Addr::new(0x3fff, 0, 0, 0, 0, 0, 0, 0), 20),
    (Ipv6Addr::new(0xfc00, 0, 0, 0, 0, 0, 0, 0), 7),
    (Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 0), 10),
    (Ipv6Addr::new(0xfec0, 0, 0, 0, 0, 0, 0, 0), 10),
    (Ipv6Addr::new(0xff00, 0, 0, 0, 0, 0, 0, 0), 8),
];

pub fn special_ipv6_ranges() -> &'static [(Ipv6Addr, u8)] {
    SPECIAL_DENY_V6
}

pub fn is_special_ipv6(address: Ipv6Addr) -> bool {
    let address_bits = u128::from(address);
    let global_unicast_mask = u128::MAX.checked_shl(125).unwrap_or(0);
    if address_bits & global_unicast_mask != u128::from(Ipv6Addr::new(0x2000, 0, 0, 0, 0, 0, 0, 0))
    {
        return true;
    }
    SPECIAL_DENY_V6.iter().any(|&(network, prefix)| {
        let mask = u128::MAX.checked_shl(128 - u32::from(prefix)).unwrap_or(0);
        address_bits & mask == u128::from(network)
    })
}

pub fn is_forbidden_destination(address: IpAddr) -> bool {
    match address.to_canonical() {
        IpAddr::V4(address) => is_special_ipv4(address),
        IpAddr::V6(address) => is_special_ipv6(address),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn last_v4(network: Ipv4Addr, prefix: u8) -> Ipv4Addr {
        let host_mask = u32::MAX.checked_shr(u32::from(prefix)).unwrap_or(u32::MAX);
        Ipv4Addr::from(u32::from(network) | host_mask)
    }

    fn last_v6(network: Ipv6Addr, prefix: u8) -> Ipv6Addr {
        let host_mask = u128::MAX
            .checked_shr(u32::from(prefix))
            .unwrap_or(u128::MAX);
        Ipv6Addr::from(u128::from(network) | host_mask)
    }

    #[test]
    fn every_special_v4_range_denies_both_edges() {
        for &(network, prefix) in SPECIAL_DENY {
            for address in [network, last_v4(network, prefix)] {
                assert!(is_special_ipv4(address), "{address}/{prefix}");
                assert!(is_forbidden_destination(IpAddr::V4(address)));
            }
        }
        assert_eq!(special_ipv4_ranges(), SPECIAL_DENY);
    }

    #[test]
    fn special_v6_ranges_deny_both_edges() {
        for &(network, prefix) in SPECIAL_DENY_V6 {
            for address in [network, last_v6(network, prefix)] {
                assert!(is_special_ipv6(address), "{address}/{prefix}");
                assert!(is_forbidden_destination(IpAddr::V6(address)));
            }
        }
        assert_eq!(special_ipv6_ranges(), SPECIAL_DENY_V6);
    }

    #[test]
    fn mapped_v4_is_judged_by_the_v4_table() {
        for address in ["::ffff:10.0.0.1", "::ffff:127.0.0.1"] {
            assert!(is_forbidden_destination(address.parse().unwrap()));
        }
        assert!(!is_forbidden_destination("::ffff:1.1.1.1".parse().unwrap()));
    }

    #[test]
    fn public_v4_and_v6_are_not_forbidden() {
        for address in ["1.1.1.1", "8.8.8.8"] {
            assert!(!is_forbidden_destination(address.parse().unwrap()));
        }
        for address in ["2606:4700:4700::1111", "2a00:1450::1"] {
            assert!(!is_forbidden_destination(address.parse().unwrap()));
        }
    }

    #[test]
    fn non_global_v6_is_forbidden_outside_the_table() {
        for address in ["4000::1", "e000::1"] {
            assert!(is_forbidden_destination(address.parse().unwrap()));
        }
    }
}
