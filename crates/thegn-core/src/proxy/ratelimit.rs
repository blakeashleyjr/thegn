//! Per-identity token-bucket rate limiting.
//!
//! Port of `ratelimit.go`. Backends that share an upstream OAuth identity also
//! share that provider's rate limit, so the limiter paces requests per identity
//! with a refilling token bucket — smoothing bursts to a sustainable rate rather
//! than stampeding the upstream and tripping a per-minute limit that cools the
//! whole chain at once.
//!
//! Unlike the Go original, the bucket never blocks (core has no async runtime):
//! [`TokenBucket::try_take`] is the non-blocking load-shedding probe, and
//! [`TokenBucket::reserve`] returns how long the async layer must sleep before a
//! token is available. All time is passed in as an explicit [`Instant`] so the
//! logic is deterministic under test.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// A classic refilling token bucket. `rps` tokens are added per second up to
/// `burst` capacity.
#[derive(Debug)]
pub struct TokenBucket {
    rps: f64,
    burst: f64,
    tokens: f64,
    last: Instant,
}

impl TokenBucket {
    /// Creates a bucket starting full (`burst` tokens), anchored at `now`.
    pub fn new(rps: f64, burst: f64, now: Instant) -> Self {
        Self {
            rps,
            burst,
            tokens: burst,
            last: now,
        }
    }

    /// Advances the bucket to `now`, accumulating tokens up to `burst`.
    fn refill(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        if elapsed > 0.0 {
            self.tokens = (self.tokens + elapsed * self.rps).min(self.burst);
            self.last = now;
        }
    }

    /// Consumes one token if one is available at `now`, returning `true`. Never
    /// blocks — an empty bucket returns `false` immediately. This is the
    /// load-shedding probe used by the cascade router.
    pub fn try_take(&mut self, now: Instant) -> bool {
        self.refill(now);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// How long until at least one whole token is available, measured from
    /// `now`. Returns [`Duration::ZERO`] when a token is already available. The
    /// async layer sleeps for this long, then calls [`try_take`](Self::try_take).
    pub fn reserve(&mut self, now: Instant) -> Duration {
        self.refill(now);
        if self.tokens >= 1.0 {
            return Duration::ZERO;
        }
        let deficit = 1.0 - self.tokens;
        let secs = deficit / self.rps;
        let wait = Duration::from_secs_f64(secs.max(0.0));
        // Mirror Go's floor of 1ms so a busy-wait can't spin.
        wait.max(Duration::from_millis(1))
    }
}

/// A `(requests-per-minute, burst)` policy for an identity.
#[derive(Debug, Clone, Copy)]
pub struct RatePolicy {
    pub rpm: f64,
    pub burst: f64,
}

impl RatePolicy {
    /// Tokens-per-second the bucket refills at.
    pub fn rps(&self) -> f64 {
        self.rpm / 60.0
    }
}

/// Parses `"<rpm>"` or `"<rpm>:<burst>"`. A zero/negative rpm is rejected so a
/// bad override can't wedge the limiter. Port of `parseRatePolicy` — note the Go
/// quirk that a bare `<rpm>` (no `:`) defaults burst to 5.
pub fn parse_rate_policy(s: &str) -> Option<RatePolicy> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (rpm_str, burst_str) = match s.split_once(':') {
        Some((r, b)) => (r, Some(b)),
        None => (s, None),
    };
    let rpm: f64 = rpm_str.trim().parse().ok()?;
    if rpm <= 0.0 {
        return None;
    }
    let burst = match burst_str {
        Some(b) => b
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|v| *v >= 1.0)
            .unwrap_or(rpm),
        None => 5.0,
    };
    Some(RatePolicy { rpm, burst })
}

/// Holds one token bucket per identity, created lazily from a policy lookup.
pub struct RateLimiter {
    buckets: Mutex<HashMap<String, TokenBucket>>,
}

