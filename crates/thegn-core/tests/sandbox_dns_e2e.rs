//! Suite A — DNS filter E2E (Tier 1, no external deps).
//!
//! Tests send raw UDP DNS packets to policy-specific filter proxies and verify
//! RCODE in the response. Allowed queries use a test-owned loopback resolver;
//! the suite never sends DNS traffic outside the process.
//!
//! Policy used here: block "evil.test", allow everything else. Tests acquire a
//! module-level mutex to prevent ring-buffer contamination between concurrent
//! test threads.

use std::net::UdpSocket;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use thegn_core::dns_filter::{DnsEvent, DnsPolicy, drain_events, get_or_start};

// ── shared fixtures ────────────────────────────────────────────────────────

static DNS_PORT: OnceLock<u16> = OnceLock::new();
static TEST_LOCK: Mutex<()> = Mutex::new(());

struct TestUpstream {
    addr: std::net::SocketAddr,
    _thread: JoinHandle<()>,
}

static UPSTREAM: OnceLock<TestUpstream> = OnceLock::new();

fn upstream_addr() -> std::net::SocketAddr {
    UPSTREAM
        .get_or_init(|| {
            let socket = UdpSocket::bind("127.0.0.1:0").expect("test upstream bind");
            socket
                .set_read_timeout(Some(Duration::from_millis(500)))
                .expect("test upstream timeout");
            let addr = socket.local_addr().expect("test upstream address");
            let thread = std::thread::spawn(move || {
                let mut packet = [0u8; 512];
                loop {
                    match socket.recv_from(&mut packet) {
                        Ok((length, source)) => {
                            if length < 12 {
                                continue;
                            }
                            packet[2] = 0x81;
                            packet[3] = 0x80;
                            if socket.send_to(&packet[..length], source).is_err() {
                                break;
                            }
                        }
                        Err(error)
                            if matches!(
                                error.kind(),
                                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                            ) => {}
                        Err(_) => break,
                    }
                }
            });
            TestUpstream {
                addr,
                _thread: thread,
            }
        })
        .addr
}

fn dns_port() -> u16 {
    *DNS_PORT.get_or_init(|| {
        get_or_start(DnsPolicy {
            block: vec!["evil.test".into()],
            allow: vec![],
            upstream: Some(upstream_addr()),
        })
        .expect("dns filter start failed")
    })
}

/// Serialise ring-buffer tests, POISON-TOLERANTLY: a test that fails an
/// assertion while holding the guard must not cascade `PoisonError` into every
/// other test (turning one real failure into four). `into_inner` recovers the
/// guard regardless of poisoning.
fn test_lock() -> MutexGuard<'static, ()> {
    TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Drain-and-accumulate DNS events until `want(&acc)` holds or `deadline`
