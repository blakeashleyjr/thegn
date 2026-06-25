//! Intelligent backoff for backend exhaustion.
//!
//! Port of the `exhaustionKind` / `classifyExhaustion` / `calculateBackoff`
//! logic from the Go `model-proxy` (`main.go`). Different 429s mean different
//! things — a payment 402 should not be re-probed every 30s like a transient
//! rate limit — so the kind selects a backoff profile.

use std::time::Duration;

/// Classifies the type of exhaustion for backoff-strategy selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExhaustionKind {
    Unknown,
    /// 429 — upstream rate limit, may carry a Retry-After.
    RateLimit,
    /// 402 — payment required / credit exhausted.
    Payment,
    /// 401/403 — auth issues, won't fix by retrying.
    Auth,
    /// 500-503 — upstream outage, may recover.
    ServerError,
    /// 400 — bad request, won't fix by retrying.
    ClientError,
}

impl ExhaustionKind {
    /// Permanent errors (auth, client config) are marked stale: the backend is
    /// parked rather than actively re-probed on a short cycle.
    pub fn is_stale(self) -> bool {
        matches!(self, ExhaustionKind::Auth | ExhaustionKind::ClientError)
    }
}

/// Extracts the exhaustion kind from a reason string or status code, mirroring
/// the Go `classifyExhaustion`. The reason string is checked first (it may carry
/// an upstream message that is more specific than the bare status), then the
/// status code is used as a fallback.
pub fn classify_exhaustion(reason: &str, status_code: u16) -> ExhaustionKind {
    let r = reason.to_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|n| r.contains(n));

    if has(&["401", "403", "unauthorized", "forbidden", "invalid api key"]) {
        return ExhaustionKind::Auth;
    }
    if has(&[
        "402",
        "payment",
        "credit",
        "billing",
        "usage limit",
        "insufficient_quota",
    ]) {
        return ExhaustionKind::Payment;
    }
    if has(&["429", "rate limit", "rate_limit", "quota"]) {
        return ExhaustionKind::RateLimit;
    }
    if has(&["500", "501", "502", "503", "outage", "upstream"]) {
        return ExhaustionKind::ServerError;
    }
    if has(&[
        "400",
        "no key",
        "no api key",
        "not found",
        "not_supported",
        "unsupported",
    ]) {
        return ExhaustionKind::ClientError;
    }

    match status_code {
        401 | 403 => ExhaustionKind::Auth,
        402 => ExhaustionKind::Payment,
        429 => ExhaustionKind::RateLimit,
        500..=503 => ExhaustionKind::ServerError,
        400 => ExhaustionKind::ClientError,
        _ => ExhaustionKind::Unknown,
    }
}

/// Backoff strategy for an exhaustion kind.
#[derive(Debug, Clone, Copy)]
pub struct BackoffConfig {
    /// First retry interval.
    pub initial: Duration,
    /// Exponential multiplier (1.0 == fixed/linear).
    pub multiplier: f64,
    /// Maximum retry interval.
    pub ceiling: Duration,
    /// Random jitter factor (0..1).
    pub jitter: f64,
}

const SECOND: Duration = Duration::from_secs(1);

/// 429 — transient; ramp from 30s to 5min.
pub const RATE_LIMIT_BACKOFF: BackoffConfig = BackoffConfig {
    initial: Duration::from_secs(30),
    multiplier: 2.0,
    ceiling: Duration::from_secs(5 * 60),
    jitter: 0.2,
};

/// 402 — out of credits, often only refills on a monthly reset; park ~6h.
pub const PAYMENT_BACKOFF: BackoffConfig = BackoffConfig {
    initial: Duration::from_secs(6 * 3600),
    multiplier: 1.0,
    ceiling: Duration::from_secs(24 * 3600),
    jitter: 0.1,
};

/// 5xx — upstream outage; ramp from 10s to 2min.
pub const SERVER_ERROR_BACKOFF: BackoffConfig = BackoffConfig {
    initial: Duration::from_secs(10),
    multiplier: 1.5,
    ceiling: Duration::from_secs(2 * 60),
    jitter: 0.3,
};

/// 401/403 — won't self-heal; fixed 10min, jumping to 30min after 3 failures.
pub const AUTH_BACKOFF: BackoffConfig = BackoffConfig {
    initial: Duration::from_secs(10 * 60),
    multiplier: 1.0,
    ceiling: Duration::from_secs(30 * 60),
    jitter: 0.1,
};

/// 400 — likely a config issue; fixed 5-15min.
pub const CLIENT_ERROR_BACKOFF: BackoffConfig = BackoffConfig {
    initial: Duration::from_secs(5 * 60),
    multiplier: 1.0,
    ceiling: Duration::from_secs(15 * 60),
    jitter: 0.0,
};

