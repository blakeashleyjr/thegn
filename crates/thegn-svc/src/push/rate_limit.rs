//! Pure bounded rate-limit state for one push sink.
//!
//! The host worker owns one of these per sink. It never grows a deferred
//! backlog: when the remaining retry window cannot carry a message, the
//! transition is `Drop` and the caller increments its delivery counter.

use std::time::{Duration, Instant};

use thegn_core::config::PushKind;

pub const MAX_RETRY_AFTER: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Send,
    Defer(Duration),
    Drop,
}

/// A fractional token bucket. Fractional tokens make the one-request/second
/// Slack policy precise without introducing a periodic timer.
#[derive(Debug)]
pub struct TokenBucket {
    capacity: f64,
    refill_per_second: f64,
    tokens: f64,
    last_refill: Instant,
}

impl TokenBucket {
    pub fn new(capacity: u32, refill_per_second: f64) -> Self {
        assert!(capacity > 0, "a rate limiter needs positive capacity");
        assert!(refill_per_second.is_finite() && refill_per_second > 0.0);
        Self {
            capacity: f64::from(capacity),
            refill_per_second,
            tokens: f64::from(capacity),
            last_refill: Instant::now(),
        }
    }

    pub fn for_kind(kind: PushKind) -> Self {
        match kind {
            // Discord's documented limit is approximately 30 requests/minute.
            PushKind::Discord => Self::new(30, 0.5),
            // Slack incoming webhooks are approximately one request/second.
            PushKind::Slack => Self::new(1, 1.0),
            // Generic endpoints have no shared platform limit; retain a
            // conservative bounded default rather than allowing a flood.
            PushKind::Webhook | PushKind::Ntfy => Self::new(10, 10.0),
            PushKind::Telegram | PushKind::Gotify | PushKind::Pushover => Self::new(1, 1.0),
        }
    }

    pub fn decide(&mut self, retry_budget: Duration) -> Decision {
        self.decide_at(Instant::now(), retry_budget)
    }

    pub fn decide_at(&mut self, now: Instant, retry_budget: Duration) -> Decision {
        self.refill(now);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            return Decision::Send;
        }
        let wait = Duration::from_secs_f64((1.0 - self.tokens) / self.refill_per_second);
        if wait <= retry_budget {
            Decision::Defer(wait)
        } else {
            Decision::Drop
        }
    }

    fn refill(&mut self, now: Instant) {
        let elapsed = now
            .saturating_duration_since(self.last_refill)
            .as_secs_f64();
        if elapsed > 0.0 {
            self.tokens = (self.tokens + elapsed * self.refill_per_second).min(self.capacity);
            self.last_refill = now;
        }
    }
}

/// The named limiter used by worker code, retained as a descriptive type for
/// callers that do not need to know the token-bucket implementation.
pub type RateLimiter = TokenBucket;

/// Parse a numeric `Retry-After` value and clamp it to the bounded retry
/// window. Invalid or absent values use the same conservative one-second
/// delay as a typical webhook service.
pub fn parse_retry_after(value: Option<&str>) -> Duration {
    let seconds = value
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(1);
    Duration::from_secs(seconds).min(MAX_RETRY_AFTER)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_sends_then_defers_then_refills() {
        let start = Instant::now();
        let mut bucket = TokenBucket {
            capacity: 1.0,
            refill_per_second: 1.0,
            tokens: 1.0,
            last_refill: start,
        };
        assert_eq!(bucket.decide_at(start, Duration::ZERO), Decision::Send);
        assert_eq!(
            bucket.decide_at(start, Duration::from_secs(1)),
            Decision::Defer(Duration::from_secs(1))
        );
        assert_eq!(
            bucket.decide_at(start, Duration::from_millis(500)),
            Decision::Drop
        );
        assert_eq!(
            bucket.decide_at(start + Duration::from_secs(1), Duration::ZERO),
            Decision::Send
        );
    }

    #[test]
    fn retry_after_is_bounded_and_safe() {
        assert_eq!(parse_retry_after(Some("5")), Duration::from_secs(5));
        assert_eq!(parse_retry_after(Some("999999")), MAX_RETRY_AFTER);
        assert_eq!(parse_retry_after(Some("garbage")), Duration::from_secs(1));
        assert_eq!(parse_retry_after(None), Duration::from_secs(1));
    }

    #[test]
    fn provider_defaults_match_documented_rates() {
        let mut discord = TokenBucket::for_kind(PushKind::Discord);
        for _ in 0..30 {
            assert_eq!(discord.decide(Duration::ZERO), Decision::Send);
        }
        assert!(matches!(
            discord.decide(Duration::from_secs(2)),
            Decision::Defer(_)
        ));
        assert_eq!(TokenBucket::for_kind(PushKind::Slack).capacity, 1.0);
    }
}
