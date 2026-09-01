//! Lifecycle policy for reclaiming worktree `target/` dirs.
//!
//! `[disk] auto_clean_on_merge` / `clean_on_pr_closed` cover *completion* — a
//! PR that merged or closed. They do not cover the dominant failure mode, which
//! is **abandonment**: a worktree nobody opened a PR for, or one the work
//! drifted away from, quietly holding several GiB of build output forever.
//!
//! Two rules close that gap, and both are pure functions of already-measured
//! facts so the decision can be unit-tested without touching a filesystem:
//!
//! * **Idle TTL** ([`Policy::idle_days`]) — a worktree with no file touched
//!   anywhere in it (source *or* `target/`) for N days has its `target/`
//!   reclaimed. This is housekeeping, so it is deliberately timid: a worktree
//!   with uncommitted work is exempt, because a cold rebuild is a poor thank-you
//!   for work that is still in flight.
//! * **Low-disk eviction** ([`Policy::on_low_disk`]) — when the filesystem
//!   crosses the `[stats] disk_free_critical` line, evict least-recently-touched
//!   `target/` dirs until free space is back above `disk_free_warn`. This is a
//!   pressure response, so it is decisive: uncommitted work does not exempt a
//!   worktree, only genuine recency does ([`LOW_DISK_MIN_IDLE_SECS`]).
//!
//! Rebasing the pressure rule on **free space** rather than an absolute GiB
//! figure is deliberate. An absolute total (`warn_threshold_gb`) is permanently
//! tripped on a machine that runs many worktrees — a threshold that is always
//! red carries no information, and one that always *acted* would delete
//! artifacts on an otherwise-roomy disk. Free percentage adapts to the disk and
//! stays silent while there is room.
//!
//! **The trade-off, stated plainly:** an unexpected cold rebuild costs an agent
//! mid-task real wall-clock. Every guard here exists to make that impossible for
//! a worktree anyone is actually using — the active one, one with a running
//! build, one touched recently — and the idle default is set far enough out
//! (two weeks) that tripping it means the worktree was abandoned, not paused.

/// A worktree the reclaimer may consider, as measured by the background disk
/// scan. Everything here is already known to the caller — nothing in this module
/// stats a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// Worktree path (the reclaim acts on `<path>/target`).
    pub path: String,
    /// Bytes currently held by `<path>/target`.
    pub target_bytes: u64,
    /// Seconds since the newest mtime anywhere in the worktree — the last time
    /// anyone edited a file or a build wrote an artifact.
    pub idle_secs: u64,
    /// The worktree the user is looking at. Never reclaimed, at any pressure.
    pub active: bool,
    /// A thegn-spawned build/test is running here. Never reclaimed.
    pub building: bool,
    /// `git status --porcelain` is non-empty. Exempt from the idle rule only.
    pub dirty: bool,
    /// This worktree still carries an unclosed pipeline dispatch row — work a
    /// supervisor has not yet verified. Never reclaimed, at any pressure: the
    /// artifact may be committed but the reviewing stage has still to build.
    pub awaiting_verification: bool,
    /// Seconds since thegn last reclaimed this worktree's `target/`, or `None`
    /// if it never has. Drives the [`RECLAIM_COOLDOWN_SECS`] hysteresis.
    pub reclaimed_secs_ago: Option<u64>,
}

/// The configured reclaim rules. Mirrors the `[disk]` keys plus the two
/// `[stats]` free-space thresholds it reuses, so no ninth number has to be
/// invented for the pressure rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    /// `[disk] idle_clean_days`. 0 disables the idle rule.
    pub idle_days: u32,
    /// `[disk] reclaim_on_low_disk`.
    pub on_low_disk: bool,
    /// `[stats] disk_free_warn` — eviction stops once free % reaches this.
    pub free_warn_pct: u8,
    /// `[stats] disk_free_critical` — eviction starts at or below this.
    pub free_critical_pct: u8,
}

/// Filesystem headroom for the worktrees, from one `statvfs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pressure {
    /// Free space as a percentage of the total (0–100).
    pub free_pct: u8,
    /// Total bytes on the filesystem.
    pub total_bytes: u64,
    /// Bytes available to an unprivileged user.
    pub free_bytes: u64,
}

