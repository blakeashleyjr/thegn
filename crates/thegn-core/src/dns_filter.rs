//! Bounded, per-policy DNS name filters for callers that point a resolver at them.
//!
//! Each distinct policy gets a lightweight UDP DNS proxy on a loopback port.
//! The proxy:
//!
//! - Forwards queries for allowed names to the configured resolver.
//! - Returns NXDOMAIN for block-listed names (block-list checked first).
//! - When the allow list is empty, allows all names not on the block-list.
//! - Logs every query in a ring-buffer that callers drain via [`drain_events`].
//!
//! This is a DNS-name filter only, not an egress boundary: direct IPs and
//! alternate resolvers bypass it. Instances are reused per policy and capped.
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
    /// Stable identifier of the policy that handled this query.
    pub policy: u64,
}

/// Policy for the DNS filter.
#[derive(Clone, Debug, Default)]
pub struct DnsPolicy {
    /// Allow-only these names (empty = allow all except block-listed).
    pub allow: Vec<String>,
    /// Block these names (checked first).
    pub block: Vec<String>,
    /// Resolver to forward allowed queries to. `None` = the system resolver
    /// (`/etc/resolv.conf`, else `8.8.8.8`). Set this to chain the filter in
    /// front of a VPN overlay's resolver (the `filter-front` DNS mode).
    pub upstream: Option<SocketAddr>,
}

impl DnsPolicy {
    pub fn allows(&self, name: &str) -> bool {
        let name = norm(name);
        if self.block.iter().any(|pattern| {
            let Some(pattern) = norm(pattern) else {
                return false;
            };
            matches_normalized(name.as_deref(), &pattern)
        }) {
            return false;
        }
        if self.allow.is_empty() {
            return true;
        }
        self.allow.iter().any(|pattern| {
            let Some(pattern) = norm(pattern) else {
                return false;
            };
            matches_normalized(name.as_deref(), &pattern)
        })
    }
}

pub(crate) fn name_matches(name: &str, pattern: &str) -> bool {
    let Some(pattern) = norm(pattern) else {
        return false;
    };
    matches_normalized(norm(name).as_deref(), &pattern)
}

fn norm(value: &str) -> Option<String> {
    let trimmed = value.trim();
    let without_dot = trimmed.strip_suffix('.').unwrap_or(trimmed);
    if without_dot.is_empty()
        || without_dot.chars().any(char::is_whitespace)
        || without_dot.split('.').any(str::is_empty)
    {
        return None;
    }
    let mut normalized = without_dot.to_string();
    normalized.make_ascii_lowercase();
    Some(normalized)
}

