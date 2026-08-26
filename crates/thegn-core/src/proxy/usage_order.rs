//! Usage-aware lane ordering.
//!
//! An opt-in input to the cascade (`usage_aware = true`): when the `[usage]`
//! tracker reports that a lane's provider account is near its window cap, that
//! lane should yield to a fresher peer. This is a pure reordering over a
//! snapshot the shell already gathers — the proxy never fetches quota itself.
//!
//! Rule (spec: "respects account headroom without refusing service"):
//! - a lane whose account is **past crit** is skipped,
//! - a lane whose account is **past warn** (but under crit) is deprioritized
//!   (kept, but ordered after every fresh lane),
//! - **at least one lane always survives**: if every lane is past crit, ordering
//!   degrades to the plain strategy order rather than refusing the request.
//!
//! Ordering is stable within each bucket, so the underlying strategy order (and
//! therefore determinism) is preserved.

/// Reorders lane indices `0..used.len()` by account headroom.
///
/// `used[i]` is lane `i`'s provider-account peak usage as a percentage (0–100),
/// or `None` when unknown (treated as fresh). `warn`/`crit` are the percentage
/// thresholds (typically the `[usage.alerts]` values). Returns the surviving
/// lane indices in attempt order; the returned list is non-empty whenever
/// `used` is non-empty.
pub fn usage_order(used: &[Option<f32>], warn: f32, crit: f32) -> Vec<usize> {
    let n = used.len();
    if n == 0 {
        return Vec::new();
    }
    let mut fresh = Vec::new();
    let mut warned = Vec::new();
    for (i, u) in used.iter().enumerate() {
        match u {
            Some(p) if *p >= crit => {} // skipped
            Some(p) if *p >= warn => warned.push(i),
            _ => fresh.push(i),
        }
    }
    if fresh.is_empty() && warned.is_empty() {
        // Every lane is past crit — degrade to plain order so the request is
        // still served (never refuse purely on account headroom).
        return (0..n).collect();
    }
    fresh.extend(warned);
    fresh
}

#[cfg(test)]
mod tests {
    use super::*;

    const WARN: f32 = 75.0;
    const CRIT: f32 = 90.0;

    #[test]
    fn empty_is_empty() {
        assert!(usage_order(&[], WARN, CRIT).is_empty());
    }

    #[test]
    fn all_fresh_keeps_order() {
        let used = [Some(10.0), None, Some(50.0)];
        assert_eq!(usage_order(&used, WARN, CRIT), vec![0, 1, 2]);
    }

    #[test]
    fn warned_lane_is_deprioritized() {
        // lane 0 is past warn, lane 1 fresh → fresh first.
        let used = [Some(80.0), Some(10.0)];
        assert_eq!(usage_order(&used, WARN, CRIT), vec![1, 0]);
    }

    #[test]
    fn crit_lane_is_skipped() {
        // lane 0 past crit → dropped; lane 1 survives.
        let used = [Some(95.0), Some(10.0)];
        assert_eq!(usage_order(&used, WARN, CRIT), vec![1]);
    }

    #[test]
    fn fresh_before_warned_before_dropped_crit() {
        let used = [Some(95.0), Some(80.0), Some(5.0), Some(85.0)];
        // fresh: [2]; warned (in order): [1, 3]; crit [0] dropped.
        assert_eq!(usage_order(&used, WARN, CRIT), vec![2, 1, 3]);
    }

    #[test]
    fn all_throttled_degrades_to_plain_order() {
        let used = [Some(99.0), Some(91.0), Some(90.0)];
        // All past crit → plain order, nothing refused.
        assert_eq!(usage_order(&used, WARN, CRIT), vec![0, 1, 2]);
    }

    #[test]
    fn nearly_exhausted_yields_to_fresh_peer() {
        // The spec scenario: first lane past warn, peer fresh → peer first.
        let used = [Some(88.0), Some(12.0)];
        assert_eq!(usage_order(&used, WARN, CRIT)[0], 1);
    }

    #[test]
    fn boundary_values() {
        // Exactly at warn → warned; exactly at crit → skipped.
        let used = [Some(75.0), Some(90.0), Some(74.9)];
        // warned: [0]; crit [1] dropped; fresh [2].
        assert_eq!(usage_order(&used, WARN, CRIT), vec![2, 0]);
    }

    #[test]
    fn stable_within_bucket() {
        let used = [Some(80.0), Some(81.0), Some(82.0)];
        // All warned, none fresh, none crit → kept in original order.
        assert_eq!(usage_order(&used, WARN, CRIT), vec![0, 1, 2]);
    }
}
