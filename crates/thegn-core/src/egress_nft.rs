//! Pure rendering and limits for proxy-only egress policy.
//!
//! Rendering is not installation. Apply these rules only in a FRESH, attested
//! network namespace whose workload holds neither CAP_NET_ADMIN nor CAP_NET_RAW.
//! Never flush rulesets or target the host namespace. IPv6 is dropped until the
//! proxy carries it.

use std::fmt;
use std::net::{Ipv4Addr, SocketAddrV4};

/// The one interface name pinned for both guest networking backends: slirp4netns
/// uses positional `tap0`, and pasta must start with `--ns-ifname tap0`. No
/// string from configuration is interpolated into the ruleset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuestInterface {
    Tap0,
}

impl GuestInterface {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Tap0 => "tap0",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProxyOnlyRules {
    proxy: SocketAddrV4,
    guest_if: GuestInterface,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyRulesError {
    PortZero,
    InvalidAddress,
}

impl fmt::Display for ProxyRulesError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PortZero => f.write_str("proxy port must be nonzero"),
            Self::InvalidAddress => f.write_str(
                "proxy address must not be unspecified, broadcast, multicast or loopback",
            ),
        }
    }
}

impl ProxyOnlyRules {
    pub fn new(proxy: SocketAddrV4, guest_if: GuestInterface) -> Result<Self, ProxyRulesError> {
        if proxy.port() == 0 {
            return Err(ProxyRulesError::PortZero);
        }
        let address = *proxy.ip();
        if address.is_unspecified()
            || address == Ipv4Addr::BROADCAST
            || address.is_multicast()
            || address.is_loopback()
        {
            return Err(ProxyRulesError::InvalidAddress);
        }
        Ok(Self { proxy, guest_if })
    }

    pub fn render(&self) -> String {
        format!(
            "table inet thegn_egress {{\n\
             chain output {{ type filter hook output priority 0; policy drop;\n\
             oifname \"lo\" accept\n\
             oifname != \"{}\" drop\n\
             meta nfproto ipv6 drop\n\
             ip daddr {} tcp dport {} accept\n\
             }}\n\
             chain input {{ type filter hook input priority 0; policy drop;\n\
             iifname \"lo\" accept\n\
             iifname != \"{}\" drop\n\
             meta nfproto ipv6 drop\n\
             ct state established,related accept\n\
             }}\n\
             chain forward {{ type filter hook forward priority 0; policy drop; }}\n\
             }}\n",
            self.guest_if.name(),
            self.proxy.ip(),
            self.proxy.port(),
            self.guest_if.name(),
        )
    }
}

pub const MAX_PROXY_TIMEOUT_MS: u64 = 86400000;
pub const MAX_PROXY_CONNECTIONS: usize = 1024;
pub const MAX_PROXY_BYTES_PER_DIRECTION: u64 = 1 << 40;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EgressProxyLimits {
    pub connect_timeout_ms: u64,
    pub header_timeout_ms: u64,
    pub idle_timeout_ms: u64,
    pub max_lifetime_ms: u64,
    pub max_connections: usize,
    pub max_bytes_per_direction: u64,
}

impl Default for EgressProxyLimits {
    fn default() -> Self {
        Self {
            connect_timeout_ms: 10000,
            header_timeout_ms: 5000,
            idle_timeout_ms: 120000,
            max_lifetime_ms: 3600000,
            max_connections: 64,
            max_bytes_per_direction: 1024 * 1024 * 1024,
        }
    }
}

