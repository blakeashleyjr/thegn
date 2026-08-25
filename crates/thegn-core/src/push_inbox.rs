//! The push command inbox — pure envelope authentication + admission.
//!
//! A phone publishes signed command envelopes to an ntfy topic; the daemon
//! subscribes and feeds each raw message through [`evaluate`], which is the
//! whole security decision as a **pure function**: parse → version → HMAC →
//! freshness → replay → admission (`allow ∩ scopes ∩ unconditional-admin-deny`).
//! Only an [`Outcome::Accepted`] reaches the capability-catalog dispatch; every
//! rejection is classified ([`RejectReason`]) for counters and logs and touches
//! nothing.
//!
//! This module is substrate-free (a hasher is not a runtime) and lives under
//! the 95%-line coverage gate — the inbound command surface's authenticator
//! must be exhaustively tested, so all of it is here, none of it in the daemon.
//!
//! **The envelope contract** (what a phone client must produce):
//!
//! ```text
//! { "v": 1, "id": "<unique>", "ts": <unix secs>, "cap": "<catalog id>",
//!   "params": { … }, "mac": "<hex hmac-sha256>" }
//! ```
//!
//! The MAC is `HMAC-SHA256(inbox_secret, canonical)` where `canonical` is
//! [`canonical_signing_string`]: the five fields joined by `\n`, with `params`
//! serialized as **compact, sorted-key JSON** (see [`canonical_signing_string`]
//! for the exact bytes). A client that serializes params any other way must
//! still sign over this canonical form.

use serde::Deserialize;
use serde_json::{Value, json};

use crate::control::{Scope, ScopeSet};

/// The only accepted envelope version. A future breaking change bumps this and
/// old-version messages are rejected ([`RejectReason::UnsupportedVersion`]).
pub const PROTO_VERSION: u32 = 1;

/// Default freshness half-window: a message's `ts` must be within this many
/// seconds of now (past OR future, to tolerate clock skew both ways).
pub const FRESHNESS_SECS: i64 = 300;

/// Hard cap on a published reply's size — a listing can never exfiltrate
/// unbounded state back through the reply topic.
pub const REPLY_CAP_BYTES: usize = 8 * 1024;

type HmacSha256 = hmac::Hmac<sha2::Sha256>;

/// A parsed command envelope. `mac` authenticates the other five fields.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Envelope {
    pub v: u32,
    pub id: String,
    pub ts: i64,
    pub cap: String,
    #[serde(default)]
    pub params: Value,
    pub mac: String,
}

/// The exact bytes the MAC is computed over: `v\nid\nts\ncap\n<params>` where
/// `<params>` is `serde_json`'s compact serialization of the params value.
/// `serde_json::Value` maps are `BTreeMap`-backed (no `preserve_order`), so the
/// output has **sorted keys and no insignificant whitespace** — canonical and
/// reproducible on any client that serializes the same way.
pub fn canonical_signing_string(v: u32, id: &str, ts: i64, cap: &str, params: &Value) -> String {
    let params = serde_json::to_string(params).unwrap_or_else(|_| "null".into());
    format!("{v}\n{id}\n{ts}\n{cap}\n{params}")
}

/// The hex HMAC-SHA256 of the canonical form under `secret`.
pub fn sign(secret: &[u8], v: u32, id: &str, ts: i64, cap: &str, params: &Value) -> String {
    use hmac::Mac;
    let mut mac = HmacSha256::new_from_slice(secret).expect("hmac accepts any key length");
    mac.update(canonical_signing_string(v, id, ts, cap, params).as_bytes());
    hex(&mac.finalize().into_bytes())
}