/// Why a `target/` was picked. Carried through to the notification so the next
/// attach can explain the cold rebuild rather than looking like a bug.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// No file touched anywhere in the worktree for this many whole days.
    Idle { days: u64 },
    /// Free space was at or below the critical line.
    LowDisk { free_pct: u8 },
}

impl Reason {
    /// One-line explanation for the `disk_cleaned` notification / CLI line.
    pub fn note(&self) -> String {
        match self {
            Reason::Idle { days } => format!("idle {days}d"),
            Reason::LowDisk { free_pct } => format!("low disk ({free_pct}% free)"),
        }
    }
}

/// One decided reclaim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reclaim {
    /// Worktree whose `target/` to clean.
    pub path: String,
    /// Bytes expected back (the measured `target/` size).
    pub bytes: u64,
    /// Which rule fired.
    pub reason: Reason,
}

/// Below this a `target/` is not worth a cold rebuild: an empty or barely-warm
/// build tree buys nothing and the reclaim is pure downside. 256 MiB is roughly
/// "one crate graph's dependencies have not even finished compiling yet".
pub const MIN_RECLAIM_BYTES: u64 = 256 * 1024 * 1024;

/// Even under disk pressure, a worktree touched inside this window is left
/// alone: it is almost certainly an agent mid-task, which is exactly the cost
/// this policy exists not to impose.
pub const LOW_DISK_MIN_IDLE_SECS: u64 = 60 * 60;

/// Hysteresis, half one: a worktree reclaimed inside this window is not
/// reclaimed again.
///
/// Without it the pressure rule and a build loop form an oscillator. Reclaiming
/// `target/` makes a worktree look *maximally* idle-and-large the moment its
/// next build repopulates it, so on a disk that stays near the critical line the
/// same worktree is chosen round after round: delete 20 GiB, rebuild 20 GiB,
/// delete it again — pure I/O with no net space gained, which is precisely the
/// thrash observed on 2026-08-29. Six hours is longer than any single warm
/// rebuild, so a worktree gets to finish being useful before it is a candidate
/// again.
pub const RECLAIM_COOLDOWN_SECS: u64 = 6 * 60 * 60;

/// Hysteresis, half two: evict past the warn line by this many points rather
/// than stopping exactly on it.
///
/// Stopping at `free_warn_pct` leaves the filesystem one build away from
/// critical, so the next round trips the rule again. Overshooting buys a margin
/// that a normal build cycle cannot immediately erase, which turns a
/// permanently-firing rule into one that fires, fixes, and goes quiet.
pub const LOW_DISK_OVERSHOOT_PCT: u8 = 5;

/// Seconds a worktree must be untouched before the idle rule may fire, or
/// `None` when the rule is off. Exposed so a caller can defer the (relatively
/// expensive) `git status` dirtiness probe to the few candidates that could
/// possibly qualify.
pub fn idle_threshold_secs(policy: &Policy) -> Option<u64> {
    (policy.idle_days > 0).then(|| u64::from(policy.idle_days) * 86_400)
}

/// Bytes that must be freed to bring `free_pct` back up to `warn_pct`.
/// Saturating and zero when already above the line.
pub fn need_bytes(pressure: &Pressure, warn_pct: u8) -> u64 {
    let want = pressure
        .total_bytes
        .saturating_mul(u64::from(warn_pct))
        .saturating_div(100);
    want.saturating_sub(pressure.free_bytes)
}

/// Whether a candidate may ever be reclaimed automatically, under any rule.
///
/// The two additions beyond "not in use and worth the rebuild" are hysteresis
/// ([`RECLAIM_COOLDOWN_SECS`]) and the unverified-work exemption: a worktree
/// whose pipeline row nobody has closed is still live work, even though no
/// process is running in it — reclaiming it imposes a cold rebuild on the very
/// next stage.
fn eligible(c: &Candidate) -> bool {
    !c.active
        && !c.building
        && !c.awaiting_verification
        && c.target_bytes >= MIN_RECLAIM_BYTES
        && c.reclaimed_secs_ago
            .is_none_or(|s| s >= RECLAIM_COOLDOWN_SECS)
}

