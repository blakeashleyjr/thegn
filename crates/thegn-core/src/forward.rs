//! Per-worktree port forwarding — auto-detect a dev server bound *inside* a
//! worktree's sandbox and forward it to the host's loopback so it is previewable
//! at `http://localhost:<port>`.
//!
//! The *outbound-localhost* sibling of [`crate::share`]: where `[share]` exposes
//! a worktree port at a **public** URL, `[forward]` makes a sandbox-internal port
//! reachable on the **host** for browser preview. Like the share/VPN seams this
//! is pure data — the resolved [`ForwardSpec`], a free-host-port allocator, and
//! `ss`-output parsing live here and are unit-tested; the detection probe and the
//! per-placement forward mechanism live in `thegn-svc::forward`, and the host
//! sequences them off the event loop.

use crate::config::ForwardConfig;
use std::collections::BTreeSet;

impl ForwardConfig {
    /// Whether a newly-detected `container_port` should be auto-forwarded:
    /// `auto` is on, the port is not in `ignore`, and (if `only` is set) it is
    /// allow-listed. (Inherent methods may live in any module of the defining
    /// crate; kept here beside their only callers to keep `config.rs` lean.)
    pub fn should_auto_forward(&self, container_port: u16) -> bool {
        self.auto
            && !self.ignore.contains(&container_port)
            && (self.only.is_empty() || self.only.contains(&container_port))
    }

    /// Parse `range` into an inclusive `(lo, hi)`, falling back to `(8000, 8999)`
    /// on a malformed or inverted value.
    pub fn port_range(&self) -> (u16, u16) {
        let parsed = || -> Option<(u16, u16)> {
            let (lo, hi) = self.range.split_once('-')?;
            let lo: u16 = lo.trim().parse().ok()?;
            let hi: u16 = hi.trim().parse().ok()?;
            (lo != 0 && hi >= lo).then_some((lo, hi))
        };
        parsed().unwrap_or((8000, 8999))
    }
}

/// A resolved request to forward one sandbox-internal port to the host. Pure
/// data assembled by [`build_forward_spec`]; the actual host-port binding (which
/// may remap on conflict) is decided by the host via [`alloc_host_port`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForwardSpec {
    /// A short, filesystem-safe id for the worktree (see [`crate::share::label_for`]).
    pub label: String,
    /// The port bound *inside* the sandbox (container / SSH host / pod).
    pub container_port: u16,
    /// The host port we would prefer to bind — the container port itself, so a
    /// preview keeps its familiar number when free. The allocator remaps on
    /// conflict (see [`alloc_host_port`]).
    pub desired_host_port: u16,
    /// Host interface to bind the forward on (loopback by default).
    pub bind_addr: String,
}

/// Build the forward request for a `container_port` detected on the worktree
/// identified by `label`. Pure and infallible: the auto/`only`/`ignore` policy
/// is applied separately by [`ForwardConfig::should_auto_forward`] so an explicit
/// (CLI) forward can bypass it.
pub fn build_forward_spec(cfg: &ForwardConfig, label: &str, container_port: u16) -> ForwardSpec {
    ForwardSpec {
        label: label.to_string(),
        container_port,
        desired_host_port: container_port,
        bind_addr: cfg.bind.clone(),
    }
}

/// Allocate a host port for a forward: prefer `desired` (so the preview keeps the
/// dev server's own port number when it is free), otherwise the lowest free port
/// in `range` (inclusive) not already taken. **Pure** — `in_use` is supplied by
/// the caller, which bind-probes the host to populate it. Returns `None` only
/// when `desired` is taken *and* the whole range is exhausted.
pub fn alloc_host_port(desired: u16, range: (u16, u16), in_use: &BTreeSet<u16>) -> Option<u16> {
    if desired != 0 && !in_use.contains(&desired) {
        return Some(desired);
    }
    let (lo, hi) = range;
    (lo..=hi).find(|p| !in_use.contains(p))
}

/// Parse `ss -ltnH` (or `netstat -ltn`) output into the set of bound TCP ports.
/// Tolerant of an optional header line and both IPv4 (`0.0.0.0:8000`,
/// `127.0.0.1:5432`) and IPv6 (`[::]:8000`, `[::1]:8000`) local-address forms.
/// The peer column of a listening socket is always a wildcard (`*`/`:*`) and so
/// is skipped naturally — only the local address carries a numeric port.
pub fn parse_ss_listening(output: &str) -> BTreeSet<u16> {
    let mut ports = BTreeSet::new();
    for line in output.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let Some(head) = fields.first() else { continue };
        // Skip a header row (ss without -H, or netstat's banner).
        if matches!(
            head.to_ascii_lowercase().as_str(),
            "state" | "proto" | "active" | "netid"
        ) {
            continue;
        }
        // The first token carrying a numeric `:port` is the local address; the
        // queue columns have no colon and the peer column's port is `*`.
        for f in &fields {
            if let Some(p) = port_of_authority(f) {
                ports.insert(p);
                break;
            }
        }
    }
    ports
}

