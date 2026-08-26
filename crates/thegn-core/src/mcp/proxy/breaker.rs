//! The per-upstream-instance circuit breaker.
//!
//! A failing upstream must be fenced without taking down the aggregated
//! endpoint or the other upstreams. The breaker is a pure, clock-injected state
//! machine: `Closed` →(N consecutive failures/timeouts)→ `Open`
//! →(cooldown elapsed)→ `HalfOpen` →(probe ok)→ `Closed` (or a probe failure
//! →`Open` again). While `Open`, the host answers that upstream's tool calls
//! with a fast error naming it — it never forwards to the wedged child.
//!
//! Every transition takes an injected `now_ms`, so the whole machine is
//! exhaustively table-testable with no real time.

/// Breaker state. `Open` fast-fails; `HalfOpen` lets one probe through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakerState {
    Closed,
    Open,
    HalfOpen,
}

impl BreakerState {
    pub fn as_str(self) -> &'static str {
        match self {
            BreakerState::Closed => "closed",
            BreakerState::Open => "open",
            BreakerState::HalfOpen => "half_open",
        }
    }
}

/// Breaker tuning (from `[mcp_proxy]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BreakerConfig {
    /// Consecutive failures that trip a closed breaker open.
    pub failure_threshold: u32,
    /// Milliseconds an open breaker waits before allowing a half-open probe.
    pub cooldown_ms: i64,
}

impl Default for BreakerConfig {
    fn default() -> Self {
        Self {
            failure_threshold: 3,
            cooldown_ms: 30_000,
        }
    }
}

/// One upstream instance's breaker.
#[derive(Debug, Clone)]
pub struct Breaker {
    cfg: BreakerConfig,
    state: BreakerState,
    consecutive_failures: u32,
    /// When the breaker last opened (ms). Only meaningful while `Open`.
    opened_at: i64,
}

impl Breaker {
    pub fn new(cfg: BreakerConfig) -> Self {
        Self {
            cfg: BreakerConfig {
                // A zero threshold would open on the first success-less probe
                // and never serve — clamp to at least one real failure.
                failure_threshold: cfg.failure_threshold.max(1),
                cooldown_ms: cfg.cooldown_ms.max(0),
            },
            state: BreakerState::Closed,
            consecutive_failures: 0,
            opened_at: 0,
        }
    }

    /// The externally-visible state *as of `now_ms`*: an `Open` breaker whose
    /// cooldown has elapsed reports `HalfOpen` (and [`allow`] would let a probe
    /// through). Pure read — does not mutate.
    pub fn state(&self, now_ms: i64) -> BreakerState {
        match self.state {
            BreakerState::Open if self.cooled_down(now_ms) => BreakerState::HalfOpen,
            other => other,
        }
    }

    /// Whether a call may be forwarded to the upstream right now. Transitions an
    /// `Open` breaker to `HalfOpen` once its cooldown has elapsed (so exactly
    /// one probe is admitted). `Closed`/`HalfOpen` always allow.
    pub fn allow(&mut self, now_ms: i64) -> bool {
        match self.state {
            BreakerState::Closed | BreakerState::HalfOpen => true,
            BreakerState::Open => {
                if self.cooled_down(now_ms) {
                    self.state = BreakerState::HalfOpen;
                    true
                } else {
                    false
                }
            }
        }
    }

    /// Record a successful call/probe: clears the failure count and closes the
    /// breaker.
    pub fn on_success(&mut self) {
        self.consecutive_failures = 0;
        self.state = BreakerState::Closed;
    }

    /// Record a failed call/probe. A failure while `HalfOpen` re-opens
    /// immediately (the probe told us it is still bad); otherwise the breaker
    /// opens once `failure_threshold` consecutive failures accrue.
    pub fn on_failure(&mut self, now_ms: i64) {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        match self.state {
            BreakerState::HalfOpen => {
                self.state = BreakerState::Open;
                self.opened_at = now_ms;
            }
            BreakerState::Closed => {
                if self.consecutive_failures >= self.cfg.failure_threshold {
                    self.state = BreakerState::Open;
                    self.opened_at = now_ms;
                }
            }
            BreakerState::Open => {
                // Already open (a call that slipped through before cooldown, or
                // a bookkeeping failure) — keep it open, don't reset the clock.
            }
        }
    }