/// Verify an envelope's MAC against `secret` in constant time.
pub fn verify_mac(secret: &[u8], env: &Envelope) -> bool {
    let expect = sign(secret, env.v, &env.id, env.ts, &env.cap, &env.params);
    ct_eq(&expect, env.mac.trim())
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Constant-time string compare (both sides are fixed-length hex, so a length
/// difference leaks nothing and is a fast reject).
fn ct_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes()
        .zip(b.bytes())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

/// Why a message did not execute. Every non-accept path carries one, so
/// counters and logs name the cause without ever revealing envelope contents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RejectReason {
    /// The payload was not a valid command envelope.
    BadJson,
    /// `v` is not [`PROTO_VERSION`].
    UnsupportedVersion,
    /// The MAC did not verify against the inbox secret.
    BadMac,
    /// `ts` is outside the freshness window.
    Stale,
    /// `id` was already seen within the window.
    Replay,
    /// `cap` is not a catalog capability.
    UnknownCap,
    /// `cap` is not in the configured allow list.
    NotAllowed,
    /// `cap` is admin-scoped — refused unconditionally, regardless of config.
    AdminDenied,
    /// `cap`'s required scope is outside the configured ceiling.
    ScopeExceeded,
}

impl RejectReason {
    pub fn as_str(self) -> &'static str {
        match self {
            RejectReason::BadJson => "bad_json",
            RejectReason::UnsupportedVersion => "unsupported_version",
            RejectReason::BadMac => "bad_mac",
            RejectReason::Stale => "stale",
            RejectReason::Replay => "replay",
            RejectReason::UnknownCap => "unknown_cap",
            RejectReason::NotAllowed => "not_allowed",
            RejectReason::AdminDenied => "admin_denied",
            RejectReason::ScopeExceeded => "scope_exceeded",
        }
    }

    /// Every reason, for exhaustive counter initialisation / tests.
    pub const ALL: &'static [RejectReason] = &[
        RejectReason::BadJson,
        RejectReason::UnsupportedVersion,
        RejectReason::BadMac,
        RejectReason::Stale,
        RejectReason::Replay,
        RejectReason::UnknownCap,
        RejectReason::NotAllowed,
        RejectReason::AdminDenied,
        RejectReason::ScopeExceeded,
    ];
}

/// The admission decision for a capability id (the AuthZ half — the MAC is the
/// AuthN half). `allow ∩ required_scope ceiling ∩ unconditional-admin-deny`,
/// enforced against the ONE catalog policy table (`capability::scope_of`), never
/// a second table.
pub fn admit(
    cap: &str,
    allow: &std::collections::BTreeSet<String>,
    ceiling: ScopeSet,
) -> Result<Scope, RejectReason> {
    let Some(row) = crate::capability::lookup(cap) else {
        return Err(RejectReason::UnknownCap);
    };
    let need = crate::capability::scope_of(row);
    // Admin-deny is unconditional: an admin capability is refused even if it is
    // allowlisted and even if the ceiling includes admin.
    if need == Scope::Admin {
        return Err(RejectReason::AdminDenied);
    }
    if !allow.contains(cap) {
        return Err(RejectReason::NotAllowed);
    }
    if !ceiling.allows(need) {
        return Err(RejectReason::ScopeExceeded);
    }
    Ok(need)
}

/// A message that passed every gate: exactly one catalog capability to invoke.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Accepted {
    pub id: String,
    pub cap: String,
    pub params: Value,
    /// The capability's required scope (within the ceiling by construction).
    pub scope: Scope,
}

/// The verdict for one raw inbox message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Accepted(Accepted),
    Rejected(RejectReason),
}

/// A bounded, freshness-windowed replay cache. In-memory only (a daemon restart
/// shrinks the window, which is fine — freshness still bounds replay). Not
/// thread-safe by itself; the daemon holds it behind its single inbox task.
#[derive(Debug)]
pub struct ReplayGuard {
    window_secs: i64,
    capacity: usize,
    /// (id, ts) in insertion order, pruned by age and capacity.
    seen: std::collections::VecDeque<(String, i64)>,
    ids: std::collections::HashSet<String>,
}

/// The freshness+replay verdict for one id/ts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    Fresh,
    Stale,
    Replay,
}

impl ReplayGuard {
    pub fn new(window_secs: i64, capacity: usize) -> Self {
        ReplayGuard {
            window_secs: window_secs.max(1),
            capacity: capacity.max(1),
            seen: std::collections::VecDeque::new(),
            ids: std::collections::HashSet::new(),
        }
    }