fn matches_normalized(name: Option<&str>, pattern: &str) -> bool {
    // Universal block: `*` matches every name. Used by the config-resolution
    // engine to encode a repo overlay's "deny-all egress" request (an empty
    // `network_allow` at the repo layer) as `network_block += ["*"]` — a
    // DNS-level deny that needs no change to `SandboxConfig`'s shape.
    if pattern == "*" {
        return true;
    }
    let Some(name) = name else {
        return false;
    };
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
const MAX_INSTANCES: usize = 16;

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
struct PolicyKey {
    allow: Vec<String>,
    block: Vec<String>,
    upstream: Option<SocketAddr>,
}

impl From<&DnsPolicy> for PolicyKey {
    fn from(policy: &DnsPolicy) -> Self {
        fn canonical_patterns(patterns: &[String]) -> Vec<String> {
            let mut result: Vec<String> = patterns
                .iter()
                .map(|pattern| {
                    norm(pattern).unwrap_or_else(|| {
                        let mut malformed = pattern.clone();
                        malformed.make_ascii_lowercase();
                        malformed
                    })
                })
                .collect();
            result.sort();
            result.dedup();
            result
        }

        Self {
            allow: canonical_patterns(&policy.allow),
            block: canonical_patterns(&policy.block),
            upstream: policy.upstream,
        }
    }
}

impl PolicyKey {
    fn stable_id(&self) -> u64 {
        let mut hash = 0xcbf29ce484222325u64;
        fn write(hash: &mut u64, bytes: &[u8]) {
            for byte in bytes {
                *hash ^= u64::from(*byte);
                *hash = hash.wrapping_mul(0x100000001b3);
            }
        }
        fn write_list(hash: &mut u64, tag: u8, values: &[String]) {
            write(hash, &[tag]);
            write(hash, &(values.len() as u64).to_be_bytes());
            for value in values {
                write(hash, &(value.len() as u64).to_be_bytes());
                write(hash, value.as_bytes());
            }
        }

        write_list(&mut hash, 0, &self.allow);
        write_list(&mut hash, 1, &self.block);
        match self.upstream {
            Some(upstream) => {
                write(&mut hash, &[1]);
                write(&mut hash, upstream.to_string().as_bytes());
            }
            None => write(&mut hash, &[0]),
        }
        hash
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Plan {
    Reuse(usize),
    Start,
    Full,
}

fn find_or_plan(keys: &[&PolicyKey], key: &PolicyKey, cap: usize) -> Plan {
    if let Some(index) = keys.iter().position(|existing| *existing == key) {
        Plan::Reuse(index)
    } else if keys.len() >= cap {
        Plan::Full
    } else {
        Plan::Start
    }
}

static INSTANCES: OnceLock<Mutex<Vec<(PolicyKey, DnsFilter)>>> = OnceLock::new();

/// Returns the loopback port of the instance for this policy.
/// Returns `None` if binding fails or the instance cap is reached.
pub fn get_or_start(policy: DnsPolicy) -> Option<u16> {
    let cell = INSTANCES.get_or_init(|| Mutex::new(Vec::new()));
    let mut guard = cell.lock().ok()?;
    let key = PolicyKey::from(&policy);
    let keys: Vec<&PolicyKey> = guard.iter().map(|(existing, _)| existing).collect();
    match find_or_plan(&keys, &key, MAX_INSTANCES) {
        Plan::Reuse(index) => return Some(guard[index].1.port),
        Plan::Full => {
            tracing::warn!(
                target: "thegn::dns",
                cap = MAX_INSTANCES,
                "DNS filter instance limit reached; reduce distinct DNS policies or restart the process"
            );
            return None;
        }
        Plan::Start => {}
    }
    let sock = UdpSocket::bind("127.0.0.1:0").ok()?;
    let port = sock.local_addr().ok()?.port();
    let events: Arc<Mutex<Vec<DnsEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let policy_id = key.stable_id();
    start_server(sock, policy, Arc::clone(&events), policy_id);
    guard.push((key, DnsFilter { port, events }));
    Some(port)
}

/// Drain all buffered DNS events. Returns an empty vec if the filter hasn't
/// been started or the mutex is poisoned.
pub fn drain_events() -> Vec<DnsEvent> {
    let cell = match INSTANCES.get() {
        Some(c) => c,
        None => return vec![],
    };
    let guard = match cell.lock() {
        Ok(g) => g,
        Err(_) => return vec![],
    };
    let mut drained = Vec::new();
    for (_, filter) in guard.iter() {
        let mut ring = filter.events.lock().unwrap_or_else(|e| e.into_inner());
        drained.append(&mut *ring);
    }
    drained
}

// ---------------------------------------------------------------------------
// Internal proxy
// ---------------------------------------------------------------------------

fn start_server(
    sock: UdpSocket,
    policy: DnsPolicy,
    events: Arc<Mutex<Vec<DnsEvent>>>,
    policy_id: u64,
) {
    sock.set_read_timeout(Some(Duration::from_millis(500))).ok(); // best-effort: timeout hint: the default blocking read is fine if the hint fails
    // best-effort: if the listener thread cannot spawn, the sandbox's DNS
    // queries fail fast (the socket drops with us) — surfaced, not silent.
    if let Err(e) = std::thread::Builder::new()
        .name("dns-filter".into())
        .spawn(move || {
            let resolver = policy.upstream.unwrap_or_else(find_system_resolver);
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
                        policy: policy_id,
                    });
                }

                if allowed {
                    if let Some(response) = forward_query(&packet, &resolver) {
                        let _ = sock.send_to(&response, src); // best-effort: a dropped UDP reply is a client retry
                    } else {
                        let _ = sock.send_to(&servfail(&packet), src); // best-effort: a dropped UDP reply is a client retry
                    }
                } else {
                    let _ = sock.send_to(&nxdomain(&packet), src); // best-effort: a dropped UDP reply is a client retry
                }
            }
        })
    {
        tracing::warn!(target: "thegn::dns", error = %e, "dns-filter listener failed to start");
    }
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
            upstream: None,
        };
        assert!(p.allows("anything.example.com"));
    }

    #[test]
    fn policy_block_list_only() {
        let p = DnsPolicy {
            allow: vec![],
            block: vec!["evil.com".into()],
            upstream: None,
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
            upstream: None,
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
            upstream: None,
        };
        assert!(!p.allows("example.com"));
    }

    #[test]
    fn wildcard_pattern() {
        let p = DnsPolicy {
            allow: vec!["*.internal.example.com".into()],
            block: vec![],
            upstream: None,
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
    fn name_matches_universal_star() {
        // `*` is the deny-all pattern the resolution engine emits.
        assert!(name_matches("example.com", "*"));
        assert!(name_matches("anything.internal", "*"));
        assert!(name_matches("a.b.c.d", "*"));
        // Blocking `*` denies every query even against an allow list.
        let p = DnsPolicy {
            allow: vec!["example.com".into()],
            block: vec!["*".into()],
            upstream: None,
        };
        assert!(!p.allows("example.com"));
        assert!(!p.allows("sub.example.com"));
    }

    #[test]
    fn block_is_case_insensitive() {
        let policy = DnsPolicy {
            allow: vec![],
            block: vec!["blocked.test".into()],
            upstream: None,
        };
        assert!(!policy.allows("BLOCKED.TEST"));
        assert!(!policy.allows("Blocked.Test."));
        assert!(!policy.allows("sub.BLOCKED.test"));
    }

    #[test]
    fn pattern_case_is_normalised() {
        let policy = DnsPolicy {
            allow: vec![],
            block: vec!["Blocked.TEST.".into()],
            upstream: None,
        };
        assert!(!policy.allows("blocked.test"));
    }

    #[test]
    fn allows_normalizes_subdomain_and_wildcard_pattern_case() {
        let suffix = DnsPolicy {
            allow: vec![],
            block: vec!["Blocked.TEST".into()],
            upstream: None,
        };
        assert!(!suffix.allows("sub.blocked.test."));
        let wildcard = DnsPolicy {
            allow: vec![],
            block: vec!["*.Example.COM.".into()],
            upstream: None,
        };
        assert!(!wildcard.allows("a.example.com"));
        let universal = DnsPolicy {
            allow: vec![],
            block: vec!["*".into()],
            upstream: None,
        };
        assert!(!universal.allows("any."));
    }

    #[test]
    fn wildcard_and_universal_case_insensitive() {
        let wildcard = DnsPolicy {
            allow: vec![],
            block: vec!["*.Example.COM".into()],
            upstream: None,
        };
        assert!(!wildcard.allows("a.example.com."));
        assert!(name_matches("A.Example.COM.", "*.example.com"));
        let universal = DnsPolicy {
            allow: vec![],
            block: vec!["*".into()],
            upstream: None,
        };
        assert!(!universal.allows("ANY."));
        assert!(name_matches("ANY.", "*"));
    }

    #[test]
    fn mixed_case_matches_exact_subdomain_and_wildcard_patterns() {
        for (name, pattern) in [
            ("BLOCKED.TEST", "blocked.test"),
            ("BLOCKED.TEST.", "blocked.test."),
            ("sub.BLOCKED.TEST", "blocked.test"),
            ("sub.BLOCKED.TEST.", "blocked.test."),
            ("a.Example.COM", "*.example.com"),
            ("a.Example.COM.", "*.example.com."),
            ("ANY", "*"),
            ("ANY.", "*."),
            ("blocked.test", "Blocked.Test"),
            ("blocked.test.", "Blocked.Test."),
            ("sub.blocked.test", "Blocked.Test"),
            ("sub.blocked.test.", "Blocked.Test."),
            ("a.example.com", "*.Example.COM"),
            ("a.example.com.", "*.Example.COM."),
            ("anything", "*"),
            ("anything.", "*."),
        ] {
            assert!(name_matches(name, pattern), "{name:?} against {pattern:?}");
        }
    }

    #[test]
    fn malformed_patterns_never_match() {
        for pattern in ["", "a..b", "a b", "."] {
            assert!(!name_matches("a.b", pattern), "pattern {pattern:?}");
            let policy = DnsPolicy {
                allow: vec![],
                block: vec![pattern.into()],
                upstream: None,
            };
            assert!(policy.allows("a.b"), "malformed block {pattern:?}");
        }
        assert!(name_matches("", "*"));
        assert!(!name_matches("a..b", "a.b"));
    }

    #[test]
    fn norm_trims_whitespace_and_one_trailing_dot() {
        assert_eq!(norm("  A.Test.  "), Some("a.test".into()));
        assert_eq!(norm("a.."), None);
        assert_eq!(norm("a\tb"), None);
    }

    #[test]
    fn policy_key_is_order_and_case_insensitive() {
        let first = DnsPolicy {
            allow: vec!["A.test".into(), "b.test.".into(), "A.test".into()],
            block: vec!["BLOCK.test".into()],
            upstream: None,
        };
        let second = DnsPolicy {
            allow: vec!["b.TEST".into(), "a.TEST.".into()],
            block: vec!["block.test.".into()],
            upstream: None,
        };
        assert_eq!(PolicyKey::from(&first), PolicyKey::from(&second));
    }

    #[test]
    fn policy_key_distinguishes_upstream() {
        let first = PolicyKey::from(&DnsPolicy {
            allow: vec![],
            block: vec![],
            upstream: Some("127.0.0.1:53".parse().unwrap()),
        });
        let second = PolicyKey::from(&DnsPolicy {
            allow: vec![],
            block: vec![],
            upstream: Some("127.0.0.1:54".parse().unwrap()),
        });
        assert_ne!(first, second);
    }

    #[test]
    fn malformed_policy_entries_keep_distinct_canonical_identity() {
        let first = PolicyKey::from(&DnsPolicy {
            allow: vec!["A..B".into()],
            block: vec![],
            upstream: None,
        });
        let second = PolicyKey::from(&DnsPolicy {
            allow: vec!["a b".into()],
            block: vec![],
            upstream: None,
        });
        assert_ne!(first, second);
        assert_eq!(first.allow, vec!["a..b"]);
    }

    #[test]
    fn find_or_plan_reuses_equal_key_starts_new_and_caps() {
        let key = PolicyKey::from(&DnsPolicy {
            allow: vec!["one.test".into()],
            block: vec![],
            upstream: None,
        });
        let other = PolicyKey::from(&DnsPolicy {
            allow: vec!["two.test".into()],
            block: vec![],
            upstream: None,
        });
        assert_eq!(find_or_plan(&[&key], &key, 1), Plan::Reuse(0));
        assert_eq!(find_or_plan(&[&key], &other, 2), Plan::Start);
        assert_eq!(find_or_plan(&[&key], &other, 1), Plan::Full);
    }

    #[test]
    fn policy_id_is_stable() {
        let key = PolicyKey::from(&DnsPolicy {
            allow: vec!["stable.test".into()],
            block: vec!["blocked.test".into()],
            upstream: Some("127.0.0.1:53".parse().unwrap()),
        });
        let first = key.stable_id();
        let second = key.stable_id();
        let other = PolicyKey::from(&DnsPolicy {
            allow: vec!["stable.test".into()],
            block: vec!["blocked.test".into()],
            upstream: Some("127.0.0.1:54".parse().unwrap()),
        });
        assert_eq!(first, second);
        assert_ne!(first, other.stable_id());
        let without_upstream = PolicyKey::from(&DnsPolicy {
            allow: vec![],
            block: vec![],
            upstream: None,
        });
        assert_ne!(without_upstream.stable_id(), first);
    }

    #[test]
    fn policy_trims_trailing_dot_on_query() {
        let p = DnsPolicy {
            allow: vec!["example.com".into()],
            block: vec![],
            upstream: None,
        };
        assert!(p.allows("example.com."));
        assert!(p.allows("sub.example.com."));
    }

    #[test]
    fn wildcard_does_not_match_unrelated_suffix() {
        let p = DnsPolicy {
            allow: vec!["*.example.com".into()],
            block: vec![],
            upstream: None,
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
    fn server_forwards_allowed_query_to_configured_upstream() {
        // A fake upstream resolver that echoes a canned response.
        let upstream = UdpSocket::bind("127.0.0.1:0").unwrap();
        let upstream_addr = upstream.local_addr().unwrap();
        let canned: Vec<u8> = vec![0xDE, 0xAD, 0xBE, 0xEF];
        let canned2 = canned.clone();
        let up = std::thread::spawn(move || {
            let mut buf = [0u8; 512];
            let (_n, src) = upstream.recv_from(&mut buf).unwrap();
            upstream.send_to(&canned2, src).unwrap();
        });

        // Start the filter pointed at our fake upstream (not the system resolver).
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let filter_port = sock.local_addr().unwrap().port();
        let policy = DnsPolicy {
            allow: vec![],
            block: vec![],
            upstream: Some(upstream_addr),
        };
        start_server(sock, policy, Arc::new(Mutex::new(Vec::new())), 0);

        // Send an allowed query to the filter; it should forward to our upstream
        // and relay the canned response back.
        let client = UdpSocket::bind("127.0.0.1:0").unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let query = build_query(0x1234, "example.com");
        client.send_to(&query, ("127.0.0.1", filter_port)).unwrap();
        let mut resp = [0u8; 512];
        let (n, _) = client.recv_from(&mut resp).unwrap();
        assert_eq!(&resp[..n], canned.as_slice());
        up.join().unwrap();
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

    fn recv_from_retry(socket: &UdpSocket, buf: &mut [u8]) -> std::io::Result<(usize, SocketAddr)> {
        loop {
            match socket.recv_from(buf) {
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                result => return result,
            }
        }
    }

    #[test]
    fn server_end_to_end_policy_reuse_and_events() {
        // Exercises get_or_start, the running server thread, and drain_events.
        let upstream = UdpSocket::bind("127.0.0.1:0").unwrap();
        upstream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let upstream_addr = upstream.local_addr().unwrap();
        let response = std::thread::spawn(move || {
            let mut query = [0u8; 512];
            let (length, source) = upstream.recv_from(&mut query)?;
            query[2] = 0x81;
            query[3] = 0x80;
            upstream.send_to(&query[..length], source)?;
            Ok::<(), std::io::Error>(())
        });
        let policy = DnsPolicy {
            allow: vec![],
            block: vec!["blocked.example".into()],
            upstream: Some(upstream_addr),
        };
        let policy_id = PolicyKey::from(&policy).stable_id();
        let port = get_or_start(policy).expect("should bind a loopback port");

        // An equal policy returns the same port.
        let port2 = get_or_start(DnsPolicy {
            allow: vec![],
            block: vec!["blocked.example".into()],
            upstream: Some(upstream_addr),
        })
        .expect("equal policy port");
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
        let (n, _) = recv_from_retry(&client, &mut buf).unwrap();
        assert!(n >= 4);
        assert_eq!(&buf[..2], &[0x11, 0x11], "id echoed back");
        assert_eq!(buf[3] & 0x0F, 3, "blocked name returns NXDOMAIN");

        // Allowed name → server forwards to the test-owned loopback resolver.
        let q_allowed = build_query(0x2222, "allowed.example");
        client.send_to(&q_allowed, server_addr).unwrap();
        let (n2, _) = recv_from_retry(&client, &mut buf).unwrap();
        assert!(n2 >= 4);
        assert_eq!(buf[3] & 0x0F, 0, "loopback upstream answers with NOERROR");
        response.join().unwrap().unwrap();

        // A malformed packet (too short to extract a name) is still handled: the
        // name comes back empty and the server replies rather than crashing.
        client.send_to(&[0u8; 4], server_addr).unwrap();
        let _ = recv_from_retry(&client, &mut buf); // best-effort: test: a dropped reply is fine; the drain below reads the ring

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
        assert!(
            events
                .iter()
                .filter(|event| event.name == "blocked.example")
                .all(|event| event.policy == policy_id),
            "events carry the handling policy id"
        );

        // A second drain returns nothing (the ring was emptied).
        assert!(drain_events().is_empty());
    }
}
