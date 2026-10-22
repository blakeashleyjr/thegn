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

/// A completed physical collection with deliberately unfinished ref cleanup.
/// Stored as an exact reserved error_detail value, not a new queue status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleanupHold {
    BranchRetained,
}
impl CleanupHold {
    pub const fn marker(self) -> &'static str {
        "thegn-cleanup-hold:v1:branch-retained"
    }
    pub fn from_detail(detail: Option<&str>) -> Option<Self> {
        (detail == Some(Self::BranchRetained.marker())).then_some(Self::BranchRetained)
    }
}

pub const CLEANUP_RETRY_BASE_SECS: u64 = 300;
pub const CLEANUP_RETRY_MAX_SECS: u64 = 86_400;

/// Retry timing for a refused landed-worktree cleanup, stored in `error_detail`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CleanupRetry {
    pub attempts: u32,
    pub next_at: i64,
}

impl CleanupRetry {
    pub fn marker(self) -> String {
        format!("thegn-cleanup-retry:v1:{}:{}", self.attempts, self.next_at)
    }

    /// Parse only the reserved marker's exact two-field decimal representation.
    pub fn from_detail(detail: Option<&str>) -> Option<Self> {
        let fields = detail?.strip_prefix("thegn-cleanup-retry:v1:")?;
        let (attempts, next_at) = fields.split_once(':')?;
        if attempts.is_empty()
            || next_at.is_empty()
            || !attempts.bytes().all(|byte| byte.is_ascii_digit())
            || !next_at.bytes().all(|byte| byte.is_ascii_digit())
        {
            return None;
        }
        let attempts = attempts.parse::<u32>().ok()?;
        let next_at = next_at.parse::<i64>().ok()?;
        (attempts >= 1 && next_at >= 0).then_some(Self { attempts, next_at })
    }

    pub fn after_refusal(previous: Option<Self>, now: i64) -> Self {
        let attempts = previous.map_or(1, |retry| retry.attempts.saturating_add(1));
        let shift = attempts.saturating_sub(1);
        let delay = CLEANUP_RETRY_BASE_SECS
            .checked_shl(shift)
            .unwrap_or(CLEANUP_RETRY_MAX_SECS)
            .min(CLEANUP_RETRY_MAX_SECS);
        Self {
            attempts,
            next_at: now.saturating_add(i64::try_from(delay).unwrap_or(i64::MAX)),
        }
    }

    pub const fn is_due(self, now: i64) -> bool {
        now >= self.next_at
    }
}

/// Human-readable rendering for reserved cleanup metadata, if `detail` is ours.
pub fn describe_reserved_detail(detail: &str, now: i64) -> Option<String> {
    if CleanupHold::from_detail(Some(detail)).is_some() {
        return Some("cleanup held: branch retained for explicit cleanup".into());
    }
    let retry = CleanupRetry::from_detail(Some(detail))?;
    let next = if retry.is_due(now) {
        "next retry due now".to_string()
    } else {
        format!(
            "next retry in {}",
            humanize_remaining(retry.next_at.saturating_sub(now) as u64)
        )
    };
    Some(format!(
        "cleanup refused {} time(s); {next}",
        retry.attempts
    ))
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

    #[test]
    fn cleanup_retry_marker_round_trips() {
        let retry = CleanupRetry {
            attempts: 3,
            next_at: 1_234_567,
        };
        assert_eq!(retry.marker(), "thegn-cleanup-retry:v1:3:1234567");
        assert_eq!(
            CleanupRetry::from_detail(Some(&retry.marker())),
            Some(retry)
        );
    }

    #[test]
    fn cleanup_retry_parser_rejects_malformed_details() {
        for detail in [
            "other:v1:1:2",
            "thegn-cleanup-retry:v2:1:2",
            "thegn-cleanup-retry:v1:0:2",
            "thegn-cleanup-retry:v1:+1:2",
            "thegn-cleanup-retry:v1:1:+2",
            "thegn-cleanup-retry:v1:1: 2",
            "thegn-cleanup-retry:v1:1:2:3",
            "thegn-cleanup-retry:v1:1",
            "thegn-cleanup-retry:v1:١:2",
            "thegn-cleanup-retry:v1:4294967296:2",
            CleanupHold::BranchRetained.marker(),
            "",
        ] {
            assert_eq!(CleanupRetry::from_detail(Some(detail)), None, "{detail:?}");
        }
        assert_eq!(CleanupRetry::from_detail(None), None);
        assert_eq!(
            CleanupRetry::from_detail(Some("thegn-cleanup-retry:v1:1:-1")),
            None
        );
        assert_eq!(
            CleanupRetry::from_detail(Some("thegn-cleanup-retry:v1:1:9223372036854775808")),
            None
        );
    }

    #[test]
    fn cleanup_retry_schedule_doubles_and_caps() {
        let mut previous = None;
        for (attempts, delay) in [
            300, 600, 1_200, 2_400, 4_800, 9_600, 19_200, 38_400, 76_800, 86_400, 86_400, 86_400,
        ]
        .into_iter()
        .enumerate()
        {
            let retry = CleanupRetry::after_refusal(previous, 10_000);
            assert_eq!(retry.attempts, u32::try_from(attempts + 1).unwrap());
            assert_eq!(retry.next_at, 10_000 + delay);
            previous = Some(retry);
        }
        let saturated = CleanupRetry::after_refusal(
            Some(CleanupRetry {
                attempts: u32::MAX,
                next_at: 0,
            }),
            0,
        );
        assert_eq!(saturated.attempts, u32::MAX);
        assert_eq!(
            saturated.next_at,
            i64::try_from(CLEANUP_RETRY_MAX_SECS).unwrap()
        );
    }

    #[test]
    fn cleanup_retry_due_boundary() {
        let retry = CleanupRetry {
            attempts: 1,
            next_at: 123,
        };
        assert!(!retry.is_due(122));
        assert!(retry.is_due(123));
        assert!(retry.is_due(124));
    }

    #[test]
    fn cleanup_retry_describes_reserved_details() {
        let hold = CleanupHold::BranchRetained.marker();
        assert_eq!(
            describe_reserved_detail(hold, 0).as_deref(),
            Some("cleanup held: branch retained for explicit cleanup")
        );
        let retry = CleanupRetry {
            attempts: 2,
            next_at: 1_060,
        };
        assert_eq!(
            describe_reserved_detail(&retry.marker(), 1_000).as_deref(),
            Some("cleanup refused 2 time(s); next retry in 1m")
        );
        assert_eq!(
            describe_reserved_detail(&retry.marker(), 1_060).as_deref(),
            Some("cleanup refused 2 time(s); next retry due now")
        );
        assert_eq!(describe_reserved_detail("ordinary detail", 0), None);
    }
}
