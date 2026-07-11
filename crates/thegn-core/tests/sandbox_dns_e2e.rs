//! Suite A — DNS filter E2E (Tier 1, no external deps).
//!
//! Tests send raw UDP DNS packets to the filter proxy and verify RCODE in the
//! response. The dns_filter singleton is process-local per integration-test
//! binary, so the policy set on the first call persists for all tests.
//!
//! Policy used here: block "evil.test", allow everything else. Tests acquire a
//! module-level mutex to prevent ring-buffer contamination between concurrent
//! test threads.

use std::net::UdpSocket;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use thegn_core::dns_filter::{DnsEvent, DnsPolicy, drain_events, get_or_start};

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

// ── A3: singleton reuse — second get_or_start returns same port ──────────────

#[test]
fn a3_singleton_reuses_port() {
    let _g = test_lock();
    // Initialise the singleton through the CANONICAL policy FIRST. Otherwise a3
    // could win the init race and start the shared per-binary server with a
    // block-list that omits `evil.test` (the policy of the FIRST `get_or_start`
    // wins for the whole process), silently making a1/a4 fail — the actual
    // contention flake this test file had.
    let canonical = dns_port();
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
    assert_eq!(
        p1,
        Some(canonical),
        "get_or_start must reuse the canonical singleton, not start a new one"
    );
    assert_eq!(p1, p2, "second get_or_start must reuse the existing server");
}

// ── A4: multiple queries → events captured per query ────────────────────────

#[test]
fn a4_multiple_queries_all_logged() {
    let _g = test_lock();
    let port = dns_port();
    drain_events();
    // blocked
    let _ = query(port, "evil.test");
    // not-blocked
    let _ = query(port, "allowed.example");
    // not-blocked
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

// ── A5: non-blocked domain — response is NOT NXDOMAIN ───────────────────────
//
// Requires an outbound DNS resolver. Skipped in CI when network is unavailable.
// Run manually: cargo test --test sandbox_dns_e2e -- --include-ignored

#[test]
#[ignore]
fn a5_allowed_domain_not_nxdomain() {
    let _g = test_lock();
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
