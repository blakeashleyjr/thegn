//! When a landed worktree's grace period is up.
//!
//! Under `[merge_queue] on_landed = "expire"` a branch that lands keeps its
//! worktree, filed into `merged_folder`, and is collected later. This module owns
//! the "later": the arithmetic is pure and lives here so the rule that decides
//! whether a directory gets deleted is unit-testable, while the host module of
//! the same name does the removal.
//!
//! The clock is the queue row's `updated_at`, stamped when the row moved to
//! `landed`. Reusing it means there is no second timestamp to drift out of step
//! with the first.

/// A landed worktree the sweep may collect, projected from its `merge_queue` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergedEntry {
    pub worktree: String,
    pub branch: String,
    /// Unix seconds the row moved to `landed` (its `updated_at`).
    pub landed_at: i64,
}

/// Seconds still to run on `landed_at`'s grace period, or `None` once it is up.
///
/// A `landed_at` in the FUTURE returns the full ttl rather than a negative or
/// saturating-zero age: a clock that jumped backwards (suspend, NTP step, a DB
/// written by another machine) must not be read as "infinitely overdue" and
/// collect everything at once. Erring long only ever delays a deletion.
pub fn remaining_secs(landed_at: i64, now: i64, ttl_secs: u64) -> Option<u64> {
    if ttl_secs == 0 {
        return Some(u64::MAX);
    }
    let age = now.saturating_sub(landed_at);
    if age < 0 {
        return Some(ttl_secs);
    }
    let age = age.unsigned_abs();
    ttl_secs.checked_sub(age).filter(|&r| r > 0)
}

/// Whether `landed_at`'s grace period has fully elapsed.
pub fn is_due(landed_at: i64, now: i64, ttl_secs: u64) -> bool {
    remaining_secs(landed_at, now, ttl_secs).is_none()
}

/// The landed entries whose grace period is up, in input order.
///
/// `ttl_secs == 0` means "never expire" — the `"move"` behavior — so it yields
/// nothing rather than everything. That asymmetry is deliberate: zero is the
/// value a half-written config is most likely to hold, and the safe reading of
/// an ambiguous retention setting is to keep the data.
pub fn due(entries: &[MergedEntry], now: i64, ttl_secs: u64) -> Vec<&MergedEntry> {
    if ttl_secs == 0 {
        return Vec::new();
    }
    entries
        .iter()
        .filter(|e| is_due(e.landed_at, now, ttl_secs))
        .collect()
}

/// Compact "3d" / "4h" / "12m" / "30s" for the longest whole unit remaining —
/// the sidebar and panel show this next to a merged worktree, so it has to fit a
/// chip. Rounds DOWN, so "1d" never means "in a moment".
pub fn humanize_remaining(secs: u64) -> String {
    const MIN: u64 = 60;
    const HOUR: u64 = 60 * MIN;
    const DAY: u64 = 24 * HOUR;
    match secs {
        s if s >= DAY => format!("{}d", s / DAY),
        s if s >= HOUR => format!("{}h", s / HOUR),
        s if s >= MIN => format!("{}m", s / MIN),
        s => format!("{s}s"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WEEK: u64 = 7 * 24 * 60 * 60;

    fn e(name: &str, landed_at: i64) -> MergedEntry {
        MergedEntry {
            worktree: format!("/wt/{name}"),
            branch: name.to_string(),
            landed_at,
        }
    }

    #[test]
    fn nothing_is_due_before_the_ttl_elapses() {
        let now = 1_000_000;
        assert!(!is_due(now - 1, now, WEEK));
        assert!(!is_due(now - (WEEK as i64 - 1), now, WEEK));
    }

    #[test]
    fn the_boundary_is_inclusive() {
        let now = 1_000_000;
        assert!(
            is_due(now - WEEK as i64, now, WEEK),
            "exactly one ttl old is up"
        );
    }

    /// `0` is "never expire", not "expire everything" — the reading that keeps
    /// data when the setting is ambiguous.
    #[test]
    fn a_zero_ttl_never_collects() {
        let now = 1_000_000;
        assert!(!is_due(0, now, 0), "ancient entry, ttl 0 ⇒ not due");
        assert!(due(&[e("old", 0)], now, 0).is_empty());
        assert_eq!(remaining_secs(0, now, 0), Some(u64::MAX));
    }

    /// A backwards clock step must not read as "infinitely overdue" and delete
    /// every merged worktree at once.
    #[test]
    fn a_future_timestamp_is_never_due() {
        let now = 1_000_000;
        assert!(!is_due(now + 5_000, now, WEEK));
        assert_eq!(remaining_secs(now + 5_000, now, WEEK), Some(WEEK));
    }

    #[test]
    fn due_selects_only_the_expired_and_keeps_order() {
        let now = 1_000_000;
        let entries = vec![
            e("fresh", now - 60),
            e("stale", now - 2 * WEEK as i64),
            e("edge", now - WEEK as i64),
            e("alsofresh", now - (WEEK as i64 / 2)),
        ];
        let got: Vec<&str> = due(&entries, now, WEEK)
            .iter()
            .map(|x| x.branch.as_str())
            .collect();
        assert_eq!(got, ["stale", "edge"]);
    }

    #[test]
    fn remaining_counts_down() {
        let now = 1_000_000;
        assert_eq!(remaining_secs(now, now, WEEK), Some(WEEK));
        assert_eq!(remaining_secs(now - 100, now, WEEK), Some(WEEK - 100));
        assert_eq!(remaining_secs(now - WEEK as i64, now, WEEK), None);
        assert_eq!(remaining_secs(now - 10 * WEEK as i64, now, WEEK), None);
    }

    #[test]
    fn humanize_takes_the_largest_whole_unit_and_rounds_down() {
        assert_eq!(humanize_remaining(0), "0s");
        assert_eq!(humanize_remaining(45), "45s");
        assert_eq!(humanize_remaining(60), "1m");
        assert_eq!(humanize_remaining(3600), "1h");
        assert_eq!(humanize_remaining(24 * 3600), "1d");
        assert_eq!(humanize_remaining(WEEK), "7d");
        // Rounds down: just shy of a day must not claim a whole day.
        assert_eq!(humanize_remaining(24 * 3600 - 1), "23h");
    }
}