/// Returns the backoff profile for an exhaustion kind. Unknown defaults to the
/// rate-limit profile, matching Go.
pub fn backoff_config_for(kind: ExhaustionKind) -> BackoffConfig {
    match kind {
        ExhaustionKind::RateLimit => RATE_LIMIT_BACKOFF,
        ExhaustionKind::Payment => PAYMENT_BACKOFF,
        ExhaustionKind::Auth => AUTH_BACKOFF,
        ExhaustionKind::ServerError => SERVER_ERROR_BACKOFF,
        ExhaustionKind::ClientError => CLIENT_ERROR_BACKOFF,
        ExhaustionKind::Unknown => RATE_LIMIT_BACKOFF,
    }
}

/// Returns the next retry interval for an exhaustion kind and consecutive
/// failure count.
pub fn calculate_backoff(kind: ExhaustionKind, consecutive_failures: u32) -> Duration {
    backoff_from_config(backoff_config_for(kind), consecutive_failures)
}

/// Computes the next retry interval: exponential ramp with jitter, jumping to
/// the ceiling after 3+ consecutive failures. Jitter is drawn from the system
/// clock (as in Go); use [`backoff_from_config_jittered`] in tests for
/// determinism.
pub fn backoff_from_config(cfg: BackoffConfig, consecutive_failures: u32) -> Duration {
    backoff_from_config_jittered(cfg, consecutive_failures, wall_clock_jitter_ns())
}

/// Deterministic core of [`backoff_from_config`]. `jitter_ns` is the signed
/// jitter source (Go uses `time.Now().UnixNano()`); the applied jitter is
/// `((jitter_ns % (jitterMax*2)) - jitterMax)` nanoseconds.
pub fn backoff_from_config_jittered(
    cfg: BackoffConfig,
    consecutive_failures: u32,
    jitter_ns: i64,
) -> Duration {
    // Match Go: the 3+-failure ceiling jump returns before the jitter block.
    if consecutive_failures >= 3 {
        return cfg.ceiling;
    }
    let backoff = backoff_ramp(cfg, consecutive_failures);
    apply_jitter(backoff, cfg.jitter, jitter_ns)
}

/// The pre-jitter exponential ramp: `initial * multiplier^failures`, clamped to
/// the ceiling and jumping straight to it after 3+ consecutive failures. Pure
/// and exact (no jitter), so tests can assert precise interval values.
pub fn backoff_ramp(cfg: BackoffConfig, consecutive_failures: u32) -> Duration {
    if consecutive_failures >= 3 {
        return cfg.ceiling;
    }
    // Multiply via integer nanoseconds (f64-exact for these magnitudes) to avoid
    // the rounding that `Duration::mul_f64` introduces.
    let mut nanos = cfg.initial.as_nanos() as f64;
    let ceiling = cfg.ceiling.as_nanos() as f64;
    for _ in 0..consecutive_failures {
        nanos *= cfg.multiplier;
        if nanos > ceiling {
            nanos = ceiling;
            break;
        }
    }
    Duration::from_nanos(nanos as u64)
}

/// Applies Go-style symmetric jitter to a backoff: jitter spans
/// `[-jitterMax, +jitterMax)` nanoseconds where `jitterMax = backoff * factor`
/// seconds, never dropping below 1s. A `factor` of 0 is a no-op.
fn apply_jitter(backoff: Duration, factor: f64, jitter_ns: i64) -> Duration {
    if factor <= 0.0 {
        return backoff;
    }
    let jitter_max = (backoff.as_secs_f64() * factor) as i64; // in seconds, as Go
    if jitter_max <= 0 {
        return backoff;
    }
    let span = jitter_max * 2;
    let j = jitter_ns.rem_euclid(span) - jitter_max;
    let nanos = (backoff.as_nanos() as i64 + j).max(SECOND.as_nanos() as i64);
    Duration::from_nanos(nanos as u64)
}

