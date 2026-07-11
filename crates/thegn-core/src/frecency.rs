//! Frecency — a combined frequency × recency score for ranking navigation
//! targets (workspaces, worktrees, palette entries).
//!
//! The classic curve: `score = count * 2^(-age / half_life)`. A frequently
//! *and* recently used entry outranks both a stale heavy-hitter and a
//! once-touched newcomer. Pure and substrate-free: callers feed it the
//! `(count, last_used)` pairs already persisted in the `palette_usage` /
//! `repos` tables (epoch seconds, `util::now`) and get a rank back — no new
//! persistence, ranking is read-time.

/// Decay half-life: a use loses half its weight every 7 days.
pub const HALF_LIFE_SECS: f64 = 7.0 * 24.0 * 3600.0;

/// Frecency score for an entry used `count` times, most recently at
/// `last_used` (epoch seconds), evaluated at `now`. A zero/negative age
/// (clock skew, same-second use) is clamped to "just used" — the score is
/// then exactly `count`. Never panics; never returns NaN.
pub fn score(count: i64, last_used: i64, now: i64) -> f64 {
    let count = count.max(0) as f64;
    let age = (now.saturating_sub(last_used)).max(0) as f64;
    count * (0.5_f64).powf(age / HALF_LIFE_SECS)
}

/// Rank `(value, count, last_used)` entries by frecency score, best first.
/// The sort is stable: entries with equal scores (e.g. all-zero usage) keep
/// their input order, so an empty usage history degrades to the caller's
/// existing (recency) order without error.
pub fn rank<T>(entries: Vec<(T, i64, i64)>, now: i64) -> Vec<T> {
    let mut scored: Vec<(f64, T)> = entries
        .into_iter()
        .map(|(v, count, last_used)| (score(count, last_used, now), v))
        .collect();
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    scored.into_iter().map(|(_, v)| v).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = 24 * 3600;

    #[test]
    fn more_recent_wins_at_equal_count() {
        let now = 100 * DAY;
        let fresh = score(5, now - DAY, now);
        let stale = score(5, now - 30 * DAY, now);
        assert!(fresh > stale, "{fresh} vs {stale}");
    }

    #[test]
    fn higher_count_wins_at_equal_age() {
        let now = 100 * DAY;
        let heavy = score(10, now - 3 * DAY, now);
        let light = score(2, now - 3 * DAY, now);
        assert!(heavy > light, "{heavy} vs {light}");
    }

    #[test]
    fn zero_or_negative_delta_does_not_panic() {
        let s = score(4, 100, 100);
        assert!((s - 4.0).abs() < f64::EPSILON, "zero age = full weight");
        // last_used in the future (clock skew) clamps to "just used".
        let s = score(4, 200, 100);
        assert!((s - 4.0).abs() < f64::EPSILON);
        // Degenerate inputs stay finite.
        assert!(score(0, 0, 0).is_finite());
        assert!(score(-3, i64::MIN, i64::MAX).is_finite());
        assert!(score(i64::MAX, i64::MIN, i64::MAX) >= 0.0);
    }

    #[test]
    fn half_life_halves_the_weight() {
        let now = 100 * DAY;
        let s = score(8, now - 7 * DAY, now);
        assert!((s - 4.0).abs() < 1e-9, "7d = one half-life: {s}");
    }

    #[test]
    fn rank_orders_best_first() {
        let now = 100 * DAY;
        let entries = vec![
            ("stale-heavy", 20, now - 60 * DAY),
            ("fresh-light", 3, now - DAY),
            ("fresh-heavy", 10, now - DAY),
        ];
        let ranked = rank(entries, now);
        assert_eq!(ranked, vec!["fresh-heavy", "fresh-light", "stale-heavy"]);
    }

    #[test]
    fn rank_is_stable_on_ties() {
        // All-zero usage (empty history): input order is preserved — the
        // caller's recency fallback survives the ranking pass.
        let entries = vec![("a", 0, 0), ("b", 0, 0), ("c", 0, 0)];
        assert_eq!(rank(entries, 1000), vec!["a", "b", "c"]);
    }
}
