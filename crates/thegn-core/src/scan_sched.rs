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
    #[must_use]
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

/// How many rows a round may **examine** for each unit of **walk** budget.
///
/// A lane whose rows can be cheaply proved unchanged has two different costs,
/// and conflating them is what kept the size lane pinned. `max_scan_per_round`
/// exists to bound the expensive thing — one `du` over a worktree with every
/// ignore rule off, which on a build tree is gigabytes. But once
/// [`crate::disk::likely_unchanged_since`] landed, most rows became *skips*
/// costing six `stat`s, and a skip still spent a whole unit of that budget.
///
/// Measured consequence: 83 worktrees against a capacity of
/// `(PUMPS_PER_TTL - 1) * 4 = 12`, so 82 of 83 rows re-stamped every five
/// minutes — the same cadence as during the incident — even though almost every
/// one of those rounds did no real work.
///
/// So the two budgets are separated. `max_scan_per_round` still bounds walks.
/// This multiplier bounds the cheap probing on top, which must stay bounded (a
/// registry of any size would otherwise `stat` without limit) but can be far
/// larger, because six `stat`s is not a `du`.
///
/// 32 puts the shipped size lane at `4 * 32 = 128` rows examined per round,
/// covering a machine several times larger than the one this was diagnosed on
/// while still capping a pathological registry.
pub const EXAMINE_PER_WALK: usize = 32;

