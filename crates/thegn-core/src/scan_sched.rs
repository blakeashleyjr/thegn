//! Scheduling policy shared by the two background measurement scans: the
//! per-worktree `du` size scan ([`crate::disk`]) and the per-worktree tokei LOC
//! count. Both answer the same question — given a set of measurable paths, when
//! each was last measured, which one is on screen, a TTL and a per-round budget:
//! what do we measure now, and in what order?
//!
//! That decision is the whole subtlety. The size scan used to walk the registry
//! in `ORDER BY position, created_at`, so a *brand-new* worktree — the one case
//! where a blank badge is actually noticed — was measured **last**, behind every
//! stale multi-GB `du`. Ordering is therefore policy, not an implementation
//! detail, and lives here as a pure function with the host-side runners staying
//! dumb (no tokio, no DB, no subprocess — so it's exhaustively unit-testable).

/// One measurable path plus everything the planner needs to know about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanTarget {
    /// Cache key, which is also the filesystem path.
    pub path: String,
    /// `fetched_at` from the cache; `None` = never measured.
    pub measured_at: Option<i64>,
    /// The worktree currently on screen.
    pub active: bool,
}

impl ScanTarget {
    /// A never-measured target (the freshly-created-worktree case).
    pub fn cold(path: impl Into<String>) -> ScanTarget {
        ScanTarget {
            path: path.into(),
            measured_at: None,
            active: false,
        }
    }

    /// A target last measured at `at`.
    pub fn measured(path: impl Into<String>, at: i64) -> ScanTarget {
        ScanTarget {
            path: path.into(),
            measured_at: Some(at),
            active: false,
        }
    }

    /// Builder: mark this target as the on-screen worktree.
    pub fn active(mut self) -> ScanTarget {
        self.active = true;
        self
    }
}

/// Ordering class, lowest scheduled first. Public so the tests read as a spec.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ScanPriority {
    /// On screen and never measured — "I just created this worktree and I'm
    /// looking right at it". The single most user-visible miss, so: always first.
    ActiveCold,
    /// Never measured. A blank badge is worse than a stale one, so cold beats
    /// stale everywhere — this is what stops a new worktree queueing behind a
    /// dozen multi-GB re-measurements.
    Cold,
    /// On screen and past its TTL.
    ActiveStale,
    /// Past its TTL.
    Stale,
}

/// The class of `t`, or `None` when it is fresh enough to skip this round.
/// `ttl_secs == 0` means nothing is ever fresh (measure every round).
pub fn priority(t: &ScanTarget, now: i64, ttl_secs: u64) -> Option<ScanPriority> {
    match t.measured_at {
        None => Some(if t.active {
            ScanPriority::ActiveCold
        } else {
            ScanPriority::Cold
        }),
        Some(at) => {
            // A stamp in the future (clock skew, a restored DB) counts as fresh
            // rather than wrapping negative and re-measuring forever.
            let age = now.saturating_sub(at);
            if ttl_secs > 0 && age < ttl_secs as i64 {
                return None;
            }
            Some(if t.active {
                ScanPriority::ActiveStale
            } else {
                ScanPriority::Stale
            })
        }
    }
}

/// This round's work, ordered. Fresh targets are dropped; the rest sort by
/// [`ScanPriority`], then oldest `measured_at` first (`None` sorts oldest), then
/// path so the order is deterministic; then the list is truncated to `budget`
/// (`0` = unlimited).
///
/// The budget is what keeps a round from holding its background-lane permit for
/// minutes on a large registry — the *next* pump picks up where this one left
/// off, because everything it skipped is still stale (and now older, so it sorts
/// earlier).
pub fn plan(targets: &[ScanTarget], now: i64, ttl_secs: u64, budget: usize) -> Vec<String> {
    let mut due: Vec<(ScanPriority, i64, &str)> = targets
        .iter()
        .filter_map(|t| {
            priority(t, now, ttl_secs).map(|p| {
                // `None` (never measured) must sort oldest within its class.
                (p, t.measured_at.unwrap_or(i64::MIN), t.path.as_str())
            })
        })
        .collect();
    due.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)).then(a.2.cmp(b.2)));
    if budget > 0 {
        due.truncate(budget);
    }
    due.into_iter().map(|(_, _, p)| p.to_string()).collect()
}

