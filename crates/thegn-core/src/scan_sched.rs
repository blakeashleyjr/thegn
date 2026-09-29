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
            if crate::time_policy::is_fresh(now, Some(at), ttl_secs) {
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

/// How many pumps fire inside one per-row TTL window.
///
/// [`pump_slots`] deliberately runs the pump at a **quarter** of the TTL, so a
/// budget-bounded round still sweeps the whole registry inside one TTL window.
/// This constant is the other half of that sentence, named so the assumption can
/// be checked instead of merely documented.
pub const PUMPS_PER_TTL: usize = 4;

/// Whether a lane can ever finish sweeping its registry inside one TTL window.
///
/// # The failure this exists to name
///
/// Both scan lanes document the same assumption — the pump runs at a quarter of
/// the TTL "so a budget-bounded round still sweeps every worktree inside one
/// window". That is true only while `rows <= (PUMPS_PER_TTL - 1) * budget` —
/// see [`saturation`] for why it is `- 1`. Past that point every row is stale
/// again before the sweep reaches it, so [`plan`]
/// always returns work, the lane **never reaches an idle round**, and it keeps
/// running for the life of the process.
///
/// Measured consequence, on the machine this was diagnosed on: the size lane
/// ships `max_scan_per_round = 4`, giving a ceiling of **12** worktrees. That
/// machine had 82 — seven times over. It ran flat out for three days at 26,000 read
/// syscalls a second — not because any single round was slow, but because there
/// was never a round with nothing due.
///
/// Note what is NOT a factor: the TTL itself cancels out, because the pump
/// cadence is derived from it. Lengthening `scan_interval_secs` does not raise
/// the ceiling — only the per-round budget does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Saturation {
    /// Every row can be refreshed inside one TTL window.
    Sustainable {
        /// How many more rows this lane could take before saturating.
        spare_rows: usize,
    },
    /// The registry is larger than one TTL window's capacity, so the lane can
    /// never idle.
    Saturated {
        /// Rows the lane is responsible for.
        rows: usize,
        /// Rows it can actually refresh inside one TTL window.
        capacity: usize,
    },
}

impl Saturation {
    /// Whether this lane can ever reach a round with nothing due.
    pub fn can_idle(self) -> bool {
        matches!(self, Self::Sustainable { .. })
    }

    /// One operator-facing line naming the remedy, or `None` when healthy.
    /// The remedy is always the budget — never the TTL (see [`Saturation`]).
    pub fn warning(self) -> Option<String> {
        match self {
            Self::Sustainable { .. } => None,
            Self::Saturated { rows, capacity } => Some(format!(
                "this lane tracks {rows} rows but can only refresh {capacity} within one                  scan_interval_secs window, so it will never go idle — raise                  max_scan_per_round to at least {} (lengthening scan_interval_secs does                  NOT help: the pump cadence is derived from it)",
                rows.div_ceil(PUMPS_PER_TTL - 1).max(1)
            )),
        }
    }
}