impl RateLimiter {
    pub fn new() -> Self {
        Self {
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// Non-blocking admission gate: consumes a token for `identity` if one is
    /// available right now. A `false` result means the identity is at its
    /// sustainable rate, so the caller should cascade to the next backend. The
    /// bucket is created from `policy` on first use.
    pub fn try_acquire(&self, identity: &str, policy: RatePolicy, now: Instant) -> bool {
        let mut buckets = self.buckets.lock().unwrap();
        buckets
            .entry(identity.to_string())
            .or_insert_with(|| TokenBucket::new(policy.rps(), policy.burst, now))
            .try_take(now)
    }

    /// How long the caller must wait before a token is available for `identity`.
    /// [`Duration::ZERO`] means a token can be taken now. Does not consume a
    /// token (the caller calls [`try_acquire`](Self::try_acquire) after waking).
    pub fn reserve(&self, identity: &str, policy: RatePolicy, now: Instant) -> Duration {
        let mut buckets = self.buckets.lock().unwrap();
        buckets
            .entry(identity.to_string())
            .or_insert_with(|| TokenBucket::new(policy.rps(), policy.burst, now))
            .reserve(now)
    }
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

/// Counts in-flight requests per shared-quota identity. The token bucket sees
/// request *rate*; this catches N agents each holding a slow streaming turn open
/// at a low per-second rate. Port of `inflightTracker`.
#[derive(Default)]
pub struct InflightTracker {
    count: Mutex<HashMap<String, u32>>,
}

impl InflightTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether `identity` is already at or above `cap` without mutating the
    /// counter. A `cap` of 0 means "no limit".
    pub fn at_cap(&self, identity: &str, cap: u32) -> bool {
        if cap == 0 {
            return false;
        }
        *self.count.lock().unwrap().get(identity).unwrap_or(&0) >= cap
    }

    /// Increments the in-flight count for an identity.
    pub fn enter(&self, identity: &str) {
        *self
            .count
            .lock()
            .unwrap()
            .entry(identity.to_string())
            .or_insert(0) += 1;
    }

    /// Decrements the in-flight count, flooring at 0 so a double-decrement can't
    /// drive the gauge negative.
    pub fn leave(&self, identity: &str) {
        let mut count = self.count.lock().unwrap();
        if let Some(n) = count.get_mut(identity) {
            *n = n.saturating_sub(1);
        }
    }

    /// Current in-flight count for an identity (for the metrics gauge).
    pub fn get(&self, identity: &str) -> u32 {
        *self.count.lock().unwrap().get(identity).unwrap_or(&0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_starts_full_then_drains() {
        let t0 = Instant::now();
        let mut b = TokenBucket::new(60.0 / 60.0, 3.0, t0); // 1 rps, burst 3
        assert!(b.try_take(t0));
        assert!(b.try_take(t0));
        assert!(b.try_take(t0));
        assert!(!b.try_take(t0)); // empty
    }

    #[test]
    fn bucket_refills_over_time() {
        let t0 = Instant::now();
        let mut b = TokenBucket::new(2.0, 2.0, t0); // 2 rps
        assert!(b.try_take(t0));
        assert!(b.try_take(t0));
        assert!(!b.try_take(t0));
        // After 1s at 2 rps, ~2 tokens are back.
        let t1 = t0 + Duration::from_secs(1);
        assert!(b.try_take(t1));
        assert!(b.try_take(t1));
    }

    #[test]
    fn refill_caps_at_burst() {
        let t0 = Instant::now();
        let mut b = TokenBucket::new(10.0, 2.0, t0);
        b.try_take(t0);
        b.try_take(t0);
        // A long idle should not over-fill beyond burst (2).
        let t1 = t0 + Duration::from_secs(100);
        assert!(b.try_take(t1));
        assert!(b.try_take(t1));
        assert!(!b.try_take(t1));
    }

    #[test]
    fn reserve_returns_zero_when_token_available() {
        let t0 = Instant::now();
        let mut b = TokenBucket::new(1.0, 1.0, t0);
        assert_eq!(b.reserve(t0), Duration::ZERO);
    }

    #[test]
    fn reserve_estimates_wait_when_empty() {
        let t0 = Instant::now();
        let mut b = TokenBucket::new(1.0, 1.0, t0); // 1 rps
        assert!(b.try_take(t0));
        let wait = b.reserve(t0);
        // Need ~1 full token at 1 rps => ~1s.
        assert!(wait >= Duration::from_millis(900) && wait <= Duration::from_millis(1100));
    }

    #[test]
    fn parse_rate_policy_forms() {
        let p = parse_rate_policy("30:3").unwrap();
        assert_eq!(p.rpm, 30.0);
        assert_eq!(p.burst, 3.0);
        // Bare rpm defaults burst to 5 (Go quirk).
        let p = parse_rate_policy("60").unwrap();
        assert_eq!(p.burst, 5.0);
        assert!(parse_rate_policy("").is_none());
        assert!(parse_rate_policy("0").is_none());
        assert!(parse_rate_policy("-5").is_none());
        // Invalid burst falls back to rpm.
        let p = parse_rate_policy("60:abc").unwrap();
        assert_eq!(p.burst, 60.0);
    }

    #[test]
    fn limiter_buckets_are_per_identity() {
        let t0 = Instant::now();
        let rl = RateLimiter::new();
        let policy = RatePolicy {
            rpm: 60.0,
            burst: 1.0,
        };
        assert!(rl.try_acquire("a", policy, t0));
        assert!(!rl.try_acquire("a", policy, t0)); // a drained
        assert!(rl.try_acquire("b", policy, t0)); // b independent
    }

    #[test]
    fn limiter_reserve_and_policy_rps() {
        let policy = RatePolicy {
            rpm: 120.0,
            burst: 1.0,
        };
        assert_eq!(policy.rps(), 2.0);
        let rl = RateLimiter::default();
        let t0 = Instant::now();
        // First reserve: token available → zero wait.
        assert_eq!(rl.reserve("id", policy, t0), Duration::ZERO);
        assert!(rl.try_acquire("id", policy, t0));
        // Now empty: reserve returns a positive wait.
        assert!(rl.reserve("id", policy, t0) > Duration::ZERO);
    }

    #[test]
    fn inflight_enter_leave_floor() {
        let t = InflightTracker::new();
        t.enter("x");
        t.enter("x");
        assert_eq!(t.get("x"), 2);
        assert!(t.at_cap("x", 2));
        assert!(!t.at_cap("x", 3));
        assert!(!t.at_cap("x", 0)); // 0 == no limit
        t.leave("x");
        t.leave("x");
        t.leave("x"); // floors at 0
        assert_eq!(t.get("x"), 0);
    }
}
