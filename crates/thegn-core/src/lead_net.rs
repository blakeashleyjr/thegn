//! The Lead's network namespace (THE-2220): `[lead] network = "isolated"`
//! wraps the Lead's bwrap argv in `pasta` so the Lead has outbound internet
//! but no host loopback, abstract sockets or X11 display; only the Lead
//! listener port is forwarded. Pure argv/text builders.
//!
//! Spine stubs (b155 lane 1); b155 lane 8 fills them.

/// The DNS forwarder address inside the Lead's namespace (named by the
/// generated `resolv.conf`, forwarded by pasta to the host resolver).
pub const DNS_FWD: &str = "169.254.1.53";

/// `pasta ... -- <inner>` for a Lead whose listener is `listener_port`.
#[allow(dead_code, reason = "filled by b155 lane 8")]
pub fn pasta_argv(_listener_port: u16, inner: &[String]) -> Vec<String> {
    inner.to_vec()
}

/// The `resolv.conf` bound over `/etc/resolv.conf` inside the namespace.
#[allow(dead_code, reason = "filled by b155 lane 8")]
pub fn resolv_conf() -> String {
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stubs_are_inert() {
        let inner = vec!["bwrap".to_string(), "--".to_string()];
        assert_eq!(pasta_argv(4242, &inner), inner);
        assert!(resolv_conf().is_empty());
        assert!(DNS_FWD.parse::<std::net::Ipv4Addr>().is_ok());
    }
}