/// Decide which `target/` dirs to reclaim.
///
/// Idle matches come first (they are unconditional housekeeping); low-disk
/// evictions are then appended, least-recently-touched first, only as far as
/// [`need_bytes`] requires and only when [`Pressure::free_pct`] is at or below
/// [`Policy::free_critical_pct`]. The result is deterministic — ties break on
/// path — so the whole decision is testable as data.
pub fn plan(candidates: &[Candidate], policy: &Policy, pressure: Option<Pressure>) -> Vec<Reclaim> {
    let mut out: Vec<Reclaim> = Vec::new();

    if let Some(threshold) = idle_threshold_secs(policy) {
        let mut idle: Vec<&Candidate> = candidates
            .iter()
            .filter(|c| eligible(c) && !c.dirty && c.idle_secs >= threshold)
            .collect();
        idle.sort_by(|a, b| b.idle_secs.cmp(&a.idle_secs).then(a.path.cmp(&b.path)));
        out.extend(idle.into_iter().map(|c| Reclaim {
            path: c.path.clone(),
            bytes: c.target_bytes,
            reason: Reason::Idle {
                days: c.idle_secs / 86_400,
            },
        }));
    }

    let Some(p) = pressure else {
        return out;
    };
    if !policy.on_low_disk || p.free_pct > policy.free_critical_pct {
        return out;
    }

    // How much more is needed once the idle matches above are counted. The
    // target is the warn line PLUS the overshoot, so the rule buys a margin
    // instead of parking the disk one build below critical (see
    // `LOW_DISK_OVERSHOOT_PCT`).
    let already: u64 = out.iter().map(|r| r.bytes).sum();
    let target_pct = policy.free_warn_pct.saturating_add(LOW_DISK_OVERSHOOT_PCT);
    let mut need = need_bytes(&p, target_pct).saturating_sub(already);
    if need == 0 {
        return out;
    }

    let mut rest: Vec<&Candidate> = candidates
        .iter()
        .filter(|c| {
            eligible(c)
                && c.idle_secs >= LOW_DISK_MIN_IDLE_SECS
                && !out.iter().any(|r| r.path == c.path)
        })
        .collect();
    // Least recently touched first — the LRU eviction order.
    rest.sort_by(|a, b| b.idle_secs.cmp(&a.idle_secs).then(a.path.cmp(&b.path)));
    for c in rest {
        out.push(Reclaim {
            path: c.path.clone(),
            bytes: c.target_bytes,
            reason: Reason::LowDisk {
                free_pct: p.free_pct,
            },
        });
        need = need.saturating_sub(c.target_bytes);
        if need == 0 {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1024 * 1024 * 1024;
    const DAY: u64 = 86_400;

    fn cand(path: &str, gib: u64, idle_days: u64) -> Candidate {
        Candidate {
            path: path.into(),
            target_bytes: gib * GIB,
            idle_secs: idle_days * DAY,
            active: false,
            building: false,
            dirty: false,
            awaiting_verification: false,
            reclaimed_secs_ago: None,
        }
    }

    fn policy() -> Policy {
        Policy {
            idle_days: 14,
            on_low_disk: true,
            free_warn_pct: 15,
            free_critical_pct: 10,
        }
    }

    /// A 1 TiB filesystem at `free_pct` free.
    fn pressure(free_pct: u8) -> Pressure {
        let total = 1024 * GIB;
        Pressure {
            free_pct,
            total_bytes: total,
            free_bytes: total * u64::from(free_pct) / 100,
        }
    }

    #[test]
    fn a_worktree_awaiting_verification_is_never_reclaimed() {
        // The 2026-08-29 shape: the coder committed and exited, so nothing is
        // running and nothing is dirty — but the row is unclosed and the next
        // stage still has to build here. Reclaiming would impose a cold rebuild
        // on work that is mid-pipeline.
        let mut c = cand("/wt/a", 30, 40);
        c.awaiting_verification = true;
        // Neither rule may touch it: not the idle rule...
        assert!(plan(&[c.clone()], &policy(), None).is_empty());
        // ...nor eviction at genuinely critical pressure.
        assert!(plan(&[c], &policy(), Some(pressure(2))).is_empty());
    }

    #[test]
    fn a_just_reclaimed_worktree_is_not_reclaimed_again() {
        // Hysteresis: without the cooldown, a rebuild repopulates `target/` and
        // the very next round picks the same worktree — delete, rebuild,
        // delete, forever, with no net space gained.
        let mut c = cand("/wt/a", 30, 40);
        c.reclaimed_secs_ago = Some(RECLAIM_COOLDOWN_SECS - 1);
        assert!(
            plan(&[c.clone()], &policy(), Some(pressure(2))).is_empty(),
            "inside the cooldown the worktree must be left alone"
        );
        // Once the cooldown has elapsed it is a candidate again.
        c.reclaimed_secs_ago = Some(RECLAIM_COOLDOWN_SECS);
        assert_eq!(plan(&[c], &policy(), Some(pressure(2))).len(), 1);
    }

    #[test]
    fn eviction_overshoots_the_warn_line_so_the_rule_stops_firing() {
        // At 9% free on a 1 TiB disk, stopping exactly at warn (15%) frees
        // ~61 GiB and leaves the disk one build from critical. The overshoot
        // target (15 + 5 = 20%) asks for ~113 GiB instead, so the round buys a
        // margin. Two 64 GiB candidates make the difference observable: the
        // warn-only target is satisfied by one, the overshoot target needs both.
        let cands = [cand("/wt/a", 64, 30), cand("/wt/b", 64, 20)];
        let mut p = policy();
        p.idle_days = 0; // isolate the pressure rule
        let picked = plan(&cands, &p, Some(pressure(9)));
        assert_eq!(
            picked.len(),
            2,
            "overshoot must keep evicting past the warn line: {picked:?}"
        );
        assert!(need_bytes(&pressure(9), 15) < need_bytes(&pressure(9), 20));
    }

    #[test]
    fn idle_threshold_is_none_when_the_rule_is_off() {
        let mut p = policy();
        assert_eq!(idle_threshold_secs(&p), Some(14 * DAY));
        p.idle_days = 0;
        assert_eq!(idle_threshold_secs(&p), None);
    }

    #[test]
    fn an_abandoned_worktree_is_reclaimed_and_a_fresh_one_is_not() {
        let cands = vec![cand("/wt/stale", 16, 30), cand("/wt/fresh", 16, 1)];
        let got = plan(&cands, &policy(), None);
        assert_eq!(
            got,
            vec![Reclaim {
                path: "/wt/stale".into(),
                bytes: 16 * GIB,
                reason: Reason::Idle { days: 30 },
            }]
        );
    }

    #[test]
    fn the_idle_rule_is_off_at_zero_days() {
        let mut p = policy();
        p.idle_days = 0;
        assert!(plan(&[cand("/wt/ancient", 16, 400)], &p, None).is_empty());
    }

    #[test]
    fn active_building_and_dirty_worktrees_survive_the_idle_rule() {
        let mut active = cand("/wt/active", 16, 30);
        active.active = true;
        let mut building = cand("/wt/building", 16, 30);
        building.building = true;
        let mut dirty = cand("/wt/dirty", 16, 30);
        dirty.dirty = true;
        let cands = vec![active, building, dirty];
        assert!(plan(&cands, &policy(), None).is_empty());
    }

    #[test]
    fn a_tiny_target_is_not_worth_a_cold_rebuild() {
        let mut small = cand("/wt/small", 0, 90);
        small.target_bytes = MIN_RECLAIM_BYTES - 1;
        assert!(plan(&[small], &policy(), None).is_empty());
        // Exactly at the floor it qualifies.
        let mut at = cand("/wt/at", 0, 90);
        at.target_bytes = MIN_RECLAIM_BYTES;
        assert_eq!(plan(&[at], &policy(), None).len(), 1);
    }

    #[test]
    fn idle_matches_come_out_least_recently_touched_first() {
        let cands = vec![
            cand("/wt/b", 1, 20),
            cand("/wt/c", 1, 40),
            cand("/wt/a", 1, 40),
        ];
        let paths: Vec<String> = plan(&cands, &policy(), None)
            .into_iter()
            .map(|r| r.path)
            .collect();
        // 40d before 20d; the 40d tie breaks on path.
        assert_eq!(paths, ["/wt/a", "/wt/c", "/wt/b"]);
    }

    #[test]
    fn need_bytes_is_the_gap_to_the_warn_line_and_saturates_above_it() {
        // 1 TiB at 10% free, warn at 15% ⇒ the gap between the two lines.
        let total = 1024 * GIB;
        assert_eq!(
            need_bytes(&pressure(10), 15),
            total * 15 / 100 - total * 10 / 100
        );
        // Already above the warn line ⇒ nothing needed.
        assert_eq!(need_bytes(&pressure(50), 15), 0);
    }

    #[test]
    fn low_disk_eviction_stays_asleep_above_the_critical_line() {
        // Nothing is idle-eligible, and 40% free is nowhere near critical.
        let cands = vec![cand("/wt/warm", 100, 2)];
        assert!(plan(&cands, &policy(), Some(pressure(40))).is_empty());
    }

    #[test]
    fn low_disk_eviction_takes_lru_targets_until_the_gap_is_covered() {
        // 1 TiB at 8% free, warn 15% + 5% overshoot ⇒ target 20%, so ~123 GiB
        // back rather than the ~72 GiB the bare warn line would ask for. Three
        // 40 GiB targets are needed to clear it; eviction is still LRU-ordered
        // and still stops as soon as the gap is covered.
        let cands = vec![
            cand("/wt/oldest", 40, 5),
            cand("/wt/older", 40, 4),
            cand("/wt/newer", 40, 3),
        ];
        let got = plan(&cands, &policy(), Some(pressure(8)));
        assert_eq!(got.len(), 3, "{got:?}");
        assert_eq!(got[0].path, "/wt/oldest");
        assert_eq!(got[1].path, "/wt/older");
        assert_eq!(got[2].path, "/wt/newer");
        assert!(matches!(got[0].reason, Reason::LowDisk { free_pct: 8 }));
    }

    #[test]
    fn low_disk_eviction_does_not_exempt_dirty_but_does_exempt_the_just_touched() {
        let mut dirty = cand("/wt/dirty", 100, 5);
        dirty.dirty = true;
        let mut just_touched = cand("/wt/hot", 100, 0);
        just_touched.idle_secs = LOW_DISK_MIN_IDLE_SECS - 1;
        let got = plan(&[dirty, just_touched], &policy(), Some(pressure(5)));
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].path, "/wt/dirty");
    }

    #[test]
    fn low_disk_eviction_honours_the_off_switch_and_the_active_guard() {
        let mut p = policy();
        p.on_low_disk = false;
        p.idle_days = 0;
        let cands = vec![cand("/wt/x", 100, 30)];
        assert!(plan(&cands, &p, Some(pressure(2))).is_empty());

        // Back on, but the only candidate is the active worktree.
        p.on_low_disk = true;
        let mut active = cand("/wt/x", 100, 30);
        active.active = true;
        assert!(plan(&[active], &p, Some(pressure(2))).is_empty());
    }

    #[test]
    fn an_idle_match_counts_against_the_pressure_gap_and_is_never_listed_twice() {
        // 1 TiB at 8% free ⇒ ~123 GiB needed to reach the 20% overshoot target.
        // The 128 GiB idle match covers it on its own, so the pressure pass adds
        // nothing — and critically does not list `/wt/stale` a second time.
        let cands = vec![cand("/wt/stale", 128, 30), cand("/wt/warm", 80, 2)];
        let got = plan(&cands, &policy(), Some(pressure(8)));
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].path, "/wt/stale");
        assert!(matches!(got[0].reason, Reason::Idle { .. }));
    }

    #[test]
    fn reason_notes_read_as_english() {
        assert_eq!(Reason::Idle { days: 21 }.note(), "idle 21d");
        assert_eq!(Reason::LowDisk { free_pct: 7 }.note(), "low disk (7% free)");
    }
}