/// Classify a lane's steady state. `budget == 0` is unlimited, so it can always
/// idle.
pub fn saturation(rows: usize, budget: usize) -> Saturation {
    if budget == 0 {
        return Saturation::Sustainable {
            spare_rows: usize::MAX,
        };
    }
    // `PUMPS_PER_TTL - 1`, not `PUMPS_PER_TTL`, and the off-by-one IS the
    // subtlety. The sweep must COMPLETE before the first row it measured
    // expires. A sweep of `ceil(rows/budget)` pumps finishes at
    // `ceil(rows/budget) * (ttl/PUMPS_PER_TTL)`; for the NEXT pump to find
    // nothing due that must be strictly less than the TTL, so
    // `ceil(rows/budget) < PUMPS_PER_TTL` — i.e. `rows <= budget * (PUMPS_PER_TTL-1)`.
    //
    // At exactly `budget * PUMPS_PER_TTL` the sweep lands ON the TTL boundary
    // and the first batch is stale again on the same tick, so the lane misses
    // idle by one pump. Found by the control test
    // `a_lane_within_its_budget_reaches_an_idle_round`, which failed against the
    // naive `budget * PUMPS_PER_TTL` model.
    let capacity = budget.saturating_mul(PUMPS_PER_TTL - 1);
    if rows <= capacity {
        Saturation::Sustainable {
            spare_rows: capacity - rows,
        }
    } else {
        Saturation::Saturated { rows, capacity }
    }
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
    crate::time_policy::cadence_slots(ttl_secs / 4, floor_secs, slot_ms).get()
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

    /// Simulate a lane: repeatedly `plan` a round, mark those rows measured, and
    /// advance one pump. Returns the pump index at which nothing was due, or
    /// `None` if it never idled within `max_pumps`.
    fn pumps_until_idle(
        rows: usize,
        ttl_secs: u64,
        budget: usize,
        max_pumps: usize,
    ) -> Option<usize> {
        let mut targets: Vec<ScanTarget> = (0..rows)
            .map(|i| ScanTarget::cold(format!("/wt/{i:04}")))
            .collect();
        let pump_secs = (ttl_secs / PUMPS_PER_TTL as u64).max(1) as i64;
        let mut now = NOW;
        for pump in 0..max_pumps {
            let due = plan(&targets, now, ttl_secs, budget);
            if due.is_empty() {
                return Some(pump);
            }
            for path in &due {
                if let Some(t) = targets.iter_mut().find(|t| &t.path == path) {
                    t.measured_at = Some(now);
                }
            }
            now += pump_secs;
        }
        None
    }

    #[test]
    fn a_lane_within_its_budget_reaches_an_idle_round() {
        // The healthy case, and the control for the test below: at or under
        // capacity the sweep completes strictly inside one window.
        let budget = 4;
        let rows = (PUMPS_PER_TTL - 1) * budget; // 12 — see `saturation`
        assert!(saturation(rows, budget).can_idle());
        assert!(
            pumps_until_idle(rows, 900, budget, 64).is_some(),
            "a lane inside its budget must reach a round with nothing due"
        );
    }

    #[test]
    fn the_shipped_size_lane_defaults_saturate_at_a_realistic_worktree_count() {
        // THE BUG, as arithmetic. The size lane ships max_scan_per_round = 4,
        // so it can refresh 16 rows per TTL window. A developer with 100
        // worktrees is therefore 6x over capacity: every row is stale again
        // before the sweep reaches it, `plan` always returns work, and the lane
        // runs forever. This is what burned 5d18h of CPU in 3 days.
        const SHIPPED_BUDGET: usize = 4;
        const REALISTIC_ROWS: usize = 100;

        let s = saturation(REALISTIC_ROWS, SHIPPED_BUDGET);
        assert!(
            !s.can_idle(),
            "if this passes, the defaults were raised and this test should be              updated to the new ceiling rather than deleted"
        );
        match s {
            Saturation::Saturated { rows, capacity } => {
                assert_eq!(rows, REALISTIC_ROWS);
                assert_eq!(capacity, (PUMPS_PER_TTL - 1) * SHIPPED_BUDGET);
            }
            Saturation::Sustainable { .. } => unreachable!("asserted above"),
        }
        // And the simulation agrees: it never idles, however long we wait.
        assert!(
            pumps_until_idle(REALISTIC_ROWS, 45, SHIPPED_BUDGET, 500).is_none(),
            "a saturated lane must never reach an idle round — if it does, the              capacity model here is wrong"
        );
        // The warning names the budget, and a number that actually works.
        let w = s.warning().expect("a saturated lane must warn");
        assert!(w.contains("max_scan_per_round"), "{w}");
        assert!(w.contains("never go idle"), "{w}");
        assert!(
            saturation(REALISTIC_ROWS, REALISTIC_ROWS.div_ceil(PUMPS_PER_TTL - 1)).can_idle(),
            "the budget the warning recommends must actually be sufficient"
        );
    }

    #[test]
    fn the_capacity_boundary_is_exact_in_both_directions() {
        // The off-by-one is the easiest thing here to get wrong, so pin both
        // sides: at capacity the lane idles, one row over it never does. Without
        // the upper assertion `saturation` could be quietly more pessimistic
        // than `plan` really is, and we would recommend budgets nobody needs.
        let budget = 4;
        let cap = (PUMPS_PER_TTL - 1) * budget;
        assert!(saturation(cap, budget).can_idle());
        assert!(
            pumps_until_idle(cap, 900, budget, 64).is_some(),
            "a lane at exactly capacity must reach an idle round"
        );
        assert!(!saturation(cap + 1, budget).can_idle());
        assert!(
            pumps_until_idle(cap + 1, 900, budget, 400).is_none(),
            "one row over capacity must never idle"
        );
    }

    #[test]
    fn lengthening_the_ttl_does_not_raise_the_ceiling() {
        // The intuitive fix, which does not work: the pump cadence is derived
        // from the TTL, so a longer TTL means proportionally fewer pumps and
        // exactly the same rows-per-window. Only the budget moves the ceiling.
        for ttl in [45_u64, 600, 3_600, 86_400] {
            assert!(
                pumps_until_idle(100, ttl, 4, 400).is_none(),
                "ttl={ttl} unexpectedly let a 100-row lane idle at budget 4"
            );
        }
        assert!(
            pumps_until_idle(100, 45, 25, 400).is_some(),
            "budget 25 should suffice"
        );
    }

    #[test]
    fn an_unlimited_budget_always_idles() {
        assert!(saturation(10_000, 0).can_idle());
        assert!(saturation(10_000, 0).warning().is_none());
        assert_eq!(pumps_until_idle(50, 900, 0, 8), Some(1));
    }

    #[test]
    fn saturation_reports_headroom_and_does_not_overflow() {
        match saturation(10, 4) {
            // capacity = (4-1)*4 = 12, so 10 rows leaves 2 spare.
            Saturation::Sustainable { spare_rows } => assert_eq!(spare_rows, 2),
            other => panic!("expected Sustainable, got {other:?}"),
        }
        // A pathological budget must not overflow into a false Saturated.
        assert!(saturation(1, usize::MAX).can_idle());
        // Zero rows is trivially sustainable.
        assert!(saturation(0, 1).can_idle());
    }

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
