//! Checked boundaries between durations, epochs, and periodic scheduler slots.
//!
//! Configuration validation is not the only caller: tests, provider data, and
//! programmatically constructed policy must also remain safe in release builds.
//! Zero/disable semantics stay with the caller rather than becoming a zero
//! scheduler divisor or an accidentally negative lifetime.

use std::num::NonZeroU64;

/// Maximum operational polling interval: 31 days. Existing feature floors and
/// `None` disable semantics still apply before scheduling.
pub const MAX_CADENCE_SECS: u64 = 31 * 24 * 60 * 60;
/// General duration ceiling: ten 365-day years. This is a duration, never an
/// upper bound on an absolute Unix timestamp.
pub const MAX_DURATION_SECS: u64 = 10 * 365 * 24 * 60 * 60;
pub const MAX_DURATION_MILLIS: u64 = MAX_DURATION_SECS * 1000;
pub const MAX_DURATION_DAYS: u64 = MAX_DURATION_SECS / (24 * 60 * 60);

/// Convert a polling cadence to nonzero slots without overflowing intermediate
/// units. Invalid direct/programmatic input is capped, never wrapped to a fast
/// cadence; raw config validation separately reports its invalid range.
pub fn cadence_slots(seconds: u64, floor_secs: u64, slot_ms: u64) -> NonZeroU64 {
    let seconds = seconds.max(floor_secs).min(MAX_CADENCE_SECS);
    cadence_millis_slots(u128::from(seconds) * 1000, slot_ms)
}

/// Millisecond-duration counterpart for subsecond model refresh intervals.
/// Ceiling division ensures a schedule never fires before its requested span.
pub fn cadence_millis_slots(millis: u128, slot_ms: u64) -> NonZeroU64 {
    let millis = millis.min(u128::from(MAX_CADENCE_SECS) * 1000);
    let slots = millis
        .div_ceil(u128::from(slot_ms.max(1)))
        .clamp(1, u128::from(u64::MAX));
    NonZeroU64::new(slots as u64).expect("clamped to nonzero u64")
}

/// Nonnegative age from trustworthy epoch seconds. Missing timestamps are
/// represented by the caller as `None`; nonpositive or future timestamps are
/// unknown, not evidence that a resource has expired.
pub fn age_seconds(now: i64, created_at: i64) -> Option<u64> {
    if created_at <= 0 || now < created_at {
        return None;
    }
    // Both operands are positive and ordered, so subtraction is representable.
    Some((now - created_at) as u64)
}

/// Cache freshness: future stamps remain fresh during clock rollback, while
/// absent/nonpositive timestamps are stale. A huge TTL must never narrow to a
/// negative number and turn a quiet cache into a hot refresh loop.
pub fn is_fresh(now: i64, fetched_at: Option<i64>, ttl_secs: u64) -> bool {
    if ttl_secs == 0 {
        return false;
    }
    fetched_at.is_some_and(|at| {
        at > 0 && (now < at || age_seconds(now, at).is_some_and(|age| age < ttl_secs))
    })
}

/// Check a remote-resource lifetime without authorizing expiry on invalid
/// input. `None` requires visible quarantine/reconciliation by the caller.
/// An explicit zero lifetime disables this expiry policy.
pub fn lifetime_expired(now: i64, created_at: Option<i64>, lifetime_secs: u64) -> Option<bool> {
    if lifetime_secs > MAX_DURATION_SECS {
        return None;
    }
    if lifetime_secs == 0 {
        return Some(false);
    }
    let age = age_seconds(now, created_at?)?;
    Some(age >= lifetime_secs)
}

/// Form an epoch-second deadline from an untrusted relative delay. Unsupported
/// delays and epoch overflow are absent, never a bogus deadline in the past.
pub fn deadline_seconds(now: i64, delay_secs: u64) -> Option<i64> {
    if now < 0 || delay_secs > MAX_DURATION_SECS {
        return None;
    }
    now.checked_add(i64::try_from(delay_secs).ok()?)
}

/// Checked relative delay in milliseconds. Absolute epochs use the full signed
/// range; only the relative delay is subject to the operational duration bound.
pub fn deadline_millis(now_ms: i64, delay_ms: u64) -> Option<i64> {
    if now_ms < 0 || delay_ms > MAX_DURATION_MILLIS {
        return None;
    }
    now_ms.checked_add(i64::try_from(delay_ms).ok()?)
}

