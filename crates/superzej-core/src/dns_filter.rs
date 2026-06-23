//! Per-container DNS interceptor for outbound network filtering.
//!
//! When `network_allow` or `network_block` is configured, superzej starts a
//! lightweight UDP DNS proxy on a loopback port and points per-container DNS at
//! it via `--dns 127.0.0.1:<port>`. The proxy:
//!
//! - Forwards queries for allow-listed names to the system resolver.
//! - Returns NXDOMAIN for block-listed names (block-list checked first).
//! - When the allow list is empty, allows all names not on the block-list.
//! - Logs every query in a ring-buffer that callers drain via [`drain_events`].
//!
//! The server is a lazy singleton: created the first time a sandbox needs it,
//! reused by subsequent containers in the same process.
//!
//! No tokio — this crate is substrate-agnostic; std threads + UdpSocket suffice.

use std::net::{SocketAddr, UdpSocket};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// A logged DNS query, queued in the ring-buffer.
#[derive(Debug, Clone)]
pub struct DnsEvent {
    pub name: String,
    /// `true` = forwarded to resolver, `false` = blocked (NXDOMAIN returned).
    pub allowed: bool,
}

/// Policy for the DNS filter.
#[derive(Clone, Debug)]
pub struct DnsPolicy {
    /// Allow-only these names (empty = allow all except block-listed).
    pub allow: Vec<String>,
    /// Block these names (checked first).
    pub block: Vec<String>,
}

impl DnsPolicy {
    pub fn allows(&self, name: &str) -> bool {
        let name = name.trim_end_matches('.');
        if self.block.iter().any(|b| name_matches(name, b)) {
            return false;
        }
        if self.allow.is_empty() {
            return true;
        }
        self.allow.iter().any(|a| name_matches(name, a))
    }
}

fn name_matches(name: &str, pattern: &str) -> bool {
    let pattern = pattern.trim_end_matches('.');
    if let Some(suffix) = pattern.strip_prefix("*.") {
        // Wildcard matches only strict subdomains: *.example.com matches
        // foo.example.com but NOT example.com itself.
        name.ends_with(&format!(".{suffix}"))
    } else {
        name == pattern || name.ends_with(&format!(".{pattern}"))
    }
}

struct DnsFilter {
    port: u16,
    /// Ring-buffer of recent DNS events; capped at MAX_RING entries.
    events: Arc<Mutex<Vec<DnsEvent>>>,
}

const MAX_RING: usize = 512;

static INSTANCE: OnceLock<Mutex<Option<DnsFilter>>> = OnceLock::new();

/// Start (or reuse) the DNS filter. Returns the loopback port callers should
/// pass as `--dns 127.0.0.1:<port>`. Returns `None` if binding fails.
pub fn get_or_start(policy: DnsPolicy) -> Option<u16> {
    let cell = INSTANCE.get_or_init(|| Mutex::new(None));
    let mut guard = cell.lock().ok()?;
    if let Some(f) = guard.as_ref() {
        return Some(f.port);
    }
    let sock = UdpSocket::bind("127.0.0.1:0").ok()?;
    let port = sock.local_addr().ok()?.port();
    let events: Arc<Mutex<Vec<DnsEvent>>> = Arc::new(Mutex::new(Vec::new()));
    start_server(sock, policy, Arc::clone(&events));
    *guard = Some(DnsFilter { port, events });
    Some(port)
}

/// Drain all buffered DNS events. Returns an empty vec if the filter hasn't
/// been started or the mutex is poisoned.
pub fn drain_events() -> Vec<DnsEvent> {
    let cell = match INSTANCE.get() {
        Some(c) => c,
        None => return vec![],
    };
    let guard = match cell.lock() {
        Ok(g) => g,
        Err(_) => return vec![],
    };
    match guard.as_ref() {
        None => vec![],
        Some(f) => {
            let mut ring = f.events.lock().unwrap_or_else(|e| e.into_inner());
            std::mem::take(&mut *ring)
        }
    }
}

// ---------------------------------------------------------------------------
// Internal proxy
// ---------------------------------------------------------------------------

