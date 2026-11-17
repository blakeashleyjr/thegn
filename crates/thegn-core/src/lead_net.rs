//! The Lead's network namespace (THE-2220): `[lead] network = "isolated"`
//! wraps the Lead's bwrap argv in `pasta` so the Lead has outbound internet
//! but no host loopback, abstract sockets or X11 display; only the Lead
//! listener port is forwarded. Pure argv/text builders, plus the one impure
//! probe [`pasta_available`].
//!
//! Flags checked against `pasta(1)` from passt 2026_07_16.090d739:
//! - `--config-net`: bring up the tap, addresses and routes in the new ns
//!   (outbound internet works; the ns has its own loopback, so host loopback
//!   services and abstract unix sockets, which are per-netns, are gone);
//! - `--quiet`: no banner on the Lead's terminal;
//! - `--foreground`: pasta stays the process the pane (or the headless
//!   waiter) reaps, and exits with the spawned command;
//! - `--no-map-gw`: the gateway address is NOT remapped to the host's
//!   loopback (the default would make every host loopback port reachable
//!   at the gateway address); `--map-host-loopback` is never passed;
//! - `--map-guest-addr none`: the host's own global address is not mapped
//!   back to the host either;
//! - `-t none -u none`: nothing on the host is forwarded into the ns;
//! - `-T <port>`: the ns's loopback `<port>` is forwarded to the host's
//!   loopback `<port>`, exactly the Lead listener, so `THEGN_CONTROL_ADDR`
//!   (`127.0.0.1:<port>`) is unchanged inside;
//! - `-U none`: no UDP from the ns to host loopback;
//! - `--dns-forward <DNS_FWD>`: DNS (port 53/853) sent to [`DNS_FWD`] is
//!   redirected to the host's resolver (`--dns-host`, default: the host's
//!   first `/etc/resolv.conf` nameserver, reached from pasta in the init ns,
//!   so a host `127.0.0.53` stub still works);
//! - `--` then the bwrap argv.

/// The DNS forwarder address inside the Lead's namespace (named by the
/// generated `resolv.conf`, redirected by pasta's `--dns-forward` to the
/// host resolver). pasta documents no fixed address for `--dns-forward`; a
/// link-local address keeps it off every real network, and `169.254.1.1` is
/// the address podman's pasta integration forwards, outside pasta's own
/// local-mode defaults (`169.254.2.1`/`169.254.2.2`).
pub const DNS_FWD: &str = "169.254.1.1";

/// `pasta ... -- <inner>` for a Lead whose listener is `listener_port`.
pub fn pasta_argv(listener_port: u16, inner: &[String]) -> Vec<String> {
    let mut v: Vec<String> = [
        "pasta",
        "--config-net",
        "--quiet",
        "--foreground",
        "--no-map-gw",
        "--map-guest-addr",
        "none",
        "-t",
        "none",
        "-u",
        "none",
        "-U",
        "none",
        "-T",
    ]
    .map(String::from)
    .to_vec();
    v.push(listener_port.to_string());
    v.extend(["--dns-forward".to_string(), DNS_FWD.to_string()]);
    v.push("--".to_string());
    v.extend(inner.iter().cloned());
    v
}

/// The `resolv.conf` bound over `/etc/resolv.conf` inside the namespace: a
/// host stub resolver at `127.0.0.53` would be the ns's own loopback.
pub fn resolv_conf() -> String {
    format!("nameserver {DNS_FWD}\n")
}

/// Whether `pasta` (passt) is installed: Linux and on `PATH`. The one impure
/// function in this module; the host injects it into its launch (like
/// `lead_sandbox::bwrap_available`) so tests can force the missing branch.
pub fn pasta_available() -> bool {
    cfg!(target_os = "linux") && crate::util::which_path("pasta").is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pos(v: &[String], needle: &[&str]) -> Option<usize> {
        v.windows(needle.len())
            .position(|w| w.iter().map(String::as_str).eq(needle.iter().copied()))
    }

    #[test]
    fn pasta_argv_forwards_only_the_listener_then_runs_inner() {
        let inner = vec![
            "bwrap".to_string(),
            "--ro-bind".to_string(),
            "/".to_string(),
            "/".to_string(),
            "--".to_string(),
            "claude".to_string(),
        ];
        let v = pasta_argv(4242, &inner);
        assert_eq!(v[0], "pasta");
        let tcp_ns = pos(&v, &["-T", "4242"]).expect("listener forwarded");
        assert!(pos(&v, &["--config-net"]).is_some());
        assert!(pos(&v, &["--no-map-gw"]).is_some());
        assert!(pos(&v, &["--map-guest-addr", "none"]).is_some());
        assert!(pos(&v, &["-t", "none"]).is_some());
        assert!(pos(&v, &["-u", "none"]).is_some());
        assert!(pos(&v, &["-U", "none"]).is_some());
        assert!(pos(&v, &["--dns-forward", DNS_FWD]).is_some());
        assert!(
            !v.iter().any(|a| a == "--map-host-loopback"),
            "host loopback is never mapped into the ns"
        );
        assert!(
            !v.iter()
                .any(|a| a == "--netns" || a == "--host-lo-to-ns-lo")
        );
        // The first `--` is pasta's, and the bwrap argv follows it verbatim.
        let sep = v.iter().position(|a| a == "--").unwrap();
        assert!(tcp_ns < sep);
        assert_eq!(&v[sep + 1..], &inner[..]);
        assert_eq!(v[sep + 1], "bwrap");
    }

    #[test]
    fn resolv_conf_names_the_forwarder() {
        assert_eq!(resolv_conf(), format!("nameserver {DNS_FWD}\n"));
        let ip: std::net::Ipv4Addr = DNS_FWD.parse().unwrap();
        assert!(ip.is_link_local(), "never a routable or loopback address");
    }

    #[test]
    fn pasta_available_is_false_off_linux() {
        let available = pasta_available();
        if !cfg!(target_os = "linux") {
            assert!(!available);
        }
    }
}
