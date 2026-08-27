//! Shell-completion **policy**: which argument slot takes which values, what a
//! candidate is allowed to look like, and how long a `<TAB>` may take.
//!
//! This is the compile-time third of THE-36's three layers (packaging owns
//! delivery, the binary owns answers, this module owns policy). It is pure and
//! substrate-free by design: no shell protocol, no clap types, no async, no new
//! dependency. The host's `crates/thegn-host/src/complete.rs` is the only place
//! that knows about `clap_complete`, and it projects what lives here.
//!
//! Three pieces:
//!
//! - [`catalog`] — `CATALOG`, the single source of truth mapping
//!   `(command path, arg id) → SourceKind`. The host's drift test walks the live
//!   clap tree against it, so a new verb with an uncompletable argument is a
//!   test failure rather than a silent gap.
//! - [`candidate`] — the [`Candidate`] type and the filtering pipeline
//!   ([`candidate::refine`]): prefix match, stable de-dup, a hard cap, and
//!   sanitisation of shell-hostile values.
//! - [`sources`] — the [`sources::CompletionSource`] seam plus the
//!   implementations. The DB-derived ones are the only I/O in this module, and
//!   they open the state DB **read-only** (see that file for why).
//!
//! ## Why there is a budget at all
//!
//! Every `<TAB>` press is a process launch, and the user is waiting on it with
//! their hand still on the keyboard. [`Deadline`] is a plain arithmetic
//! deadline checked *between* sources — not a watchdog thread. The real
//! enforcement is that every source's I/O is bounded by construction (a
//! read-only SQLite handle with a short busy timeout, a config file read; never
//! a network call, a subprocess, or a git invocation). The deadline is the belt
//! to those braces.

pub mod candidate;
pub mod catalog;
pub mod sources;

pub use candidate::{Candidate, MAX_CANDIDATES, MAX_DESCRIPTION_CHARS};
pub use catalog::{CATALOG, Reserved, Slot, SourceKind};

/// Environment override for the per-request budget, in milliseconds.
pub const BUDGET_ENV: &str = "THEGN_COMPLETE_BUDGET_MS";

/// Default per-request budget. 100 ms is comfortably under the ~150 ms at which
/// a keypress stops feeling instant, and well above what any implemented source
/// costs in practice.
pub const DEFAULT_BUDGET_MS: u64 = 100;

/// Parse a budget from its raw environment value. Anything unparseable, zero,
/// or absent falls back to [`DEFAULT_BUDGET_MS`] — a `<TAB>` never fails
/// because of a typo'd env var, it just uses the default.
pub fn budget_ms_from(raw: Option<&str>) -> u64 {
    raw.map(str::trim)
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|ms| *ms > 0)
        .unwrap_or(DEFAULT_BUDGET_MS)
}

/// The wall-clock budget for one completion request.
///
/// Deliberately arithmetic over an injected [`std::time::Instant`]: every
/// predicate has an `_at(now)` form so the policy is unit-testable without
/// sleeping, and the convenience wrappers just pass `Instant::now()`.
#[derive(Debug, Clone, Copy)]
pub struct Deadline {
    started: std::time::Instant,
    budget: std::time::Duration,
}

impl Deadline {
    /// A deadline of `budget_ms` starting now.
    pub fn new(budget_ms: u64) -> Self {
        Self::starting_at(std::time::Instant::now(), budget_ms)
    }

    /// A deadline of `budget_ms` starting at an explicit instant.
    pub fn starting_at(started: std::time::Instant, budget_ms: u64) -> Self {
        Self {
            started,
            budget: std::time::Duration::from_millis(budget_ms),
        }
    }

    /// The deadline the process environment asks for (see [`BUDGET_ENV`]).
    pub fn from_env() -> Self {
        Self::new(budget_ms_from(std::env::var(BUDGET_ENV).ok().as_deref()))
    }

    /// The configured budget.
    pub fn budget(&self) -> std::time::Duration {
        self.budget
    }

    /// Whether the budget is used up as of `now`.
    pub fn expired_at(&self, now: std::time::Instant) -> bool {
        now.saturating_duration_since(self.started) >= self.budget
    }

    /// Whether the budget is used up.
    pub fn expired(&self) -> bool {
        self.expired_at(std::time::Instant::now())
    }

    /// How much budget is left as of `now` (saturating at zero).
    pub fn remaining_at(&self, now: std::time::Instant) -> std::time::Duration {
        self.budget
            .saturating_sub(now.saturating_duration_since(self.started))
    }

    /// How much budget is left (saturating at zero).
    pub fn remaining(&self) -> std::time::Duration {
        self.remaining_at(std::time::Instant::now())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn budget_parses_or_defaults() {
        assert_eq!(budget_ms_from(Some("250")), 250);
        assert_eq!(budget_ms_from(Some("  40 ")), 40);
        assert_eq!(budget_ms_from(None), DEFAULT_BUDGET_MS);
        // Unparseable, negative and zero all fall back rather than failing.
        assert_eq!(budget_ms_from(Some("")), DEFAULT_BUDGET_MS);
        assert_eq!(budget_ms_from(Some("soon")), DEFAULT_BUDGET_MS);
        assert_eq!(budget_ms_from(Some("-5")), DEFAULT_BUDGET_MS);
        assert_eq!(budget_ms_from(Some("0")), DEFAULT_BUDGET_MS);
    }

    #[test]
    fn deadline_is_pure_arithmetic() {
        let t0 = Instant::now();
        let d = Deadline::starting_at(t0, 100);
        assert_eq!(d.budget(), Duration::from_millis(100));
        assert!(!d.expired_at(t0));
        assert!(!d.expired_at(t0 + Duration::from_millis(99)));
        // The boundary counts as expired: a source that would start exactly at
        // the deadline has no budget left to spend.
        assert!(d.expired_at(t0 + Duration::from_millis(100)));
        assert!(d.expired_at(t0 + Duration::from_millis(5_000)));
        assert_eq!(
            d.remaining_at(t0 + Duration::from_millis(40)),
            Duration::from_millis(60)
        );
        // Saturating, never a panic on an overrun.
        assert_eq!(d.remaining_at(t0 + Duration::from_secs(9)), Duration::ZERO);
    }

    #[test]
    fn deadline_now_wrappers_agree_with_the_pure_form() {
        let d = Deadline::new(10_000);
        assert!(!d.expired());
        assert!(d.remaining() > Duration::ZERO);
        let spent = Deadline::starting_at(Instant::now() - Duration::from_secs(1), 10);
        assert!(spent.expired());
        assert_eq!(spent.remaining(), Duration::ZERO);
    }

    #[test]
    fn from_env_reads_the_override() {
        // `testenv` serialises env mutation across the crate's tests.
        let _g = crate::testenv::EnvGuard::set(&[(BUDGET_ENV, "777")]);
        assert_eq!(Deadline::from_env().budget(), Duration::from_millis(777));
        drop(_g);
        let _g = crate::testenv::EnvGuard::unset(&[BUDGET_ENV]);
        assert_eq!(
            Deadline::from_env().budget(),
            Duration::from_millis(DEFAULT_BUDGET_MS)
        );
    }
}