    /// The default guard: [`FRESHNESS_SECS`] window, sized to hold a busy
    /// minute's worth of ids comfortably.
    pub fn default_guard() -> Self {
        ReplayGuard::new(FRESHNESS_SECS, 4096)
    }

    fn prune(&mut self, now: i64) {
        let cutoff = now - self.window_secs;
        while let Some((_, ts)) = self.seen.front() {
            if *ts < cutoff {
                if let Some((id, _)) = self.seen.pop_front() {
                    self.ids.remove(&id);
                }
            } else {
                break;
            }
        }
        while self.seen.len() >= self.capacity {
            if let Some((id, _)) = self.seen.pop_front() {
                self.ids.remove(&id);
            } else {
                break;
            }
        }
    }

    /// Admit an id observed at `ts`, evaluated at `now`. A fresh, unseen id is
    /// recorded and returns [`Freshness::Fresh`]; a stale ts is rejected
    /// **without** recording (so an old id can't consume a cache slot); a seen
    /// id returns [`Freshness::Replay`].
    pub fn admit(&mut self, id: &str, ts: i64, now: i64) -> Freshness {
        // `ts` is attacker-controlled (it comes off the envelope), so a plain
        // `now - ts` could overflow. Saturate, then take the unsigned distance.
        if now.saturating_sub(ts).unsigned_abs() > self.window_secs as u64 {
            return Freshness::Stale;
        }
        self.prune(now);
        if self.ids.contains(id) {
            return Freshness::Replay;
        }
        self.ids.insert(id.to_string());
        self.seen.push_back((id.to_string(), ts));
        Freshness::Fresh
    }
}

/// The whole security decision for one raw inbox message, as a pure function.
///
/// Order is security-critical: the MAC is verified **before** the replay cache
/// is touched, so an unauthenticated message can never poison the cache with a
/// chosen id, and freshness/replay run before admission so a stale/duplicate is
/// dropped regardless of what it asked for.
pub fn evaluate(
    raw: &str,
    secret: &[u8],
    allow: &std::collections::BTreeSet<String>,
    ceiling: ScopeSet,
    now: i64,
    replay: &mut ReplayGuard,
) -> Outcome {
    let env: Envelope = match serde_json::from_str(raw) {
        Ok(e) => e,
        Err(_) => return Outcome::Rejected(RejectReason::BadJson),
    };
    if env.v != PROTO_VERSION {
        return Outcome::Rejected(RejectReason::UnsupportedVersion);
    }
    if !verify_mac(secret, &env) {
        return Outcome::Rejected(RejectReason::BadMac);
    }
    match replay.admit(&env.id, env.ts, now) {
        Freshness::Stale => return Outcome::Rejected(RejectReason::Stale),
        Freshness::Replay => return Outcome::Rejected(RejectReason::Replay),
        Freshness::Fresh => {}
    }
    match admit(&env.cap, allow, ceiling) {
        Ok(scope) => Outcome::Accepted(Accepted {
            id: env.id,
            cap: env.cap,
            params: env.params,
            scope,
        }),
        Err(reason) => Outcome::Rejected(reason),
    }
}

/// Running counts of inbox outcomes, for `doctor`/log visibility. Pure; the
/// daemon owns one and calls [`Self::record`] per message.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Counters {
    pub accepted: u64,
    /// Accepted-but-not-executed: dropped by the per-minute execution cap.
    pub rate_limited: u64,
    /// Per-[`RejectReason`] counts, keyed by [`RejectReason::as_str`].
    pub rejected: std::collections::BTreeMap<&'static str, u64>,
}

impl Counters {
    pub fn record(&mut self, outcome: &Outcome) {
        match outcome {
            Outcome::Accepted(_) => self.accepted += 1,
            Outcome::Rejected(r) => *self.rejected.entry(r.as_str()).or_insert(0) += 1,
        }
    }

    /// Total rejections across all reasons.
    pub fn rejected_total(&self) -> u64 {
        self.rejected.values().sum()
    }
}