/// Provider headers may contain fractional seconds. Reject nonfinite/oversized
/// input before conversion and round up so a retry never precedes the delay.
pub fn deadline_millis_from_seconds(now_ms: i64, seconds: f64) -> Option<i64> {
    if !seconds.is_finite() || !(0.0..=MAX_DURATION_SECS as f64).contains(&seconds) {
        return None;
    }
    deadline_millis(now_ms, (seconds * 1000.0).ceil() as u64)
}

/// Compare elapsed time in either unit without signed narrowing. Unlike a
/// provider creation timestamp, zero is a valid injected clock origin here.
pub fn elapsed_at_least(now: i64, since: i64, duration: u64) -> bool {
    now >= since && (i128::from(now) - i128::from(since)) as u128 >= u128::from(duration)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceExpiry {
    Keep,
    Expired,
    Quarantine(&'static str),
}

/// Age can authorize a destructive action only when all active time policies
/// are supported and provider creation time is known and ordered. Caller-owned
/// resource identity must be verified separately before supplying the timestamp.
pub fn resource_expiry(
    now: i64,
    created: Option<i64>,
    lifetime_secs: u64,
    orphan_after_secs: Option<u64>,
) -> ResourceExpiry {
    if lifetime_secs > MAX_DURATION_SECS
        || orphan_after_secs.is_some_and(|seconds| seconds > MAX_DURATION_SECS)
    {
        return ResourceExpiry::Quarantine(
            "unsupported lifetime policy; repair duration configuration",
        );
    }
    if lifetime_secs == 0 && orphan_after_secs.is_none() {
        return ResourceExpiry::Keep;
    }
    let Some(age) = created.and_then(|at| age_seconds(now, at)) else {
        return ResourceExpiry::Quarantine(
            "creation time is missing, invalid or future; reconcile provider inventory",
        );
    };
    if (lifetime_secs > 0 && age >= lifetime_secs)
        || orphan_after_secs.is_some_and(|seconds| age >= seconds)
    {
        ResourceExpiry::Expired
    } else {
        ResourceExpiry::Keep
    }
}

/// Saturating duration conversion for existing signed-millisecond APIs. Convert
/// in the wider domain first, so large unsigned durations cannot become negative.
pub fn duration_millis(seconds: u64) -> i64 {
    saturating_i64(u128::from(seconds) * 1000)
}

/// Narrow an unsigned measurement or epoch without modular wraparound.
pub fn saturating_i64(value: u128) -> i64 {
    value.min(i64::MAX as u128) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expiry_quarantines_unknown_provider_time_and_bad_policy() {
        for now in [1, 1_700_000_000, i64::MAX] {
            for created in [None, Some(i64::MIN), Some(-1), Some(0)] {
                assert!(matches!(
                    resource_expiry(now, created, 60, None),
                    ResourceExpiry::Quarantine(_)
                ));
                assert!(matches!(
                    resource_expiry(now, created, 0, Some(600)),
                    ResourceExpiry::Quarantine(_)
                ));
            }
            assert!(matches!(
                resource_expiry(now, Some(1), u64::MAX, Some(1)),
                ResourceExpiry::Quarantine(_)
            ));
        }
        assert!(matches!(
            resource_expiry(100, Some(101), 1, None),
            ResourceExpiry::Quarantine(_)
        ));
        assert_eq!(
            resource_expiry(100, Some(50), 51, None),
            ResourceExpiry::Keep
        );
        assert_eq!(
            resource_expiry(100, Some(50), 50, None),
            ResourceExpiry::Expired
        );
        assert_eq!(resource_expiry(100, None, 0, None), ResourceExpiry::Keep);
        assert_eq!(
            resource_expiry(100, Some(50), 0, Some(50)),
            ResourceExpiry::Expired
        );
    }

    #[test]
    fn fractional_provider_delays_and_elapsed_extremes_are_checked() {
        assert_eq!(deadline_millis_from_seconds(1000, 0.0001), Some(1001));
        assert_eq!(deadline_millis_from_seconds(1000, 0.5), Some(1500));
        for seconds in [
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            -1.0,
            MAX_DURATION_SECS as f64 + 1.0,
        ] {
            assert_eq!(deadline_millis_from_seconds(1000, seconds), None);
        }
        assert_eq!(deadline_millis(i64::MAX, 1), None);
        assert_eq!(deadline_millis(1000, u64::MAX), None);
        assert!(!elapsed_at_least(0, 0, u64::MAX));
        assert!(!elapsed_at_least(-1, 0, 0));
        assert!(elapsed_at_least(i64::MAX, i64::MIN, u64::MAX));
    }

    #[test]
    fn cadence_floors_caps_and_units_never_produce_zero() {
        for floor in [0, 1, 5, 15, 60, 120, MAX_CADENCE_SECS, u64::MAX] {
            for seconds in [0, 1, 5, 15, 60, MAX_CADENCE_SECS, 1 << 61, u64::MAX] {
                for slot in [0, 1, 500, 1000, u64::MAX] {
                    let got = cadence_slots(seconds, floor, slot).get();
                    assert!(got > 0);
                    assert!(got <= MAX_CADENCE_SECS * 1000);
                }
            }
        }
        assert_eq!(cadence_slots(0, 15, 500).get(), 30);
        assert_eq!(cadence_slots(1, 0, 1500).get(), 1);
        assert_eq!(cadence_slots(2, 0, 1500).get(), 2);
        assert_eq!(cadence_slots(3, 0, 1000).get(), 3);
        assert_eq!(
            cadence_slots(1 << 61, 15, 500),
            cadence_slots(MAX_CADENCE_SECS, 15, 500)
        );
        assert_eq!(cadence_slots(900, 120, 500).get(), 1800);
    }

    #[test]
    fn generated_cadences_are_monotone_and_nonzero() {
        let mut state = 0x853c49e6748fea9bu64;
        for _ in 0..10_000 {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            let a = state;
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            let b = state;
            let (low, high) = (a.min(b), a.max(b));
            assert!(cadence_slots(low, 15, 500) <= cadence_slots(high, 15, 500));
            assert_ne!(cadence_slots(a, 0, b).get(), 0);
        }
    }

    #[test]
    fn unknown_or_invalid_age_never_authorizes_destruction() {
        for now in [i64::MIN, -1, 0, 1, 1000, i64::MAX] {
            for created in [None, Some(i64::MIN), Some(-1), Some(0), Some(i64::MAX)] {
                if created == Some(i64::MAX) && now == i64::MAX {
                    assert_eq!(lifetime_expired(now, created, 1), Some(false));
                } else {
                    assert_eq!(lifetime_expired(now, created, 1), None);
                }
            }
            for lifetime in [
                i64::MAX as u64 - 1,
                i64::MAX as u64,
                i64::MAX as u64 + 1,
                u64::MAX,
            ] {
                assert_eq!(lifetime_expired(now, Some(1), lifetime), None);
            }
        }
        assert_eq!(age_seconds(i64::MAX, 1), Some(i64::MAX as u64 - 1));
        assert_eq!(lifetime_expired(100, Some(50), 0), Some(false));
        assert_eq!(lifetime_expired(100, None, 0), Some(false));
        assert_eq!(lifetime_expired(100, Some(50), 50), Some(true));
        assert_eq!(lifetime_expired(100, Some(50), 51), Some(false));
    }

    #[test]
    fn cache_rollback_and_large_ttl_do_not_force_repolling() {
        assert!(!is_fresh(100, None, 50));
        assert!(!is_fresh(100, Some(0), 50));
        assert!(!is_fresh(100, Some(-1), 50));
        assert!(!is_fresh(100, Some(99), 0));
        assert!(is_fresh(100, Some(200), 1));
        assert!(is_fresh(i64::MIN, Some(i64::MAX), 1));
        assert!(!is_fresh(100, Some(50), 50));
        assert!(is_fresh(100, Some(50), 51));
        for ttl in [
            i64::MAX as u64 - 1,
            i64::MAX as u64,
            i64::MAX as u64 + 1,
            u64::MAX,
        ] {
            assert!(is_fresh(100, Some(1), ttl));
        }
    }

    #[test]
    fn provider_deadlines_reject_overflow_and_unsupported_delays() {
        assert_eq!(deadline_seconds(100, 0), Some(100));
        assert_eq!(deadline_seconds(100, 30), Some(130));
        assert_eq!(
            deadline_seconds(100, MAX_DURATION_SECS),
            Some(100 + MAX_DURATION_SECS as i64)
        );
        assert_eq!(deadline_seconds(-1, 0), None);
        assert_eq!(deadline_seconds(i64::MAX, 1), None);
        assert_eq!(deadline_seconds(100, u64::MAX), None);
    }

    #[test]
    fn milliseconds_never_narrow_to_negative_or_disable_a_window() {
        assert_eq!(duration_millis(0), 0);
        assert_eq!(duration_millis(30), 30_000);
        assert_eq!(saturating_i64(u128::MAX), i64::MAX);
        assert_eq!(saturating_i64(42), 42);
        for seconds in [
            i64::MAX as u64 - 1,
            i64::MAX as u64,
            i64::MAX as u64 + 1,
            u64::MAX,
        ] {
            assert_eq!(duration_millis(seconds), i64::MAX);
        }
    }
}