/// Rows a round may examine, given its per-round **walk** budget.
///
/// `0` (unlimited) stays unlimited. Pass the result to [`plan`] and to
/// [`saturation`]: it is the number that decides whether the lane can reach an
/// idle round, because a row the round never *looks at* stays due forever.
///
/// A lane with no cheap skip (the LOC lane walks every row with `tokei`) must
/// keep passing its walk budget to both — for it, examining and walking are the
/// same act.
pub fn examine_cap(walk_budget: usize) -> usize {
    walk_budget.saturating_mul(EXAMINE_PER_WALK)
}

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

    /// One operator-facing line describing the condition, or `None` when
    /// healthy. It no longer prescribes raising `max_scan_per_round`: since
    /// [`lane_shape`] adapts the examine cap to the registry, a lane whose walk
    /// budget is small still reaches an idle state.
    pub fn warning(self) -> Option<String> {
        match self {
            Self::Sustainable { .. } => None,
            Self::Saturated { rows, capacity } => Some(format!(
                "this lane tracks {rows} rows but its walk budget alone covers only \
                 {capacity} within one scan_interval_secs window; the scheduler \
                 widens how many rows a round examines (cheap probes) and, past \
                 {EXAMINE_CEILING} rows a round, stretches the effective TTL, so \
                 the lane still goes idle (about {} walk(s) a round would be \
                 needed if every row changed)",
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

/// What fired a round.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoundTrigger {
    /// The ticker pump: the only trigger that sweeps background rows.
    Pump,
    /// The diff watcher saw the ACTIVE worktree change: the active row only.
    Watch,
    /// Switch / create / workspace add: the active row plus never-measured rows.
    Kick,
}

/// Inputs that shape a lane, from config plus the real pump cadence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LaneParams {
    /// Configured per-row TTL (`scan_interval_secs`).
    pub ttl_secs: u64,
    /// Real pump cadence after its floor ([`pump_secs`]).
    pub pump_secs: u64,
    /// Configured walks per round (`max_scan_per_round`; 0 = unlimited).
    pub walk_budget: usize,
}

/// The lane's derived shape for a registry of `rows`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LaneShape {
    /// Walks per round (= configured; 0 = unlimited).
    pub walk_budget: usize,
    /// Rows a round may examine (cheap probes); 0 = unlimited.
    pub examine_cap: usize,
    /// Effective per-row TTL, stretched so a sweep fits in half a window.
    pub ttl_secs: u64,
}

/// Rows examined per round never exceed this, whatever the registry size.
///
/// Past `EXAMINE_CEILING * sweep_pumps` rows [`lane_shape`] stretches the
/// effective TTL instead of examining more per round, so a pathological
/// registry costs a bounded number of `stat`s per pump and still idles.
pub const EXAMINE_CEILING: usize = 512;

/// Real seconds between pumps: a quarter of the TTL, floored.
///
/// The same arithmetic as [`pump_slots`] in seconds rather than ticker slots,
/// so the planner can reason about the cadence the ticker really runs at (a
/// 45 s TTL with a 15 s floor pumps every 15 s, three times a window, not
/// four). Never `0`.
pub fn pump_secs(ttl_secs: u64, floor_secs: u64) -> u64 {
    (ttl_secs / 4)
        .max(floor_secs)
        .clamp(1, crate::time_policy::MAX_CADENCE_SECS.max(1))
}

/// Pumps per TTL window from the REAL cadence (not the nominal 4), at least 2.
pub fn pumps_per_ttl(ttl_secs: u64, pump_secs: u64) -> usize {
    let per = ttl_secs / pump_secs.max(1);
    usize::try_from(per).unwrap_or(usize::MAX).max(2)
}

/// Derive a lane's shape for a registry of `rows`.
///
/// `examine_cap = max(walk_budget * EXAMINE_PER_WALK, rows.div_ceil(sweep_pumps))`
/// capped at [`EXAMINE_CEILING`], where `sweep_pumps = max(1, pumps_per_ttl / 2)`
/// so a sweep takes at most half a window. The TTL is stretched only when even
/// the ceiling cannot cover the registry in `sweep_pumps` rounds:
/// `ttl = ttl_secs * rows.div_ceil(examine_cap * sweep_pumps).max(1)`.
///
/// `walk_budget == 0` is unlimited: examine cap `0` (unlimited) and the TTL
/// unchanged.
pub fn lane_shape(rows: usize, p: &LaneParams) -> LaneShape {
    if p.walk_budget == 0 {
        return LaneShape {
            walk_budget: 0,
            examine_cap: 0,
            ttl_secs: p.ttl_secs,
        };
    }
    let sweep_pumps = (pumps_per_ttl(p.ttl_secs, p.pump_secs) / 2).max(1);
    let examine_cap = p
        .walk_budget
        .saturating_mul(EXAMINE_PER_WALK)
        .max(rows.div_ceil(sweep_pumps))
        .min(EXAMINE_CEILING);
    let stretch = rows
        .div_ceil(examine_cap.saturating_mul(sweep_pumps).max(1))
        .max(1);
    LaneShape {
        walk_budget: p.walk_budget,
        examine_cap,
        ttl_secs: p
            .ttl_secs
            .saturating_mul(u64::try_from(stretch).unwrap_or(u64::MAX)),
    }
}

/// One round's work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoundPlan {
    /// Ordered rows to examine (walk only if changed, cold or active).
    pub due: Vec<String>,
    /// When the next pump has work: `Some(now)` = leftovers; `None` = nothing
    /// measured is tracked (empty registry).
    pub next_due_at: Option<i64>,
}

/// Order `(priority, measured_at, path)` rows the way [`plan`] does and keep at
/// most `cap` (`0` = unlimited). Returns the kept paths and whether any were
/// dropped.
fn order_and_cap(mut due: Vec<(ScanPriority, i64, &str)>, cap: usize) -> (Vec<String>, bool) {
    due.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)).then(a.2.cmp(b.2)));
    let truncated = cap > 0 && due.len() > cap;
    if cap > 0 {
        due.truncate(cap);
    }
    (
        due.into_iter().map(|(_, _, p)| p.to_string()).collect(),
        truncated,
    )
}