/// A fixed-window per-minute (or per-N-second) execution cap. Bounds how many
/// commands the inbox will actually run, so a stolen topic+secret can't drive an
/// unbounded command rate. Pure; the daemon holds one behind its inbox task.
#[derive(Debug)]
pub struct RateLimiter {
    max_per_window: u32,
    window_secs: i64,
    window_start: i64,
    count: u32,
}

impl RateLimiter {
    pub fn new(max_per_window: u32, window_secs: i64) -> Self {
        RateLimiter {
            max_per_window,
            window_secs: window_secs.max(1),
            window_start: i64::MIN,
            count: 0,
        }
    }

    /// The default: 30 executions per minute.
    pub fn per_minute(max: u32) -> Self {
        RateLimiter::new(max, 60)
    }

    /// Whether one more execution is allowed at `now`, consuming a slot if so.
    pub fn allow(&mut self, now: i64) -> bool {
        // saturating: the initial `window_start` is `i64::MIN` (force a reset on
        // the first call), and `now - i64::MIN` would overflow.
        if now.saturating_sub(self.window_start) >= self.window_secs {
            self.window_start = now;
            self.count = 0;
        }
        if self.count < self.max_per_window {
            self.count += 1;
            true
        } else {
            false
        }
    }
}

/// Build the JSON reply for one command result, bounded to [`REPLY_CAP_BYTES`]
/// so a listing can never exfiltrate unbounded state. `result` is the
/// capability's payload (`Ok`) or an error message (`Err`).
pub fn build_reply(id: &str, result: Result<&Value, &str>, cap_bytes: usize) -> String {
    // Reserve headroom for the JSON envelope keys/quotes/id so the assembled
    // reply lands under `cap_bytes` without needing the last-resort fallback.
    let inner_cap = cap_bytes.saturating_sub(64 + id.len());
    let body = match result {
        Ok(v) => json!({ "id": id, "ok": true, "result": clamp_value(v, inner_cap) }),
        Err(e) => json!({ "id": id, "ok": false, "error": clamp_str(e, inner_cap) }),
    };
    let s = body.to_string();
    if s.len() <= cap_bytes {
        s
    } else {
        // Last resort (e.g. a tiny cap): drop the payload but preserve ok.
        json!({ "id": id, "ok": result.is_ok(), "truncated": true }).to_string()
    }
}

/// A value whose compact form fits `cap`, else a truncated marker carrying a
/// bounded preview — the reply never grows without limit.
fn clamp_value(v: &Value, cap: usize) -> Value {
    let s = v.to_string();
    if s.len() <= cap {
        return v.clone();
    }
    let preview_cap = cap.min(1024);
    json!({
        "truncated": true,
        "bytes": s.len(),
        "preview": clamp_str(&s, preview_cap),
    })
}