fn start_server(sock: UdpSocket, policy: DnsPolicy, events: Arc<Mutex<Vec<DnsEvent>>>) {
    sock.set_read_timeout(Some(Duration::from_millis(500))).ok();
    std::thread::Builder::new()
        .name("dns-filter".into())
        .spawn(move || {
            let resolver = find_system_resolver();
            let mut buf = [0u8; 512];
            loop {
                let Ok((n, src)) = sock.recv_from(&mut buf) else {
                    continue;
                };
                let packet = buf[..n].to_vec();
                let name = extract_query_name(&packet).unwrap_or_default();
                let allowed = policy.allows(&name);

                // Append to ring-buffer, evict oldest if full.
                if let Ok(mut ring) = events.lock() {
                    if ring.len() >= MAX_RING {
                        ring.remove(0);
                    }
                    ring.push(DnsEvent {
                        name: name.clone(),
                        allowed,
                    });
                }

                if allowed {
                    if let Some(response) = forward_query(&packet, &resolver) {
                        let _ = sock.send_to(&response, src);
                    } else {
                        let _ = sock.send_to(&servfail(&packet), src);
                    }
                } else {
                    let _ = sock.send_to(&nxdomain(&packet), src);
                }
            }
        })
        .ok();
}

fn find_system_resolver() -> SocketAddr {
    if let Ok(content) = std::fs::read_to_string("/etc/resolv.conf") {
        for line in content.lines() {
            let line = line.trim();
            if let Some(ip) = line
                .strip_prefix("nameserver ")
                .and_then(|a| a.trim().parse::<std::net::IpAddr>().ok())
            {
                return SocketAddr::new(ip, 53);
            }
        }
    }
    "8.8.8.8:53".parse().unwrap()
}

fn forward_query(packet: &[u8], resolver: &SocketAddr) -> Option<Vec<u8>> {
    let client = UdpSocket::bind("0.0.0.0:0").ok()?;
    client.set_read_timeout(Some(Duration::from_secs(3))).ok()?;
    client.send_to(packet, resolver).ok()?;
    let mut resp = vec![0u8; 512];
    let (n, _) = client.recv_from(&mut resp).ok()?;
    resp.truncate(n);
    Some(resp)
}

fn extract_query_name(packet: &[u8]) -> Option<String> {
    if packet.len() < 13 {
        return None;
    }
    let mut pos = 12usize;
    let mut labels = Vec::new();
    loop {
        if pos >= packet.len() {
            return None;
        }
        let len = packet[pos] as usize;
        if len == 0 {
            break;
        }
        pos += 1;
        if pos + len > packet.len() {
            return None;
        }
        labels.push(String::from_utf8_lossy(&packet[pos..pos + len]).to_string());
        pos += len;
    }
    Some(labels.join("."))
}

fn nxdomain(query: &[u8]) -> Vec<u8> {
    build_response(query, 3)
}

fn servfail(query: &[u8]) -> Vec<u8> {
    build_response(query, 2)
}