/// Plan a round.
///
/// - **Pump:** if any row has age `>= shape.ttl` (or is cold), take every row
///   with age `>= shape.ttl / 2`. That is alignment slack: a late recount is
///   tolerated and early ones are pulled forward, so rows re-converge into one
///   sweep instead of trickling one due row per pump forever. Ordered by
///   [`ScanPriority`], truncated to the examine cap.
/// - **Watch:** the active row only, if cold or stale.
/// - **Kick:** the active row (if cold or stale) plus cold rows, by priority,
///   truncated.
///
/// `ttl == 0`: every row is due on every pump, so such a lane never idles (this
/// is the documented meaning of `scan_interval_secs = 0`).
///
/// `next_due_at` assumes every planned row is stamped `now`; it is `Some(now)`
/// when the examine cap left due rows behind.
pub fn plan_round(
    targets: &[ScanTarget],
    now: i64,
    shape: &LaneShape,
    trigger: RoundTrigger,
) -> RoundPlan {
    let ttl = shape.ttl_secs;
    // Which rows this trigger considers, and the staleness threshold that
    // decides whether one of them is due.
    let (threshold, considered): (u64, Vec<&ScanTarget>) = match trigger {
        RoundTrigger::Pump => {
            if targets.iter().any(|t| priority(t, now, ttl).is_some()) {
                (ttl / 2, targets.iter().collect())
            } else {
                (ttl, Vec::new())
            }
        }
        RoundTrigger::Watch => (ttl, targets.iter().filter(|t| t.active).collect()),
        RoundTrigger::Kick => (
            ttl,
            targets
                .iter()
                .filter(|t| t.active || t.measured_at.is_none())
                .collect(),
        ),
    };
    let picked: Vec<(ScanPriority, i64, &str)> = considered
        .into_iter()
        .filter_map(|t| {
            priority(t, now, threshold)
                .map(|p| (p, t.measured_at.unwrap_or(i64::MIN), t.path.as_str()))
        })
        .collect();
    let cap = match trigger {
        RoundTrigger::Watch => 0,
        RoundTrigger::Pump | RoundTrigger::Kick => shape.examine_cap,
    };
    let (due, truncated) = order_and_cap(picked, cap);
    let next = if truncated {
        Some(now)
    } else {
        // `next_due_at` over the targets as they will be once this round has
        // stamped what it planned, without cloning the registry.
        let planned: std::collections::HashSet<&str> = due.iter().map(String::as_str).collect();
        let ttl_i = i64::try_from(ttl).unwrap_or(i64::MAX);
        targets
            .iter()
            .map(|t| {
                if planned.contains(t.path.as_str()) {
                    now.saturating_add(ttl_i)
                } else {
                    t.measured_at.map_or(now, |at| at.saturating_add(ttl_i))
                }
            })
            .min()
    };
    RoundPlan {
        due,
        next_due_at: next,
    }
}