/// Clip a string to at most `cap` bytes on a char boundary.
fn clamp_str(s: &str, cap: usize) -> String {
    if s.len() <= cap {
        return s.to_string();
    }
    let mut end = cap;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &[u8] = b"correct horse battery staple";

    fn allow(caps: &[&str]) -> std::collections::BTreeSet<String> {
        caps.iter().map(|s| s.to_string()).collect()
    }

    /// A well-formed, signed envelope JSON for `cap`/`params` at time `ts`.
    fn signed(id: &str, ts: i64, cap: &str, params: Value) -> String {
        let mac = sign(SECRET, PROTO_VERSION, id, ts, cap, &params);
        json!({ "v": PROTO_VERSION, "id": id, "ts": ts, "cap": cap, "params": params, "mac": mac })
            .to_string()
    }

    #[test]
    fn canonical_is_sorted_and_compact_regardless_of_input_key_order() {
        let a: Value = serde_json::from_str(r#"{"b":2,"a":1}"#).unwrap();
        let b: Value = serde_json::from_str(r#"{"a":1,"b":2}"#).unwrap();
        assert_eq!(
            canonical_signing_string(1, "x", 10, "c", &a),
            canonical_signing_string(1, "x", 10, "c", &b),
            "key order must not change the signature"
        );
        assert!(canonical_signing_string(1, "x", 10, "c", &a).ends_with(r#"{"a":1,"b":2}"#));
    }

    #[test]
    fn happy_path_accepts_and_carries_scope() {
        let mut g = ReplayGuard::default_guard();
        let raw = signed("m1", 1000, "worktrees.list", json!({}));
        let out = evaluate(
            &raw,
            SECRET,
            &allow(&["worktrees.list"]),
            ScopeSet::parse("read"),
            1000,
            &mut g,
        );
        match out {
            Outcome::Accepted(a) => {
                assert_eq!(a.cap, "worktrees.list");
                assert_eq!(a.scope, Scope::Read);
                assert_eq!(a.id, "m1");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn tampered_mac_is_rejected() {
        let mut g = ReplayGuard::default_guard();
        let mut raw = signed("m1", 1000, "worktrees.list", json!({}));
        // Flip a byte in the payload after signing.
        raw = raw.replace("worktrees.list", "worktrees.open");
        let out = evaluate(
            &raw,
            SECRET,
            &allow(&["worktrees.open"]),
            ScopeSet::parse("write"),
            1000,
            &mut g,
        );
        assert_eq!(out, Outcome::Rejected(RejectReason::BadMac));
    }

    #[test]
    fn wrong_secret_is_rejected() {
        let mut g = ReplayGuard::default_guard();
        let raw = signed("m1", 1000, "worktrees.list", json!({}));
        let out = evaluate(
            &raw,
            b"other-secret",
            &allow(&["worktrees.list"]),
            ScopeSet::parse("read"),
            1000,
            &mut g,
        );
        assert_eq!(out, Outcome::Rejected(RejectReason::BadMac));
    }

    #[test]
    fn stale_timestamp_is_rejected_both_directions() {
        let mut g = ReplayGuard::default_guard();
        let past = signed("old", 1000, "worktrees.list", json!({}));
        let out = evaluate(
            &past,
            SECRET,
            &allow(&["worktrees.list"]),
            ScopeSet::parse("read"),
            1000 + FRESHNESS_SECS + 1,
            &mut g,
        );
        assert_eq!(out, Outcome::Rejected(RejectReason::Stale));
        let future = signed("new", 100_000, "worktrees.list", json!({}));
        let out = evaluate(
            &future,
            SECRET,
            &allow(&["worktrees.list"]),
            ScopeSet::parse("read"),
            100_000 - FRESHNESS_SECS - 1,
            &mut g,
        );
        assert_eq!(out, Outcome::Rejected(RejectReason::Stale));
    }

    #[test]
    fn extreme_timestamps_are_stale_not_overflow() {
        // An adversarial `ts` at the i64 extremes must be rejected as stale, not
        // panic on an overflowing subtraction.
        let mut g = ReplayGuard::default_guard();
        for ts in [i64::MIN, i64::MAX] {
            let raw = signed("x", ts, "worktrees.list", json!({}));
            assert_eq!(
                evaluate(
                    &raw,
                    SECRET,
                    &allow(&["worktrees.list"]),
                    ScopeSet::parse("read"),
                    0,
                    &mut g
                ),
                Outcome::Rejected(RejectReason::Stale),
                "ts={ts}"
            );
        }
        // The guard's own admit is also overflow-safe at the extremes.
        assert_eq!(g.admit("y", i64::MIN, i64::MAX), Freshness::Stale);
    }

    #[test]
    fn replayed_id_is_rejected_second_time() {
        let mut g = ReplayGuard::default_guard();
        let raw = signed("dup", 1000, "worktrees.list", json!({}));
        let a = allow(&["worktrees.list"]);
        assert!(matches!(
            evaluate(&raw, SECRET, &a, ScopeSet::parse("read"), 1000, &mut g),
            Outcome::Accepted(_)
        ));
        assert_eq!(
            evaluate(&raw, SECRET, &a, ScopeSet::parse("read"), 1001, &mut g),
            Outcome::Rejected(RejectReason::Replay)
        );
    }

    #[test]
    fn unknown_and_unlisted_caps_are_rejected() {
        let mut g = ReplayGuard::default_guard();
        let unknown = signed("m1", 1000, "does.not.exist", json!({}));
        assert_eq!(
            evaluate(
                &unknown,
                SECRET,
                &allow(&["worktrees.list"]),
                ScopeSet::parse("read"),
                1000,
                &mut g
            ),
            Outcome::Rejected(RejectReason::UnknownCap)
        );
        let unlisted = signed("m2", 1000, "git.status", json!({}));
        assert_eq!(
            evaluate(
                &unlisted,
                SECRET,
                &allow(&["worktrees.list"]),
                ScopeSet::parse("read"),
                1000,
                &mut g
            ),
            Outcome::Rejected(RejectReason::NotAllowed)
        );
    }

    #[test]
    fn admin_cap_is_refused_even_when_allowlisted_and_ceiling_admin() {
        let mut g = ReplayGuard::default_guard();
        let raw = signed("m1", 1000, "daemon.shutdown", json!({}));
        // Allowlisted AND an admin ceiling — still refused unconditionally.
        let out = evaluate(
            &raw,
            SECRET,
            &allow(&["daemon.shutdown"]),
            ScopeSet::parse("admin"),
            1000,
            &mut g,
        );
        assert_eq!(out, Outcome::Rejected(RejectReason::AdminDenied));
    }

    #[test]
    fn scope_exceeded_when_cap_needs_more_than_ceiling() {
        let mut g = ReplayGuard::default_guard();
        // git.commit needs `git`; ceiling is read-only.
        let raw = signed(
            "m1",
            1000,
            "git.commit",
            json!({"worktree": "/w", "message": "x"}),
        );
        let out = evaluate(
            &raw,
            SECRET,
            &allow(&["git.commit"]),
            ScopeSet::parse("read"),
            1000,
            &mut g,
        );
        assert_eq!(out, Outcome::Rejected(RejectReason::ScopeExceeded));
        // With `git` in the ceiling it is admitted.
        let raw = signed(
            "m2",
            1000,
            "git.commit",
            json!({"worktree": "/w", "message": "x"}),
        );
        assert!(matches!(
            evaluate(
                &raw,
                SECRET,
                &allow(&["git.commit"]),
                ScopeSet::parse("read,git"),
                1000,
                &mut g
            ),
            Outcome::Accepted(_)
        ));
    }

    #[test]
    fn wrong_version_is_rejected() {
        let mut g = ReplayGuard::default_guard();
        let raw = json!({ "v": 99, "id": "m1", "ts": 1000, "cap": "worktrees.list", "params": {}, "mac": "00" }).to_string();
        assert_eq!(
            evaluate(
                &raw,
                SECRET,
                &allow(&["worktrees.list"]),
                ScopeSet::parse("read"),
                1000,
                &mut g
            ),
            Outcome::Rejected(RejectReason::UnsupportedVersion)
        );
    }

    #[test]
    fn malformed_json_is_rejected() {
        let mut g = ReplayGuard::default_guard();
        assert_eq!(
            evaluate(
                "not json",
                SECRET,
                &allow(&["worktrees.list"]),
                ScopeSet::parse("read"),
                1000,
                &mut g
            ),
            Outcome::Rejected(RejectReason::BadJson)
        );
        // Missing required field (mac).
        let raw = json!({ "v": 1, "id": "m", "ts": 1, "cap": "x" }).to_string();
        assert_eq!(
            evaluate(
                &raw,
                SECRET,
                &allow(&["x"]),
                ScopeSet::parse("read"),
                1,
                &mut g
            ),
            Outcome::Rejected(RejectReason::BadJson)
        );
    }

    #[test]
    fn replay_guard_prunes_by_window_and_capacity() {
        let mut g = ReplayGuard::new(100, 2);
        assert_eq!(g.admit("a", 0, 0), Freshness::Fresh);
        assert_eq!(g.admit("b", 0, 0), Freshness::Fresh);
        // Capacity 2: adding "c" evicts "a"; "a" is admittable again (fresh ts).
        assert_eq!(g.admit("c", 10, 10), Freshness::Fresh);
        assert_eq!(
            g.admit("a", 10, 10),
            Freshness::Fresh,
            "a was evicted by capacity"
        );
        // Age-based prune: far-future now drops old entries.
        assert_eq!(g.admit("d", 1000, 1000), Freshness::Fresh);
    }

    #[test]
    fn unauthenticated_message_does_not_poison_replay_cache() {
        let mut g = ReplayGuard::default_guard();
        // A forged message with a chosen id and a bad MAC.
        let forged = json!({ "v": 1, "id": "victim", "ts": 1000, "cap": "worktrees.list", "params": {}, "mac": "deadbeef" }).to_string();
        assert_eq!(
            evaluate(
                &forged,
                SECRET,
                &allow(&["worktrees.list"]),
                ScopeSet::parse("read"),
                1000,
                &mut g
            ),
            Outcome::Rejected(RejectReason::BadMac)
        );
        // The real signed message with the same id still goes through: the
        // forged one never reached the cache.
        let real = signed("victim", 1000, "worktrees.list", json!({}));
        assert!(matches!(
            evaluate(
                &real,
                SECRET,
                &allow(&["worktrees.list"]),
                ScopeSet::parse("read"),
                1000,
                &mut g
            ),
            Outcome::Accepted(_)
        ));
    }

    #[test]
    fn counters_tally_outcomes() {
        let mut c = Counters::default();
        c.record(&Outcome::Accepted(Accepted {
            id: "1".into(),
            cap: "x".into(),
            params: json!({}),
            scope: Scope::Read,
        }));
        c.record(&Outcome::Rejected(RejectReason::BadMac));
        c.record(&Outcome::Rejected(RejectReason::BadMac));
        assert_eq!(c.accepted, 1);
        assert_eq!(c.rejected_total(), 2);
        assert_eq!(c.rejected.get("bad_mac"), Some(&2));
    }

    #[test]
    fn reply_is_bounded() {
        let small = json!({"ok": 1});
        let r = build_reply("id1", Ok(&small), REPLY_CAP_BYTES);
        assert!(r.contains("\"ok\":true") && r.contains("\"result\""));
        assert!(r.len() <= REPLY_CAP_BYTES);
        // A huge result is clamped to a truncated marker under the cap.
        let big = json!({ "blob": "x".repeat(100_000) });
        let r = build_reply("id2", Ok(&big), REPLY_CAP_BYTES);
        assert!(r.len() <= REPLY_CAP_BYTES, "len {}", r.len());
        assert!(r.contains("truncated"));
        // An error reply is also bounded.
        let e = "boom".repeat(10_000);
        let r = build_reply("id3", Err(&e), REPLY_CAP_BYTES);
        assert!(r.len() <= REPLY_CAP_BYTES);
        assert!(r.contains("\"ok\":false"));
        // Multibyte content is clamped on a char boundary (never mid-codepoint):
        // a small cap over a long multibyte error must still yield valid UTF-8.
        let multi = "é".repeat(5_000); // 2 bytes each
        let r = build_reply("id4", Err(&multi), 200);
        assert!(r.len() <= 200, "bounded by the cap");
        // Re-parses as JSON ⇒ the clamp did not split a codepoint; ok is preserved.
        let v: Value = serde_json::from_str(&r).expect("valid UTF-8 JSON");
        assert_eq!(
            v["ok"],
            serde_json::json!(false),
            "error stays an error: {r}"
        );
    }

    #[test]
    fn rate_limiter_caps_per_window_and_resets() {
        let mut r = RateLimiter::new(2, 60);
        assert!(r.allow(1000));
        assert!(r.allow(1001));
        assert!(!r.allow(1002), "third in the window is denied");
        assert!(!r.allow(1059), "still within the window");
        assert!(r.allow(1060), "new window resets the count");
        assert!(r.allow(1061));
        assert!(!r.allow(1062));
    }

    #[test]
    fn reject_reasons_have_unique_strings() {
        let mut seen = std::collections::BTreeSet::new();
        for r in RejectReason::ALL {
            assert!(seen.insert(r.as_str()), "dup {}", r.as_str());
        }
        assert_eq!(seen.len(), RejectReason::ALL.len());
    }
}