    /// Consecutive failures accrued (for status/doctor).
    pub fn failures(&self) -> u32 {
        self.consecutive_failures
    }

    fn cooled_down(&self, now_ms: i64) -> bool {
        now_ms.saturating_sub(self.opened_at) >= self.cfg.cooldown_ms
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn breaker() -> Breaker {
        Breaker::new(BreakerConfig {
            failure_threshold: 3,
            cooldown_ms: 1_000,
        })
    }

    #[test]
    fn starts_closed_and_allows() {
        let mut b = breaker();
        assert_eq!(b.state(0), BreakerState::Closed);
        assert!(b.allow(0));
    }

    #[test]
    fn opens_after_threshold_consecutive_failures() {
        let mut b = breaker();
        b.on_failure(0);
        assert_eq!(
            b.state(0),
            BreakerState::Closed,
            "one failure is not enough"
        );
        b.on_failure(0);
        assert_eq!(
            b.state(0),
            BreakerState::Closed,
            "two failures is not enough"
        );
        b.on_failure(0);
        assert_eq!(b.state(0), BreakerState::Open, "third trips it");
        assert!(!b.allow(0), "open fails fast");
        assert_eq!(b.failures(), 3);
    }

    #[test]
    fn a_success_resets_the_failure_run() {
        let mut b = breaker();
        b.on_failure(0);
        b.on_failure(0);
        b.on_success();
        assert_eq!(b.failures(), 0);
        // Two more failures should NOT open it (the run was reset).
        b.on_failure(0);
        b.on_failure(0);
        assert_eq!(b.state(0), BreakerState::Closed);
    }

    #[test]
    fn open_then_cooldown_admits_one_probe_half_open() {
        let mut b = breaker();
        for _ in 0..3 {
            b.on_failure(10);
        }
        assert_eq!(b.state(10), BreakerState::Open);
        // Before cooldown: still open, no probe.
        assert!(!b.allow(500));
        assert_eq!(b.state(500), BreakerState::Open);
        // At/after cooldown: state() previews half-open; allow() admits a probe.
        assert_eq!(b.state(1_010), BreakerState::HalfOpen);
        assert!(b.allow(1_010));
        assert_eq!(b.state(1_010), BreakerState::HalfOpen);
    }

    #[test]
    fn half_open_probe_success_closes() {
        let mut b = breaker();
        for _ in 0..3 {
            b.on_failure(0);
        }
        assert!(b.allow(2_000)); // → half-open probe
        b.on_success();
        assert_eq!(b.state(2_000), BreakerState::Closed);
        assert!(b.allow(2_000));
    }

    #[test]
    fn half_open_probe_failure_reopens_immediately() {
        let mut b = breaker();
        for _ in 0..3 {
            b.on_failure(0);
        }
        assert!(b.allow(2_000)); // → half-open probe
        b.on_failure(2_000); // probe failed
        assert_eq!(b.state(2_000), BreakerState::Open);
        assert!(!b.allow(2_100), "re-opened, cooldown restarts from 2_000");
        assert!(b.allow(3_000), "cooldown elapses again");
    }

    #[test]
    fn zero_threshold_is_clamped_to_one() {
        let mut b = Breaker::new(BreakerConfig {
            failure_threshold: 0,
            cooldown_ms: 100,
        });
        assert_eq!(b.state(0), BreakerState::Closed);
        b.on_failure(0);
        assert_eq!(b.state(0), BreakerState::Open, "clamped to 1 failure");
    }

    #[test]
    fn state_str_labels() {
        assert_eq!(BreakerState::Closed.as_str(), "closed");
        assert_eq!(BreakerState::Open.as_str(), "open");
        assert_eq!(BreakerState::HalfOpen.as_str(), "half_open");
    }

    #[test]
    fn failure_while_open_keeps_original_open_clock() {
        let mut b = breaker();
        for _ in 0..3 {
            b.on_failure(0);
        }
        assert_eq!(b.state(0), BreakerState::Open);
        // A stray failure recorded while open must not push the cooldown out.
        b.on_failure(500);
        assert!(
            b.allow(1_000),
            "cooldown still measured from the original open"
        );
    }

    #[test]
    fn default_config_is_sane() {
        let d = BreakerConfig::default();
        assert_eq!(d.failure_threshold, 3);
        assert_eq!(d.cooldown_ms, 30_000);
    }
}