/// Ticker slots (of `slot_ms` each) between scan pumps, for a per-row TTL of
/// `ttl_secs`.
///
/// The pump runs at a **quarter** of the TTL so a budget-bounded round still
/// sweeps the whole registry inside one TTL window. This is what makes a single
/// `scan_interval_secs` key honest: the size scan previously paired a hardcoded
/// 30s ticker with a 45s TTL, so every other pump was a no-op and the effective
/// refresh was 60s — neither of the two numbers a reader would predict.
///
/// Floored at `floor_secs` so a misconfigured `0` can never spin the scanner,
/// and clamped to at least one slot.
pub fn pump_slots(ttl_secs: u64, floor_secs: u64, slot_ms: u64) -> u64 {
    let slot_ms = slot_ms.max(1);
    let secs = (ttl_secs / 4).max(floor_secs);
    ((secs * 1000) / slot_ms).max(1)
}

/// Cache rows whose path has left the live set — the shared shape of both orphan
/// GCs (`worktree_disk` and `loc_cache`). A row for a removed worktree is never
/// re-measured by the scan loop, so without this it would inflate the statusbar
/// total forever.
pub fn orphans<'a, I, L>(cached: I, live: L) -> Vec<String>
where
    I: IntoIterator<Item = &'a str>,
    L: IntoIterator<Item = &'a str>,
{
    let live: std::collections::HashSet<&str> = live.into_iter().collect();
    let mut out: Vec<String> = cached
        .into_iter()
        .filter(|p| !live.contains(p))
        .map(str::to_string)
        .collect();
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const TTL: u64 = 100;
    const NOW: i64 = 1_000_000;

    #[test]
    fn never_measured_beats_stale() {
        let targets = vec![
            ScanTarget::measured("/stale", NOW - 3600),
            ScanTarget::cold("/new"),
        ];
        assert_eq!(plan(&targets, NOW, TTL, 0), vec!["/new", "/stale"]);
    }

    /// The reproduction of the reported bug: a dozen stale multi-GB worktrees
    /// plus one brand-new one the user is looking at. The new one must be first,
    /// not last.
    #[test]
    fn active_cold_is_always_first() {
        let mut targets: Vec<ScanTarget> = (0..12)
            .map(|i| ScanTarget::measured(format!("/old{i:02}"), NOW - 3600 - i))
            .collect();
        targets.push(ScanTarget::cold("/brand-new").active());
        let order = plan(&targets, NOW, TTL, 0);
        assert_eq!(order[0], "/brand-new");
        assert_eq!(order.len(), 13);
    }

    #[test]
    fn priority_classes_are_exhaustive_and_ordered() {
        let cold_active = ScanTarget::cold("/a").active();
        let cold = ScanTarget::cold("/b");
        let stale_active = ScanTarget::measured("/c", NOW - TTL as i64).active();
        let stale = ScanTarget::measured("/d", NOW - TTL as i64);
        assert_eq!(
            priority(&cold_active, NOW, TTL),
            Some(ScanPriority::ActiveCold)
        );
        assert_eq!(priority(&cold, NOW, TTL), Some(ScanPriority::Cold));
        assert_eq!(
            priority(&stale_active, NOW, TTL),
            Some(ScanPriority::ActiveStale)
        );
        assert_eq!(priority(&stale, NOW, TTL), Some(ScanPriority::Stale));
        assert!(ScanPriority::ActiveCold < ScanPriority::Cold);
        assert!(ScanPriority::Cold < ScanPriority::ActiveStale);
        assert!(ScanPriority::ActiveStale < ScanPriority::Stale);
    }

    #[test]
    fn fresh_targets_are_dropped() {
        let fresh = ScanTarget::measured("/fresh", NOW - 1);
        assert_eq!(priority(&fresh, NOW, TTL), None);
        assert!(plan(std::slice::from_ref(&fresh), NOW, TTL, 0).is_empty());
        // ttl 0 = nothing is ever fresh.
        assert_eq!(priority(&fresh, NOW, 0), Some(ScanPriority::Stale));
        assert_eq!(plan(&[fresh], NOW, 0, 0), vec!["/fresh"]);
    }

    #[test]
    fn exactly_at_the_ttl_boundary_is_stale() {
        let t = ScanTarget::measured("/x", NOW - TTL as i64);
        assert_eq!(priority(&t, NOW, TTL), Some(ScanPriority::Stale));
        let t = ScanTarget::measured("/x", NOW - TTL as i64 + 1);
        assert_eq!(priority(&t, NOW, TTL), None);
    }

    /// A stamp in the future (clock skew, a DB copied from another machine)
    /// must read as fresh, not wrap negative and re-measure every round.
    #[test]
    fn future_stamps_count_as_fresh() {
        let t = ScanTarget::measured("/skewed", NOW + 5_000);
        assert_eq!(priority(&t, NOW, TTL), None);
    }

    #[test]
    fn budget_truncates_but_keeps_priority_order() {
        let targets = vec![
            ScanTarget::measured("/stale", NOW - 3600),
            ScanTarget::cold("/cold"),
            ScanTarget::cold("/active-cold").active(),
        ];
        assert_eq!(plan(&targets, NOW, TTL, 2), vec!["/active-cold", "/cold"]);
        assert_eq!(plan(&targets, NOW, TTL, 1), vec!["/active-cold"]);
        // 0 = unlimited.
        assert_eq!(plan(&targets, NOW, TTL, 0).len(), 3);
    }

    #[test]
    fn oldest_first_within_a_class() {
        let targets = vec![
            ScanTarget::measured("/recent", NOW - 200),
            ScanTarget::measured("/ancient", NOW - 9000),
            ScanTarget::measured("/middle", NOW - 1000),
        ];
        assert_eq!(
            plan(&targets, NOW, TTL, 0),
            vec!["/ancient", "/middle", "/recent"]
        );
    }

    #[test]
    fn order_is_deterministic_for_equal_stamps() {
        let targets = vec![
            ScanTarget::measured("/b", NOW - 500),
            ScanTarget::measured("/a", NOW - 500),
            ScanTarget::measured("/c", NOW - 500),
        ];
        assert_eq!(plan(&targets, NOW, TTL, 0), vec!["/a", "/b", "/c"]);
    }

    #[test]
    fn empty_input_plans_nothing() {
        assert!(plan(&[], NOW, TTL, 4).is_empty());
    }

    #[test]
    fn pump_slots_is_a_quarter_of_the_ttl_and_floored() {
        // The disk default: 45s TTL → 11s, floored to 15s → 30 slots of 500ms.
        assert_eq!(pump_slots(45, 15, 500), 30);
        // The loc default: 900s TTL → 225s → 450 slots.
        assert_eq!(pump_slots(900, 60, 500), 450);
        // A large TTL is genuinely a quarter.
        assert_eq!(pump_slots(1200, 15, 500), 600);
        // A misconfigured 0 falls back to the floor, never to 0 slots.
        assert_eq!(pump_slots(0, 15, 500), 30);
        assert!(pump_slots(0, 0, 500) >= 1);
        // Never zero, whatever the slot size.
        assert!(pump_slots(1, 0, 60_000) >= 1);
    }

    #[test]
    fn orphans_returns_only_paths_absent_from_live() {
        let cached = ["/a", "/b", "/gone", "/also-gone"];
        let live = ["/a", "/b", "/never-cached"];
        assert_eq!(
            orphans(cached, live),
            vec!["/also-gone".to_string(), "/gone".to_string()]
        );
    }

    #[test]
    fn orphans_of_an_empty_live_set_is_everything() {
        assert_eq!(
            orphans(["/a", "/b"], std::iter::empty()),
            vec!["/a".to_string(), "/b".to_string()]
        );
        assert!(orphans(std::iter::empty(), ["/a"]).is_empty());
    }

    #[test]
    fn orphans_dedupes_repeated_cache_keys() {
        assert_eq!(orphans(["/x", "/x"], ["/y"]), vec!["/x".to_string()]);
    }
}
