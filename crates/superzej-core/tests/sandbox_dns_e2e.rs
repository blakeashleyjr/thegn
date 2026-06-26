//! Suite A — DNS filter E2E (Tier 1, no external deps).
//!
//! Tests send raw UDP DNS packets to the filter proxy and verify RCODE in the
//! response. The dns_filter singleton is process-local per integration-test
//! binary, so the policy set on the first call persists for all tests.
//!
//! Policy used here: block "evil.test", allow everything else. Tests acquire a
//! module-level mutex to prevent ring-buffer contamination between concurrent
//! test threads.
//!
//! Skipped on macOS: the filter's UDP/resolver behavior differs on the darwin
//! CI runner (the macOS host port is on-device WIP, tasks.md §AX 732); the
//! suite is exercised on Linux.
#![cfg(not(target_os = "macos"))]

use std::net::UdpSocket;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use superzej_core::dns_filter::{DnsPolicy, drain_events, get_or_start};

// ── singleton initialisation ────────────────────────────────────────────────

static DNS_PORT: OnceLock<u16> = OnceLock::new();
static TEST_LOCK: Mutex<()> = Mutex::new(());

fn dns_port() -> u16 {
    *DNS_PORT.get_or_init(|| {
        get_or_start(DnsPolicy {
            block: vec!["evil.test".into()],
            allow: vec![],
            upstream: None,
        })
        .expect("dns filter start failed")
    })
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
    let _g = TEST_LOCK.lock().unwrap();
    let port = dns_port();
    drain_events(); // clear prior
    let resp = query(port, "evil.test").expect("no response from dns filter");
    assert_eq!(
        rcode(&resp),
        3,
        "expected NXDOMAIN (3), got {}",
        rcode(&resp)
    );
    let events = drain_events();
    let blocked = events.iter().find(|e| e.name == "evil.test" && !e.allowed);
    assert!(
        blocked.is_some(),
        "expected a blocked event for evil.test; got {events:?}"
    );
}

// ── A2: drain returns and clears the ring ────────────────────────────────────

#[test]
fn a2_drain_clears_ring() {
    let _g = TEST_LOCK.lock().unwrap();
    let port = dns_port();
    drain_events(); // clear prior
    // Send two queries: one blocked, one allowed (not in block-list).
    let _r1 = query(port, "evil.test");
    let _r2 = query(port, "good.test");
    // Tiny sleep to let the server thread process both packets.
    std::thread::sleep(Duration::from_millis(50));
    let first = drain_events();
    assert!(!first.is_empty(), "expected events after queries");
    let second = drain_events();
    assert!(
        second.is_empty(),
        "drain should clear the ring; got {second:?}"
    );
}

// ── A3: singleton reuse — second get_or_start returns same port ──────────────

#[test]
fn a3_singleton_reuses_port() {
    let p1 = get_or_start(DnsPolicy {
        block: vec!["a.test".into()],
        allow: vec![],
        upstream: None,
    });
    let p2 = get_or_start(DnsPolicy {
        block: vec!["b.test".into()],
        allow: vec![],
        upstream: None,
    });
    assert!(p1.is_some() && p2.is_some());
    assert_eq!(p1, p2, "second get_or_start must reuse the existing server");
}

// ── A4: multiple queries → events captured per query ────────────────────────

#[test]
fn a4_multiple_queries_all_logged() {
    let _g = TEST_LOCK.lock().unwrap();
    let port = dns_port();
    drain_events();
    // blocked
    let _ = query(port, "evil.test");
    // not-blocked
    let _ = query(port, "allowed.example");
    // not-blocked
    let _ = query(port, "other.example");
    std::thread::sleep(Duration::from_millis(80));
    let events = drain_events();
    let blocked_count = events.iter().filter(|e| !e.allowed).count();
    let allowed_count = events.iter().filter(|e| e.allowed).count();
    assert!(blocked_count >= 1, "expected at least 1 blocked event");
    assert!(allowed_count >= 2, "expected at least 2 allowed events");
}

// ── A5: non-blocked domain — response is NOT NXDOMAIN ───────────────────────
//
// Requires an outbound DNS resolver. Skipped in CI when network is unavailable.
// Run manually: cargo test --test sandbox_dns_e2e -- --include-ignored

#[test]
#[ignore]
fn a5_allowed_domain_not_nxdomain() {
    let _g = TEST_LOCK.lock().unwrap();
    let port = dns_port();
    drain_events();
    let resp = query(port, "example.com").expect("no response");
    // QR bit (byte 2 bit 7) should be set (it's a response).
    let qr_set = resp.get(2).copied().unwrap_or(0) & 0x80 != 0;
    assert!(qr_set, "expected a DNS response (QR bit set)");
    // rcode may be 0 (NOERROR) or something other than NXDOMAIN if the
    // domain exists — either way, it was forwarded (not immediately blocked).
    let rc = rcode(&resp);
    assert_ne!(
        rc, 3,
        "allowed domain should not get NXDOMAIN; got rcode={rc}"
    );
}