fn wall_clock_jitter_ns() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_from_reason_string() {
        assert_eq!(
            classify_exhaustion("HTTP 401 (auth)", 0),
            ExhaustionKind::Auth
        );
        assert_eq!(
            classify_exhaustion("invalid api key", 0),
            ExhaustionKind::Auth
        );
        assert_eq!(
            classify_exhaustion("insufficient_quota", 0),
            ExhaustionKind::Payment
        );
        assert_eq!(
            classify_exhaustion("rate_limit exceeded", 0),
            ExhaustionKind::RateLimit
        );
        assert_eq!(
            classify_exhaustion("upstream outage", 0),
            ExhaustionKind::ServerError
        );
        assert_eq!(
            classify_exhaustion("model is not_supported", 0),
            ExhaustionKind::ClientError
        );
    }

    #[test]
    fn classify_falls_back_to_status_code() {
        assert_eq!(classify_exhaustion("", 401), ExhaustionKind::Auth);
        assert_eq!(classify_exhaustion("", 402), ExhaustionKind::Payment);
        assert_eq!(classify_exhaustion("", 429), ExhaustionKind::RateLimit);
        assert_eq!(classify_exhaustion("", 503), ExhaustionKind::ServerError);
        assert_eq!(classify_exhaustion("", 400), ExhaustionKind::ClientError);
        assert_eq!(classify_exhaustion("", 418), ExhaustionKind::Unknown);
    }

    #[test]
    fn first_failure_uses_initial() {
        assert_eq!(backoff_ramp(RATE_LIMIT_BACKOFF, 0), Duration::from_secs(30));
    }

    #[test]
    fn exponential_ramp() {
        // 1 failure: 30s * 2 = 60s.
        assert_eq!(backoff_ramp(RATE_LIMIT_BACKOFF, 1), Duration::from_secs(60));
        // 2 failures: 30s * 2 * 2 = 120s.
        assert_eq!(
            backoff_ramp(RATE_LIMIT_BACKOFF, 2),
            Duration::from_secs(120)
        );
    }

    #[test]
    fn three_failures_jumps_to_ceiling() {
        assert_eq!(
            backoff_ramp(RATE_LIMIT_BACKOFF, 3),
            RATE_LIMIT_BACKOFF.ceiling
        );
        assert_eq!(
            backoff_from_config_jittered(RATE_LIMIT_BACKOFF, 3, 12345),
            RATE_LIMIT_BACKOFF.ceiling
        );
        assert_eq!(
            calculate_backoff(ExhaustionKind::Payment, 5),
            PAYMENT_BACKOFF.ceiling
        );
    }

    #[test]
    fn fixed_multiplier_stays_at_initial_until_ceiling_jump() {
        // Auth backoff multiplier is 1.0, so 1-2 failures stay at initial (10min).
        assert_eq!(backoff_ramp(AUTH_BACKOFF, 1), Duration::from_secs(10 * 60));
        assert_eq!(backoff_ramp(AUTH_BACKOFF, 2), Duration::from_secs(10 * 60));
    }

    #[test]
    fn jitter_is_bounded_and_positive() {
        // With a large positive jitter source, the result stays within
        // [backoff - jitterMax, backoff + jitterMax] and never below 1s.
        let base = RATE_LIMIT_BACKOFF.initial.as_secs_f64();
        let jitter_max = base * RATE_LIMIT_BACKOFF.jitter;
        let d = backoff_from_config_jittered(RATE_LIMIT_BACKOFF, 0, i64::MAX);
        let secs = d.as_secs_f64();
        assert!(secs >= 1.0);
        assert!((secs - base).abs() <= jitter_max + 1.0);
    }

    #[test]
    fn every_config_arm_is_selected() {
        // Each kind maps to its profile (covers backoff_config_for arms),
        // checked on the jitter-free initial interval.
        let init = |k| backoff_config_for(k).initial;
        assert_eq!(init(ExhaustionKind::RateLimit), Duration::from_secs(30));
        assert_eq!(init(ExhaustionKind::ServerError), Duration::from_secs(10));
        assert_eq!(init(ExhaustionKind::Auth), Duration::from_secs(10 * 60));
        assert_eq!(
            init(ExhaustionKind::ClientError),
            Duration::from_secs(5 * 60)
        );
        assert_eq!(init(ExhaustionKind::Payment), Duration::from_secs(6 * 3600));
        // Unknown falls back to the rate-limit profile.
        assert_eq!(init(ExhaustionKind::Unknown), Duration::from_secs(30));
        // The wall-clock-jittered wrapper stays within one jitter step of 30s.
        let d = calculate_backoff(ExhaustionKind::RateLimit, 0);
        assert!((d.as_secs_f64() - 30.0).abs() <= 6.0 + 1.0);
    }

    #[test]
    fn ramp_clamps_at_ceiling_before_third_failure() {
        // server-error: 10s *1.5^2 = 22.5s (< 120s ceiling); push multiplier to
        // clamp by using a config that overshoots.
        let cfg = BackoffConfig {
            initial: Duration::from_secs(100),
            multiplier: 10.0,
            ceiling: Duration::from_secs(200),
            jitter: 0.0,
        };
        // 1 failure: 100*10 = 1000 > 200 → clamps to ceiling.
        assert_eq!(backoff_ramp(cfg, 1), Duration::from_secs(200));
    }

    #[test]
    fn jitter_noop_when_factor_zero_or_tiny() {
        // client-error has jitter 0.0 → exact.
        assert_eq!(
            backoff_from_config_jittered(CLIENT_ERROR_BACKOFF, 0, 999),
            CLIENT_ERROR_BACKOFF.initial
        );
        // A sub-second backoff with a small factor yields jitter_max==0 → no-op.
        let cfg = BackoffConfig {
            initial: Duration::from_millis(100),
            multiplier: 1.0,
            ceiling: Duration::from_secs(1),
            jitter: 0.1,
        };
        assert_eq!(
            backoff_from_config_jittered(cfg, 0, 12345),
            Duration::from_millis(100)
        );
    }

    #[test]
    fn kind_staleness() {
        assert!(ExhaustionKind::Auth.is_stale());
        assert!(ExhaustionKind::ClientError.is_stale());
        assert!(!ExhaustionKind::RateLimit.is_stale());
        assert!(!ExhaustionKind::Payment.is_stale());
    }
}