/// elapses. `drain_events()` clears the ring, so events are MOVED into `acc`
/// across polls — when the whole test suite saturates the CPU the server thread
/// just needs a few more polls to log a packet, so this never false-fails the
/// way the old fixed 50/80ms sleeps did. Returns whatever accumulated.
fn collect_events_until(deadline: Duration, want: impl Fn(&[DnsEvent]) -> bool) -> Vec<DnsEvent> {
    let start = Instant::now();
    let mut acc: Vec<DnsEvent> = Vec::new();
    loop {
        acc.extend(drain_events());
        if want(&acc) || start.elapsed() >= deadline {
            return acc;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

// ── DNS query helpers ────────────────────────────────────────────────────────

fn make_a_query(name: &str, id: u16) -> Vec<u8> {
    let mut p = vec![
        (id >> 8) as u8,
        (id & 0xFF) as u8,
        0x01,
        0x00, // flags: QR=0, RD=1
        0x00,
        0x01, // QDCOUNT=1
        0x00,
        0x00, // ANCOUNT=0
        0x00,
        0x00, // NSCOUNT=0
        0x00,
        0x00, // ARCOUNT=0
    ];
    for label in name.split('.') {
        p.push(label.len() as u8);
        p.extend_from_slice(label.as_bytes());
    }
    p.push(0x00); // root
    p.extend_from_slice(&[0x00, 0x01, 0x00, 0x01]); // QTYPE=A, QCLASS=IN
    p
}

fn query(port: u16, name: &str) -> Option<Vec<u8>> {
    let sock = UdpSocket::bind("127.0.0.1:0").ok()?;
    sock.set_read_timeout(Some(Duration::from_secs(3))).ok()?;
    let q = make_a_query(name, 0xAB12);
    sock.send_to(&q, format!("127.0.0.1:{port}")).ok()?;
    let mut buf = [0u8; 512];
    let (n, _) = sock.recv_from(&mut buf).ok()?;
    Some(buf[..n].to_vec())
}

fn rcode(resp: &[u8]) -> u8 {
    resp.get(3).copied().unwrap_or(0) & 0x0F
}

// ── A1: blocked domain → NXDOMAIN ───────────────────────────────────────────

#[test]
fn a1_blocked_domain_returns_nxdomain() {
    let _g = test_lock();
    let port = dns_port();
    drain_events(); // clear prior
    let resp = query(port, "evil.test").expect("no response from dns filter");
    assert_eq!(
        rcode(&resp),
        3,
        "expected NXDOMAIN (3), got {}",
        rcode(&resp)
    );
    let events = collect_events_until(Duration::from_secs(5), |e| {
        e.iter().any(|x| x.name == "evil.test" && !x.allowed)
    });
    assert!(
        events.iter().any(|e| e.name == "evil.test" && !e.allowed),
        "expected a blocked event for evil.test; got {events:?}"
    );
}

// ── A2: drain returns and clears the ring ────────────────────────────────────

#[test]
fn a2_drain_clears_ring() {
    let _g = test_lock();
    let port = dns_port();
    drain_events(); // clear prior
    // Send two queries: one blocked, one allowed (not in block-list).
    let _r1 = query(port, "evil.test");
    let _r2 = query(port, "good.test");
    // Poll until BOTH query events have been recorded (the server processed
    // them) — this drains as it collects, so the ring is now empty.
    let collected = collect_events_until(Duration::from_secs(5), |e| e.len() >= 2);
    assert!(
        collected.len() >= 2,
        "expected events for both queries; got {collected:?}"
    );
    // Both queries returned synchronously and their events were collected, so
    // nothing is in flight: a fresh drain must be empty (the ring was cleared).
    let second = drain_events();
    assert!(
        second.is_empty(),
        "drain should clear the ring; got {second:?}"
    );
}

// ── A3: each distinct policy gets its own server ────────────────────────────

#[test]
fn a3_distinct_policies_get_distinct_servers() {
    let _g = test_lock();
    // Initialise the canonical policy before exercising a distinct policy.
    let canonical = dns_port();
    let policy = DnsPolicy {
        block: vec!["b.test".into()],
        allow: vec![],
        upstream: Some(upstream_addr()),
    };
    let p1 = get_or_start(policy.clone()).expect("first policy server");
    let p2 = get_or_start(policy).expect("reused policy server");
    assert_eq!(p1, p2, "the same policy must reuse its server");
    assert_ne!(
        p1, canonical,
        "a distinct policy must get a distinct server"
    );
    let response = query(p1, "b.test").expect("no response from b.test policy");
    assert_eq!(rcode(&response), 3, "the b.test policy must block b.test");
    let response = query(canonical, "b.test").expect("no response from canonical policy");
    assert_eq!(
        rcode(&response),
        0,
        "the canonical policy must allow b.test"
    );
    let response = query(p1, "evil.test").expect("no response from b.test policy");
    assert_eq!(
        rcode(&response),
        0,
        "the b.test policy must allow evil.test"
    );
    let response = query(canonical, "evil.test").expect("no response from canonical policy");
    assert_eq!(
        rcode(&response),
        3,
        "canonical policy must still block evil.test"
    );
}

// ── A6: deny-all remains effective after an allow-list policy ────────────────

#[test]
fn a6_deny_all_after_allow_list_is_honoured() {
    let _g = test_lock();
    let allow_port = get_or_start(DnsPolicy {
        allow: vec!["allowed.test".into()],
        block: vec![],
        upstream: Some(upstream_addr()),
    })
    .expect("allow-list policy server");
    let deny_port = get_or_start(DnsPolicy {
        allow: vec![],
        block: vec!["*".into()],
        upstream: Some(upstream_addr()),
    })
    .expect("deny-all policy server");
    assert_ne!(allow_port, deny_port);
    let response = query(deny_port, "allowed.test").expect("no response from deny-all policy");
    assert_eq!(rcode(&response), 3, "deny-all policy must return NXDOMAIN");
    let response = query(allow_port, "allowed.test").expect("no response from allow-list policy");
    assert_eq!(
        rcode(&response),
        0,
        "allow-list policy must allow allowed.test"
    );
}

// ── A4: multiple queries → events captured per query ────────────────────────

#[test]
fn a4_multiple_queries_all_logged() {
    let _g = test_lock();
    let port = dns_port();
    drain_events();
    // blocked
    // best-effort: test smoke: this path must not panic
    let _ = query(port, "evil.test");
    // not-blocked
    // best-effort: test smoke: this path must not panic
    let _ = query(port, "allowed.example");
    // not-blocked
    // best-effort: test smoke: this path must not panic
    let _ = query(port, "other.example");
    let events = collect_events_until(Duration::from_secs(5), |e| {
        e.iter().filter(|x| !x.allowed).count() >= 1 && e.iter().filter(|x| x.allowed).count() >= 2
    });
    let blocked_count = events.iter().filter(|e| !e.allowed).count();
    let allowed_count = events.iter().filter(|e| e.allowed).count();
    assert!(
        blocked_count >= 1,
        "expected at least 1 blocked event; got {events:?}"
    );
    assert!(
        allowed_count >= 2,
        "expected at least 2 allowed events; got {events:?}"
    );
}

// ── A5: allowed domain — response is NOT NXDOMAIN ──────────────────────────

#[test]
fn a5_allowed_domain_not_nxdomain() {
    let _g = test_lock();
    let port = dns_port();
    drain_events();
    let resp = query(port, "example.com").expect("no response");
    // QR bit (byte 2 bit 7) should be set (it's a response).
    let qr_set = resp.get(2).copied().unwrap_or(0) & 0x80 != 0;
    assert!(qr_set, "expected a DNS response (QR bit set)");
    // The test-owned loopback upstream answers with RCODE 0.
    let rc = rcode(&resp);
    assert_eq!(rc, 0, "loopback upstream should answer with NOERROR");
}