fn build_response(query: &[u8], rcode: u8) -> Vec<u8> {
    if query.len() < 12 {
        return vec![];
    }
    let mut resp = query[..12].to_vec();
    resp[2] = 0x81;
    resp[3] = rcode & 0x0F;
    resp.extend_from_slice(&query[12..]);
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_allows_all_when_lists_empty() {
        let p = DnsPolicy {
            allow: vec![],
            block: vec![],
        };
        assert!(p.allows("anything.example.com"));
    }

    #[test]
    fn policy_block_list_only() {
        let p = DnsPolicy {
            allow: vec![],
            block: vec!["evil.com".into()],
        };
        assert!(p.allows("good.com"));
        assert!(!p.allows("evil.com"));
        assert!(!p.allows("sub.evil.com"));
    }

    #[test]
    fn policy_allow_list_restricts() {
        let p = DnsPolicy {
            allow: vec!["api.anthropic.com".into(), "github.com".into()],
            block: vec![],
        };
        assert!(p.allows("api.anthropic.com"));
        assert!(p.allows("sub.github.com"));
        assert!(!p.allows("evil.com"));
    }

    #[test]
    fn policy_block_beats_allow() {
        let p = DnsPolicy {
            allow: vec!["example.com".into()],
            block: vec!["example.com".into()],
        };
        assert!(!p.allows("example.com"));
    }

    #[test]
    fn wildcard_pattern() {
        let p = DnsPolicy {
            allow: vec!["*.internal.example.com".into()],
            block: vec![],
        };
        assert!(p.allows("foo.internal.example.com"));
        assert!(!p.allows("internal.example.com")); // wildcard doesn't match root
        assert!(!p.allows("external.example.com"));
    }

    #[test]
    fn extract_name_parses_simple_query() {
        let mut packet = vec![
            0x12, 0x34, 0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 7, b'e', b'x',
            b'a', b'm', b'p', b'l', b'e', 3, b'c', b'o', b'm', 0, 0x00, 0x01, 0x00, 0x01,
        ];
        assert_eq!(extract_query_name(&packet), Some("example.com".into()));
        packet.truncate(5);
        assert_eq!(extract_query_name(&packet), None);
    }

    #[test]
    fn extract_name_rejects_truncated_label() {
        // Packet claims label length 10 but only 3 bytes remain — should return None.
        let packet = vec![
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 10, b'a', b'b',
            b'c',
        ];
        assert_eq!(extract_query_name(&packet), None);
    }

    #[test]
    fn extract_name_rejects_unterminated_labels() {
        // A 12-byte header plus exactly one full label and nothing after it: the
        // loop consumes the label, advances `pos` to the end, then re-enters the
        // loop and finds `pos >= packet.len()` with no zero terminator → None.
        let packet = vec![
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 1, b'a',
        ];
        assert_eq!(extract_query_name(&packet), None);
    }

    #[test]
    fn extract_name_too_short_header() {
        // Fewer than 13 bytes → immediate None.
        assert_eq!(extract_query_name(&[0u8; 12]), None);
        assert_eq!(extract_query_name(&[]), None);
    }

    #[test]
    fn extract_name_multi_label() {
        let packet = vec![
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 3, b'f', b'o',
            b'o', 7, b'e', b'x', b'a', b'm', b'p', b'l', b'e', 3, b'c', b'o', b'm', 0,
        ];
        assert_eq!(extract_query_name(&packet), Some("foo.example.com".into()));
    }

    #[test]
    fn name_matches_trailing_dots_normalised() {
        // The pattern's trailing dot is stripped inside name_matches (the query's
        // trailing dot is normalised earlier, in DnsPolicy::allows).
        assert!(name_matches("example.com", "example.com."));
        assert!(name_matches("sub.example.com", "example.com."));
        assert!(name_matches("sub.example.com", "example.com"));
        assert!(!name_matches("notexample.com", "example.com"));
    }

    #[test]
    fn name_matches_exact_and_suffix() {
        assert!(name_matches("example.com", "example.com"));
        assert!(name_matches("a.b.example.com", "example.com"));
        // suffix must be on a label boundary
        assert!(!name_matches("fooexample.com", "example.com"));
    }

    #[test]
    fn policy_trims_trailing_dot_on_query() {
        let p = DnsPolicy {
            allow: vec!["example.com".into()],
            block: vec![],
        };
        assert!(p.allows("example.com."));
        assert!(p.allows("sub.example.com."));
    }

    #[test]
    fn wildcard_does_not_match_unrelated_suffix() {
        let p = DnsPolicy {
            allow: vec!["*.example.com".into()],
            block: vec![],
        };
        assert!(p.allows("a.example.com"));
        assert!(!p.allows("example.com"));
        assert!(!p.allows("a.other.com"));
    }

    #[test]
    fn build_response_short_query_returns_empty() {
        assert_eq!(build_response(&[0u8; 11], 3), Vec::<u8>::new());
    }

    #[test]
    fn build_response_sets_flags_and_rcode() {
        // 12-byte header + a question section.
        let query = vec![
            0x12, 0x34, 0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xAB, 0xCD,
        ];
        let resp = nxdomain(&query);
        // id preserved
        assert_eq!(&resp[..2], &[0x12, 0x34]);
        // QR + flags byte set
        assert_eq!(resp[2], 0x81);
        // rcode for NXDOMAIN
        assert_eq!(resp[3] & 0x0F, 3);
        // tail (question) carried over
        assert_eq!(&resp[12..], &[0xAB, 0xCD]);

        let sf = servfail(&query);
        assert_eq!(sf[3] & 0x0F, 2);
    }

    #[test]
    fn find_system_resolver_returns_addr() {
        // Whatever the host's /etc/resolv.conf says (or the 8.8.8.8 fallback),
        // this must return a usable address on port 53.
        let addr = find_system_resolver();
        assert_eq!(addr.port(), 53);
    }

    #[test]
    fn forward_query_round_trips_against_fake_resolver() {
        // Stand up a fake UDP "resolver" that echoes back a canned response.
        let resolver = UdpSocket::bind("127.0.0.1:0").unwrap();
        let resolver_addr = resolver.local_addr().unwrap();
        let canned: Vec<u8> = vec![0xDE, 0xAD, 0xBE, 0xEF, 0x01, 0x02];
        let canned_clone = canned.clone();
        let handle = std::thread::spawn(move || {
            let mut buf = [0u8; 512];
            let (_n, src) = resolver.recv_from(&mut buf).unwrap();
            resolver.send_to(&canned_clone, src).unwrap();
        });

        let query = vec![0x00, 0x01, 0x02, 0x03];
        let resp = forward_query(&query, &resolver_addr).expect("should get a response");
        assert_eq!(resp, canned);
        handle.join().unwrap();
    }

    #[test]
    fn forward_query_times_out_when_no_resolver() {
        // An address with nothing listening: send succeeds, recv times out → None.
        let dead = UdpSocket::bind("127.0.0.1:0").unwrap();
        let dead_addr = dead.local_addr().unwrap();
        drop(dead); // free the port so nothing answers
        let query = vec![0x00, 0x01, 0x02, 0x03];
        // recv_from on a connectionless socket with no responder times out after 3s.
        assert_eq!(forward_query(&query, &dead_addr), None);
    }

    fn build_query(id: u16, name: &str) -> Vec<u8> {
        let mut p = vec![
            (id >> 8) as u8,
            (id & 0xFF) as u8,
            0x01,
            0x00,
            0x00,
            0x01,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
        ];
        for label in name.split('.') {
            p.push(label.len() as u8);
            p.extend_from_slice(label.as_bytes());
        }
        p.push(0); // root terminator
        p.extend_from_slice(&[0x00, 0x01, 0x00, 0x01]); // QTYPE=A, QCLASS=IN
        p
    }

    #[test]
    fn server_end_to_end_singleton_and_events() {
        // Exercises get_or_start (cold + warm), the running server thread
        // (blocked + allowed branches, ring-buffer logging) and drain_events.
        let policy = DnsPolicy {
            allow: vec![],
            block: vec!["blocked.example".into()],
        };
        let port = get_or_start(policy).expect("should bind a loopback port");

        // Calling again returns the same warm port (the early-return branch).
        let port2 = get_or_start(DnsPolicy {
            allow: vec!["ignored".into()],
            block: vec![],
        })
        .expect("warm port");
        assert_eq!(port, port2);

        let client = UdpSocket::bind("127.0.0.1:0").unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let server_addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();

        // Blocked name → NXDOMAIN (rcode 3), no network needed.
        let q_blocked = build_query(0x1111, "blocked.example");
        client.send_to(&q_blocked, server_addr).unwrap();
        let mut buf = [0u8; 512];
        let (n, _) = client.recv_from(&mut buf).unwrap();
        assert!(n >= 4);
        assert_eq!(&buf[..2], &[0x11, 0x11], "id echoed back");
        assert_eq!(buf[3] & 0x0F, 3, "blocked name returns NXDOMAIN");

        // Allowed name → server forwards to the real resolver. We don't assert on
        // the payload (network may be unavailable; could be a real answer or a
        // SERVFAIL), only that the server replies and logs the event.
        let q_allowed = build_query(0x2222, "allowed.example");
        client.send_to(&q_allowed, server_addr).unwrap();
        let (n2, _) = client.recv_from(&mut buf).unwrap();
        assert!(n2 >= 4);

        // A malformed packet (too short to extract a name) is still handled: the
        // name comes back empty and the server replies rather than crashing.
        client.send_to(&[0u8; 4], server_addr).unwrap();
        let _ = client.recv_from(&mut buf);

        // Drain should surface the logged queries.
        let events = drain_events();
        let names: Vec<&str> = events.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"blocked.example"), "events: {names:?}");
        assert!(
            events
                .iter()
                .any(|e| e.name == "blocked.example" && !e.allowed),
            "blocked event marked disallowed"
        );

        // A second drain returns nothing (the ring was emptied).
        assert!(drain_events().is_empty());
    }
}