/// Extract the numeric port from a `host:port` authority token, tolerating
/// bracketed IPv6 hosts. Returns `None` for wildcard peers (`0.0.0.0:*`) and
/// tokens without a numeric port.
fn port_of_authority(tok: &str) -> Option<u16> {
    let (_host, port) = tok.rsplit_once(':')?;
    port.parse::<u16>().ok()
}

/// Diff two snapshots of bound ports into `(appeared, disappeared)` — the ports
/// newly bound since `prev` (to forward) and those gone (to tear down). Both
/// returned ascending.
pub fn diff_listening(prev: &BTreeSet<u16>, now: &BTreeSet<u16>) -> (Vec<u16>, Vec<u16>) {
    let appeared = now.difference(prev).copied().collect();
    let disappeared = prev.difference(now).copied().collect();
    (appeared, disappeared)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> ForwardConfig {
        ForwardConfig::default()
    }

    #[test]
    fn build_spec_prefers_container_port_and_bind() {
        let c = cfg();
        let spec = build_forward_spec(&c, "app-feat", 3000);
        assert_eq!(spec.label, "app-feat");
        assert_eq!(spec.container_port, 3000);
        assert_eq!(spec.desired_host_port, 3000);
        assert_eq!(spec.bind_addr, "127.0.0.1");
    }

    #[test]
    fn should_auto_forward_honors_auto_ignore_and_only() {
        let mut c = cfg();
        assert!(c.should_auto_forward(3000));

        // auto = false disables everything.
        c.auto = false;
        assert!(!c.should_auto_forward(3000));
        c.auto = true;

        // ignore wins.
        c.ignore = vec![5432, 22];
        assert!(!c.should_auto_forward(5432));
        assert!(c.should_auto_forward(3000));

        // a non-empty `only` is an allowlist.
        c.ignore = vec![];
        c.only = vec![3000, 8080];
        assert!(c.should_auto_forward(3000));
        assert!(!c.should_auto_forward(9000));
    }

    #[test]
    fn port_range_parses_and_falls_back() {
        let mut c = cfg();
        assert_eq!(c.port_range(), (8000, 8999));
        c.range = "9000-9100".into();
        assert_eq!(c.port_range(), (9000, 9100));
        // Malformed → default.
        c.range = "nonsense".into();
        assert_eq!(c.port_range(), (8000, 8999));
        // Inverted → default.
        c.range = "9100-9000".into();
        assert_eq!(c.port_range(), (8000, 8999));
    }

    #[test]
    fn alloc_prefers_desired_when_free() {
        let in_use = BTreeSet::new();
        assert_eq!(alloc_host_port(3000, (8000, 8999), &in_use), Some(3000));
    }

    #[test]
    fn alloc_remaps_into_range_on_conflict() {
        let in_use: BTreeSet<u16> = [3000, 8000, 8001].into_iter().collect();
        // desired 3000 is taken → first free in range is 8002.
        assert_eq!(alloc_host_port(3000, (8000, 8999), &in_use), Some(8002));
    }

    #[test]
    fn alloc_returns_none_when_range_exhausted() {
        let in_use: BTreeSet<u16> = (8000..=8002).chain(std::iter::once(3000)).collect();
        assert_eq!(alloc_host_port(3000, (8000, 8002), &in_use), None);
    }

    #[test]
    fn parse_ss_handles_ipv4_ipv6_and_wildcards() {
        // `ss -ltnH` (no header): State Recv-Q Send-Q Local:Port Peer:Port
        let out = "\
LISTEN 0      128          0.0.0.0:8000       0.0.0.0:*
LISTEN 0      128             [::]:8000          [::]:*
LISTEN 0      4096       127.0.0.1:5432       0.0.0.0:*
LISTEN 0      511              *:3000             *:*
";
        let ports = parse_ss_listening(out);
        assert_eq!(
            ports,
            [3000, 5432, 8000].into_iter().collect::<BTreeSet<_>>()
        );
    }

    #[test]
    fn parse_ss_skips_header_and_blank_lines() {
        // ss invoked without -H emits a header row; netstat emits a banner.
        let out = "\
State  Recv-Q Send-Q Local Address:Port  Peer Address:Port

LISTEN 0      128         127.0.0.1:9229      0.0.0.0:*
";
        assert_eq!(
            parse_ss_listening(out),
            std::iter::once(9229).collect::<BTreeSet<_>>()
        );
    }

    #[test]
    fn diff_reports_appeared_and_disappeared() {
        let prev: BTreeSet<u16> = [3000, 5432].into_iter().collect();
        let now: BTreeSet<u16> = [3000, 8080].into_iter().collect();
        let (appeared, disappeared) = diff_listening(&prev, &now);
        assert_eq!(appeared, vec![8080]);
        assert_eq!(disappeared, vec![5432]);
    }
}