impl EgressProxyLimits {
    pub fn validate(&self) -> Result<(), &'static str> {
        if [
            self.connect_timeout_ms,
            self.header_timeout_ms,
            self.idle_timeout_ms,
            self.max_lifetime_ms,
        ]
        .iter()
        .any(|millis| *millis == 0 || *millis > MAX_PROXY_TIMEOUT_MS)
            || self.max_connections == 0
            || self.max_connections > MAX_PROXY_CONNECTIONS
            || self.max_bytes_per_direction == 0
            || self.max_bytes_per_direction > MAX_PROXY_BYTES_PER_DIRECTION
        {
            return Err(
                "proxy limits require timeouts 1..86400000 milliseconds, connections 1..1024 and bytes per direction 1..1099511627776",
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddrV4;

    #[test]
    fn proxy_only_render_is_deterministic_and_accepts_only_the_proxy() {
        let rules = ProxyOnlyRules::new(
            SocketAddrV4::new(Ipv4Addr::new(10, 0, 2, 2), 3128),
            GuestInterface::Tap0,
        )
        .unwrap();
        let rendered = rules.render();
        assert_eq!(rendered, rules.render());
        assert_eq!(
            rendered,
            "table inet thegn_egress {\n\
             chain output { type filter hook output priority 0; policy drop;\n\
             oifname \"lo\" accept\n\
             oifname != \"tap0\" drop\n\
             meta nfproto ipv6 drop\n\
             ip daddr 10.0.2.2 tcp dport 3128 accept\n\
             }\n\
             chain input { type filter hook input priority 0; policy drop;\n\
             iifname \"lo\" accept\n\
             iifname != \"tap0\" drop\n\
             meta nfproto ipv6 drop\n\
             ct state established,related accept\n\
             }\n\
             chain forward { type filter hook forward priority 0; policy drop; }\n\
             }\n"
        );
        let output = rendered
            .split("chain output {")
            .nth(1)
            .unwrap()
            .split("chain input {")
            .next()
            .unwrap();
        assert_eq!(output.matches(" accept").count(), 2);
        assert!(output.contains("oifname \"lo\" accept"));
        assert!(output.contains("ip daddr 10.0.2.2 tcp dport 3128 accept"));
        for forbidden in ["udp", "dport 53", "flush", "delete"] {
            assert!(!rendered.contains(forbidden));
        }
    }

    #[test]
    fn render_rejects_invalid_proxy() {
        for address in [
            Ipv4Addr::UNSPECIFIED,
            Ipv4Addr::BROADCAST,
            Ipv4Addr::new(224, 0, 0, 1),
            Ipv4Addr::LOCALHOST,
        ] {
            let error = ProxyOnlyRules::new(SocketAddrV4::new(address, 3128), GuestInterface::Tap0)
                .unwrap_err();
            assert_eq!(error, ProxyRulesError::InvalidAddress);
            assert_eq!(
                error.to_string(),
                "proxy address must not be unspecified, broadcast, multicast or loopback"
            );
        }
        let error = ProxyOnlyRules::new(
            SocketAddrV4::new(Ipv4Addr::new(10, 0, 2, 2), 0),
            GuestInterface::Tap0,
        )
        .unwrap_err();
        assert_eq!(error, ProxyRulesError::PortZero);
        assert_eq!(error.to_string(), "proxy port must be nonzero");
    }

    #[test]
    fn proxy_limits_validate_bounds() {
        EgressProxyLimits::default().validate().unwrap();
        for field in [
            "connect_timeout_ms",
            "header_timeout_ms",
            "idle_timeout_ms",
            "max_lifetime_ms",
        ] {
            let mut limits = EgressProxyLimits::default();
            match field {
                "connect_timeout_ms" => limits.connect_timeout_ms = 0,
                "header_timeout_ms" => limits.header_timeout_ms = 0,
                "idle_timeout_ms" => limits.idle_timeout_ms = 0,
                "max_lifetime_ms" => limits.max_lifetime_ms = 0,
                _ => unreachable!(),
            }
            assert!(limits.validate().is_err(), "{field} zero");
            match field {
                "connect_timeout_ms" => limits.connect_timeout_ms = MAX_PROXY_TIMEOUT_MS + 1,
                "header_timeout_ms" => limits.header_timeout_ms = MAX_PROXY_TIMEOUT_MS + 1,
                "idle_timeout_ms" => limits.idle_timeout_ms = MAX_PROXY_TIMEOUT_MS + 1,
                "max_lifetime_ms" => limits.max_lifetime_ms = MAX_PROXY_TIMEOUT_MS + 1,
                _ => unreachable!(),
            }
            assert!(limits.validate().is_err(), "{field} above maximum");
            match field {
                "connect_timeout_ms" => limits.connect_timeout_ms = MAX_PROXY_TIMEOUT_MS,
                "header_timeout_ms" => limits.header_timeout_ms = MAX_PROXY_TIMEOUT_MS,
                "idle_timeout_ms" => limits.idle_timeout_ms = MAX_PROXY_TIMEOUT_MS,
                "max_lifetime_ms" => limits.max_lifetime_ms = MAX_PROXY_TIMEOUT_MS,
                _ => unreachable!(),
            }
            limits.validate().unwrap();
        }
        let mut limits = EgressProxyLimits::default();
        limits.max_connections = 0;
        assert!(limits.validate().is_err());
        limits.max_connections = MAX_PROXY_CONNECTIONS + 1;
        assert!(limits.validate().is_err());
        limits.max_connections = MAX_PROXY_CONNECTIONS;
        limits.validate().unwrap();

        limits.max_bytes_per_direction = 0;
        assert!(limits.validate().is_err());
        limits.max_bytes_per_direction = MAX_PROXY_BYTES_PER_DIRECTION + 1;
        assert!(limits.validate().is_err());
        limits.max_bytes_per_direction = MAX_PROXY_BYTES_PER_DIRECTION;
        limits.validate().unwrap();
    }
}