/// Earliest `measured_at + ttl` over targets, `Some(now)` if any is cold, `None`
/// if empty.
///
/// This is the moment a [`RoundTrigger::Pump`] round first has work: before it,
/// no row has reached its TTL, so [`plan_round`] would plan nothing. A lane can
/// sleep until then without opening its database.
pub fn next_due_at(targets: &[ScanTarget], now: i64, ttl_secs: u64) -> Option<i64> {
    let ttl = i64::try_from(ttl_secs).unwrap_or(i64::MAX);
    targets
        .iter()
        .map(|t| t.measured_at.map_or(now, |at| at.saturating_add(ttl)))
        .min()
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
    fn a_walk_budget_alone_cannot_sweep_a_realistic_registry() {
        // WHY `EXAMINE_PER_WALK` EXISTS, as arithmetic.
        //
        // This test used to be named for the shipped size lane and assert that
        // it could never idle. That claim is no longer true of the lane — it is
        // still true of the number the lane used to pass. Keeping the assertion
        // and renaming it is deliberate: the arithmetic below is exactly the
        // reason the examine cap had to be introduced, and deleting it would
        // erase the evidence for a design decision.
        //
        // The size lane ships max_scan_per_round = 4. Feed that straight to
        // `saturation` as if every row cost a walk and a developer with 100
        // worktrees is 8x over capacity: every row is stale again before the
        // sweep reaches it, `plan` always returns work, and the lane runs
        // forever. That is what burned 5d18h of CPU in 3 days.
        //
        // See `the_shipped_size_lane_idles_once_a_skip_stops_costing_a_walk`
        // for the same defaults under the budget the lane actually uses now.
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
        // The warning still describes the condition, but it no longer tells
        // the operator to raise `max_scan_per_round`: `lane_shape` widens the
        // examine cap instead, so the lane idles at the shipped budget.
        let w = s.warning().expect("a saturated lane must warn");
        assert!(!w.contains("raise"), "{w}");
        assert!(w.contains("still goes idle"), "{w}");
        assert!(w.contains(&REALISTIC_ROWS.to_string()), "{w}");
        assert!(
            saturation(REALISTIC_ROWS, REALISTIC_ROWS.div_ceil(PUMPS_PER_TTL - 1)).can_idle(),
            "the budget the warning recommends must actually be sufficient"
        );
    }

    /// The fix, measured against the same defaults and the same registry size.
    ///
    /// Nothing about `saturation` changed — what changed is the budget the size
    /// lane hands it. A row the round merely *examines* costs six `stat`s
    /// (`disk::likely_unchanged_since`), not a `du`, so bounding examinations by
    /// the walk budget priced a skip as if it were a walk. On the machine this
    /// was diagnosed on that meant 82 of 83 rows re-stamping every five minutes
    /// while doing essentially no work.
    #[test]
    fn the_shipped_size_lane_idles_once_a_skip_stops_costing_a_walk() {
        const SHIPPED_BUDGET: usize = 4;
        // 83 is the real registry this was measured on; 100 is the round number
        // the sibling test uses. Both must now idle.
        for rows in [83usize, 100] {
            let s = saturation(rows, examine_cap(SHIPPED_BUDGET));
            assert!(
                s.can_idle(),
                "the size lane must reach an idle round at {rows} rows: {s:?}"
            );
            assert!(
                s.warning().is_none(),
                "a lane that can idle must not warn ({rows} rows)"
            );
            // The simulation agrees — and bounding the pump count matters: an
            // assertion that it *eventually* idles would pass for a lane that
            // takes an hour.
            let pumps = pumps_until_idle(rows, 45, examine_cap(SHIPPED_BUDGET), 64)
                .expect("a sustainable lane must reach an idle round");
            assert!(
                pumps < PUMPS_PER_TTL,
                "the sweep must COMPLETE inside one TTL window, took {pumps} pumps"
            );
        }
        // The cap is still a cap: a pathological registry must still saturate,
        // or the bound on cheap probing is not doing its job either.
        let huge = examine_cap(SHIPPED_BUDGET) * PUMPS_PER_TTL;
        assert!(
            !saturation(huge, examine_cap(SHIPPED_BUDGET)).can_idle(),
            "the examine cap must still bound an unbounded registry"
        );
    }

    /// `examine_cap` must preserve `plan`'s "0 means unlimited" convention — a
    /// lane configured with no walk budget must not silently acquire a cap of 0,
    /// which `plan` would read as unlimited anyway but `saturation` would not.
    #[test]
    fn an_unlimited_walk_budget_stays_unlimited_through_the_examine_cap() {
        assert_eq!(examine_cap(0), 0);
        assert!(saturation(1_000_000, examine_cap(0)).can_idle());
        // And it cannot overflow on a nonsense budget.
        assert_eq!(examine_cap(usize::MAX), usize::MAX);
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

    // ---- lane_shape / plan_round: the idle-lane regression gate (b165) ----

    /// What [`simulate`] observed.
    #[derive(Debug)]
    struct Sim {
        /// First pump whose plan was empty.
        idle_at: Option<usize>,
        /// Walks performed (only changed rows are walked).
        walks: usize,
        /// Pumps that planned anything.
        rounds: usize,
        /// Rows planned on each pump, for "stays idle" assertions.
        planned: Vec<usize>,
    }

    /// Drive [`lane_shape`] + [`plan_round`] with the examine-only model the
    /// hosts implement: an unchanged row is stamped without a walk; a changed
    /// row costs a walk, bounded by `walk_budget` per round (a changed row past
    /// the budget stays unstamped and therefore due). `changed_per_sweep` rows
    /// change at the start of every effective TTL window. Every row starts
    /// measured once, exactly one effective TTL ago.
    fn simulate(rows: usize, p: LaneParams, changed_per_sweep: usize, max_pumps: usize) -> Sim {
        let shape = lane_shape(rows, &p);
        let ttl = shape.ttl_secs as i64;
        let mut targets: Vec<ScanTarget> = (0..rows)
            .map(|i| ScanTarget::measured(format!("/wt/{i:06}"), NOW - ttl))
            .collect();
        let index: std::collections::HashMap<String, usize> = targets
            .iter()
            .enumerate()
            .map(|(i, t)| (t.path.clone(), i))
            .collect();
        let window = pumps_per_ttl(shape.ttl_secs, p.pump_secs);
        let mut changed = vec![false; rows];
        let mut sim = Sim {
            idle_at: None,
            walks: 0,
            rounds: 0,
            planned: Vec::new(),
        };
        let mut now = NOW;
        for pump in 0..max_pumps {
            if rows > 0 && pump % window == 0 {
                for j in 0..changed_per_sweep.min(rows) {
                    changed[((pump / window) * changed_per_sweep + j) % rows] = true;
                }
            }
            let plan = plan_round(&targets, now, &shape, RoundTrigger::Pump);
            sim.planned.push(plan.due.len());
            if plan.due.is_empty() {
                sim.idle_at.get_or_insert(pump);
            } else {
                sim.rounds += 1;
            }
            let mut walked = 0usize;
            for path in &plan.due {
                let i = index[path];
                if changed[i] {
                    if shape.walk_budget > 0 && walked >= shape.walk_budget {
                        continue;
                    }
                    walked += 1;
                    changed[i] = false;
                }
                targets[i].measured_at = Some(now);
            }
            sim.walks += walked;
            now += p.pump_secs as i64;
        }
        sim
    }

    fn params(ttl_secs: u64, pump: u64, walk_budget: usize) -> LaneParams {
        LaneParams {
            ttl_secs,
            pump_secs: pump,
            walk_budget,
        }
    }

    #[test]
    fn any_registry_reaches_idle() {
        for rows in [0usize, 1, 6, 7, 50, 74, 222, 384, 1000, 10_000] {
            for budget in [1usize, 2, 4] {
                for (ttl, pump) in [(900u64, 225u64), (45, 15), (300, 75)] {
                    let p = params(ttl, pump, budget);
                    let ppt = pumps_per_ttl(ttl, pump);
                    let bound = 4 * ppt * rows.div_ceil(EXAMINE_CEILING).max(1);
                    let sim = simulate(rows, p, 0, bound + ppt);
                    let idle = sim.idle_at.unwrap_or_else(|| {
                        panic!("rows={rows} budget={budget} ttl={ttl}: never idled ({sim:?})")
                    });
                    assert!(
                        idle <= bound,
                        "rows={rows} budget={budget} ttl={ttl}: idled at {idle} > {bound}"
                    );
                    for k in 0..(ppt / 2) {
                        assert_eq!(
                            sim.planned[idle + k],
                            0,
                            "rows={rows} budget={budget} ttl={ttl}: pump {} after idling planned work",
                            idle + k
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn idle_lane_has_no_walks_when_nothing_changed() {
        for (ttl, pump, budget) in [(900u64, 225u64, 2usize), (45, 15, 4)] {
            let p = params(ttl, pump, budget);
            let shape = lane_shape(222, &p);
            let window = pumps_per_ttl(shape.ttl_secs, pump);
            let sim = simulate(222, p, 0, 10 * window);
            assert_eq!(sim.walks, 0, "{sim:?}");
            // And the rounds it does run are bounded: one sweep per window.
            let per_sweep = 222usize.div_ceil(shape.examine_cap);
            assert!(
                sim.rounds <= 10 * per_sweep + 1,
                "ttl={ttl}: {} rounds over 10 windows",
                sim.rounds
            );
            assert!(sim.idle_at.is_some());
        }
    }

    #[test]
    fn changed_rows_are_walked_within_budget_then_idle() {
        // 50 rows, two walks a round, four rows change per window: two rounds
        // of walks, then idle, every window.
        let p = params(900, 225, 2);
        let one = simulate(50, p, 4, 4);
        assert_eq!(one.walks, 4, "{one:?}");
        assert_eq!(one.idle_at, Some(2), "{one:?}");
        let three = simulate(50, p, 4, 12);
        assert_eq!(three.walks, 12, "{three:?}");
        // Never more walks per round than the budget.
        let burst = simulate(50, p, 50, 1);
        assert_eq!(burst.walks, 2);
    }

    /// The 15x regression: a watch event used to plan the whole stale registry.
    #[test]
    fn watch_plans_only_the_active_row() {
        let shape = lane_shape(222, &params(900, 225, 2));
        let mut targets: Vec<ScanTarget> = (0..220)
            .map(|i| ScanTarget::measured(format!("/wt/{i:03}"), NOW - 5_000))
            .collect();
        targets.push(ScanTarget::measured("/wt/fresh", NOW - 1));
        targets.push(ScanTarget::cold("/wt/active").active());
        let plan = plan_round(&targets, NOW, &shape, RoundTrigger::Watch);
        assert_eq!(plan.due, vec!["/wt/active".to_string()]);

        // A fresh active row: nothing.
        let last = targets.len() - 1;
        targets[last].measured_at = Some(NOW - 1);
        let plan = plan_round(&targets, NOW, &shape, RoundTrigger::Watch);
        assert!(plan.due.is_empty(), "{:?}", plan.due);
        // …but the stale background rows keep the next pump due.
        assert_eq!(plan.next_due_at, Some(NOW - 5_000 + 900));
    }

    #[test]
    fn kick_plans_active_and_cold_only() {
        let shape = lane_shape(4, &params(900, 225, 2));
        let targets = vec![
            ScanTarget::measured("/stale", NOW - 5_000),
            ScanTarget::measured("/fresh", NOW - 1),
            ScanTarget::cold("/new"),
            ScanTarget::measured("/active", NOW - 5_000).active(),
        ];
        let plan = plan_round(&targets, NOW, &shape, RoundTrigger::Kick);
        assert_eq!(plan.due, vec!["/new".to_string(), "/active".to_string()]);
        // A fresh active row is not re-planned by a kick.
        let targets = vec![
            ScanTarget::measured("/stale", NOW - 5_000),
            ScanTarget::measured("/active", NOW - 1).active(),
        ];
        assert!(
            plan_round(&targets, NOW, &shape, RoundTrigger::Kick)
                .due
                .is_empty()
        );
    }

    #[test]
    fn alignment_pulls_half_stale_rows_into_one_sweep() {
        let shape = lane_shape(3, &params(900, 225, 2));
        let targets = vec![
            ScanTarget::measured("/due", NOW - 900),
            ScanTarget::measured("/half", NOW - 451),
            ScanTarget::measured("/young", NOW - 449),
        ];
        let plan = plan_round(&targets, NOW, &shape, RoundTrigger::Pump);
        assert_eq!(plan.due, vec!["/due".to_string(), "/half".to_string()]);
        assert_eq!(plan.next_due_at, Some(NOW - 449 + 900));

        // Half-stale alone does not start a sweep.
        let targets = vec![
            ScanTarget::measured("/half", NOW - 451),
            ScanTarget::measured("/young", NOW - 449),
        ];
        let plan = plan_round(&targets, NOW, &shape, RoundTrigger::Pump);
        assert!(plan.due.is_empty());
        assert_eq!(plan.next_due_at, Some(NOW - 451 + 900));
    }

    #[test]
    fn stretch_only_past_examine_ceiling() {
        let p = params(900, 225, 2);
        assert_eq!(lane_shape(50, &p).examine_cap, 64);
        assert_eq!(lane_shape(222, &p).examine_cap, 111);
        let thousand = lane_shape(1000, &p);
        assert_eq!(thousand.examine_cap, 500);
        assert_eq!(thousand.ttl_secs, 900);
        let huge = lane_shape(10_000, &p);
        assert_eq!(huge.examine_cap, EXAMINE_CEILING);
        assert!(huge.ttl_secs > 900, "{huge:?}");
        // Disk at (45, 15): one sweep pump per window, so 1,000 rows stretch.
        let disk = params(45, 15, 4);
        assert_eq!(lane_shape(50, &disk).examine_cap, 128);
        assert_eq!(lane_shape(222, &disk).ttl_secs, 45);
        let d1k = lane_shape(1000, &disk);
        assert_eq!((d1k.examine_cap, d1k.ttl_secs), (512, 90));
    }

    #[test]
    fn an_unlimited_walk_budget_examines_everything_at_the_configured_ttl() {
        let shape = lane_shape(10_000, &params(900, 225, 0));
        assert_eq!(
            shape,
            LaneShape {
                walk_budget: 0,
                examine_cap: 0,
                ttl_secs: 900
            }
        );
        let sim = simulate(300, params(900, 225, 0), 300, 4);
        assert_eq!(sim.walks, 300);
        assert_eq!(sim.idle_at, Some(1));
    }

    #[test]
    fn pump_secs_and_pumps_per_ttl_use_the_floor() {
        assert_eq!(pump_secs(45, 15), 15);
        assert_eq!(pumps_per_ttl(45, pump_secs(45, 15)), 3);
        assert_eq!(pump_secs(900, 60), 225);
        assert_eq!(pumps_per_ttl(900, 225), 4);
        assert_eq!(pump_secs(0, 15), 15);
        assert_eq!(pumps_per_ttl(0, 15), 2, "never fewer than two");
        assert_eq!(pump_secs(0, 0), 1, "never zero");
        assert_eq!(
            pumps_per_ttl(10, 0),
            10,
            "a zero pump cannot divide by zero"
        );
        assert_eq!(pump_secs(u64::MAX, 0), crate::time_policy::MAX_CADENCE_SECS);
    }

    #[test]
    fn a_zero_ttl_plans_every_row_every_pump() {
        let shape = lane_shape(3, &params(0, 15, 4));
        let targets = vec![
            ScanTarget::measured("/a", NOW),
            ScanTarget::measured("/b", NOW),
            ScanTarget::measured("/c", NOW),
        ];
        let plan = plan_round(&targets, NOW, &shape, RoundTrigger::Pump);
        assert_eq!(plan.due.len(), 3);
        assert_eq!(plan.next_due_at, Some(NOW));
    }

    #[test]
    fn a_truncated_round_leaves_the_next_pump_due_now() {
        let shape = LaneShape {
            walk_budget: 1,
            examine_cap: 2,
            ttl_secs: 100,
        };
        let targets = vec![
            ScanTarget::cold("/a"),
            ScanTarget::cold("/b"),
            ScanTarget::cold("/c"),
        ];
        let plan = plan_round(&targets, NOW, &shape, RoundTrigger::Kick);
        assert_eq!(plan.due.len(), 2);
        assert_eq!(plan.next_due_at, Some(NOW));
        let plan = plan_round(&targets, NOW, &shape, RoundTrigger::Pump);
        assert_eq!(plan.next_due_at, Some(NOW));
    }

    #[test]
    fn next_due_at_is_the_earliest_expiry() {
        assert_eq!(next_due_at(&[], NOW, 100), None);
        let targets = vec![
            ScanTarget::measured("/a", NOW - 10),
            ScanTarget::measured("/b", NOW - 50),
        ];
        assert_eq!(next_due_at(&targets, NOW, 100), Some(NOW + 50));
        let mut with_cold = targets.clone();
        with_cold.push(ScanTarget::cold("/c"));
        assert_eq!(next_due_at(&with_cold, NOW, 100), Some(NOW));
        // An empty registry plans nothing and tracks nothing.
        let plan = plan_round(
            &[],
            NOW,
            &lane_shape(0, &params(900, 225, 2)),
            RoundTrigger::Pump,
        );
        assert!(plan.due.is_empty());
        assert_eq!(plan.next_due_at, None);
        // No overflow on a huge TTL.
        assert_eq!(
            next_due_at(&[ScanTarget::measured("/x", i64::MAX - 1)], NOW, u64::MAX),
            Some(i64::MAX)
        );
    }
}
