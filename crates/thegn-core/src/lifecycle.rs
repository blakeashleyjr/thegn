//! Pure, budget-governed warm/suspend policy for per-worktree sandboxes.
//!
//! A managed-provider sandbox (sprite/ssh/k8s/oci) stays *running* only while a
//! live exec session into it exists — a registered resident bridge or an
//! interactive pane. Dropping that session lets the platform suspend the idle
//! sandbox (free, filesystem preserved). thegn's own background polling
//! (sidebar git status, activity) runs *inside* the sandbox for a provider
//! worktree, so left unchecked it keeps every visible worktree warm — burning
//! compute purely to refresh the sidebar.
//!
//! This module is the substrate-agnostic decision core: given a snapshot of the
//! worktrees and a budget, it decides which sandboxes to keep warm and which to
//! let suspend, and whether a given worktree should be *live-scanned* (touch the
//! sandbox) versus served from cache. It is pure (no I/O) so it is exhaustively
//! unit-tested; the host applies the decision by holding/dropping bridges.

/// Whether thegn may run a *live*, in-sandbox query (git status, activity
/// poll) for a worktree, versus serving its last-known cached value.
///
/// Local worktrees always live-scan — their git is a cheap host subprocess that
/// never wakes anything. A remote worktree is only live-scanned when it is the
/// active one or already warm; otherwise we serve cached glyphs so an idle,
/// suspended sandbox is never woken just to refresh the sidebar.
pub fn should_live_scan(is_remote: bool, is_warm: bool, is_active: bool) -> bool {
    !is_remote || is_warm || is_active
}

/// One worktree's inputs to the warm decision.
#[derive(Debug, Clone)]
pub struct WarmCandidate {
    pub worktree: String,
    /// Provider/ssh/k8s placement (a `Local` worktree has no sandbox to manage).
    pub is_remote: bool,
    /// A live interactive pane holds the sandbox session (never suspend under it).
    pub has_pane: bool,
    /// Genuine in-sandbox activity (a dev server / agent still working).
    pub busy: bool,
    /// Seconds since this worktree was last active (for the idle-TTL check).
    pub idle_secs: u64,
    /// Recency rank for discretionary keeps — smaller = more recently active.
    pub last_active_rank: u64,
    /// A bridge/session is currently registered (so it is currently warm).
    pub currently_warm: bool,
}

/// The budget + policy knobs governing the warm set (from `[lifecycle]` config).
#[derive(Debug, Clone)]
pub struct WarmBudget {
    /// When false, the policy is inert: nothing is suspended (today's behavior).
    pub enabled: bool,
    /// Max concurrently-warm sandboxes (discretionary keeps are bounded by this).
    pub max_warm: usize,
    /// Idle seconds before a non-essential warm sandbox may be suspended.
    pub idle_ttl_secs: u64,
    /// The active worktree is always kept warm.
    pub keep_active_warm: bool,
    /// A busy worktree (live process) stays warm past the idle TTL.
    pub keep_busy_warm: bool,
}

/// Everything `decide` needs: who's active, the budget, and the candidates.
#[derive(Debug, Clone)]
pub struct WarmInputs {
    pub active_worktree: Option<String>,
    pub budget: WarmBudget,
    pub candidates: Vec<WarmCandidate>,
}

/// The policy output: which sandboxes to hold warm and which to let suspend.
/// `suspend` only ever names currently-warm worktrees (others are already idle).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WarmDecision {
    pub keep_warm: Vec<String>,
    pub suspend: Vec<String>,
}

/// Decide the warm set. Pure + deterministic.
///
/// Ranking (budget-safe):
/// 1. Active worktree → keep (if `keep_active_warm`).
/// 2. Pane-held worktree → keep (a pane already holds the session).
/// 3. Busy worktree → keep (if `keep_busy_warm`), even past the idle TTL.
/// 4. Remaining slots up to `max_warm` → the most-recently-active worktrees that
///    are still within the idle TTL.
/// 5. Everyone else → suspend (only acted on if currently warm).
///
/// Mandatory keeps (1–3) are unconditional and not bounded by `max_warm` — we
/// never suspend under a live pane/active/busy worktree. `max_warm` bounds only
/// the discretionary keeps (4). Local worktrees are ignored (no sandbox cost).
/// It never proposes waking a suspended sandbox — only holding/releasing.
pub fn decide(input: &WarmInputs) -> WarmDecision {
    let remote: Vec<&WarmCandidate> = input.candidates.iter().filter(|c| c.is_remote).collect();

    // Policy off ⇒ inert: keep whatever is warm, suspend nothing.
    if !input.budget.enabled {
        return WarmDecision {
            keep_warm: remote
                .iter()
                .filter(|c| c.currently_warm)
                .map(|c| c.worktree.clone())
                .collect(),
            suspend: Vec::new(),
        };
    }

    let active = input.active_worktree.as_deref();
    let is_mandatory = |c: &WarmCandidate| -> bool {
        (input.budget.keep_active_warm && active == Some(c.worktree.as_str()))
            || c.has_pane
            || (input.budget.keep_busy_warm && c.busy)
    };

    let mut keep: Vec<String> = Vec::new();
    let mut mandatory_count = 0usize;
    for c in &remote {
        if is_mandatory(c) {
            keep.push(c.worktree.clone());
            mandatory_count += 1;
        }
    }

    // Discretionary keeps: most-recently-active worktrees still within the idle
    // TTL, up to whatever slots remain under `max_warm`.
    let slots = input.budget.max_warm.saturating_sub(mandatory_count);
    if slots > 0 {
        let mut discretionary: Vec<&WarmCandidate> = remote
            .iter()
            .copied()
            .filter(|c| !is_mandatory(c) && c.idle_secs < input.budget.idle_ttl_secs)
            .collect();
        discretionary.sort_by_key(|c| c.last_active_rank);
        for c in discretionary.into_iter().take(slots) {
            keep.push(c.worktree.clone());
        }
    }

    // Suspend a currently-warm sandbox ONLY once it is genuinely idle (past the
    // idle TTL) and not otherwise kept. Crucially, a within-TTL worktree is NEVER
    // suspended — even if it falls outside the `max_warm` discretionary keeps —
    // because suspending a freshly-opened worktree's sandbox out from under its
    // live pane drops the exec session and thrashes (open→suspend→reopen). So
    // `max_warm` bounds the *discretionary keeps* (which gate live-scanning), but
    // the suspend ACTION is strictly idle-TTL-driven: idle remotes suspend (and
    // recover on next open), recently-used ones stay warm.
    let suspend: Vec<String> = remote
        .iter()
        .filter(|c| {
            c.currently_warm
                && !keep.iter().any(|k| k == &c.worktree)
                && c.idle_secs >= input.budget.idle_ttl_secs
        })
        .map(|c| c.worktree.clone())
        .collect();

    WarmDecision {
        keep_warm: keep,
        suspend,
    }
}

/// Why a ready spare is being released. The host treats these differently: an
/// [`DestroyReason::OverTarget`] spare is genuinely surplus (recycling it would
/// keep the pool over target), while a [`DestroyReason::Stale`] one only aged
/// out — restoring it in place from its provisioned-base checkpoint resets its
/// idle clock, so the host may RECYCLE it instead of destroy+rebuild.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DestroyReason {
    /// More ready spares than the target: this one is surplus.
    OverTarget,
    /// Idle past `max_idle_secs`: freshness expired, not surplus.
    Stale,
}

/// What the warm-spare-pool maintainer should do this tick: how many new spares
/// to provision and which existing ones to destroy. Pure output of [`decide_pool`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PoolAction {
    /// How many fresh spares to provision (toward the target).
    pub create: usize,
    /// Spare sandbox names to destroy, tagged with why (over-target or idle
    /// past the TTL — the latter is a recycle candidate).
    pub destroy: Vec<(String, DestroyReason)>,
}

/// How the pool treats an *idle* ready spare — the axis that differs by provider
/// billing model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolPolicy {
    /// Scale-to-zero provider (sprites): an idle spare self-suspends for free, so
    /// idle time is **never** a release reason. A parked spare is only released
    /// when it is surplus (over target) or its base went stale (flake.lock drift).
    ParkIdle,
    /// Billed-while-stopped provider (VPS): a stopped instance still costs money,
    /// so a ready spare idle past `max_idle_secs` is aged out (recycled/destroyed)
    /// to reclaim spend.
    AgeOut,
}

/// One READY (unclaimed, provisioned) pool spare, as seen by [`decide_pool`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadySpare {
    /// The spare sandbox's name.
    pub name: String,
    /// Seconds since it was last touched (freshness rank + the `AgeOut` TTL check).
    pub idle_secs: u64,
    /// Its provisioned base was built against a **different** `flake.lock` than the
    /// repo's current one — so it must rotate regardless of idle time (the host
    /// applies the release as recycle-or-rebuild via [`recyclable`]).
    pub lock_stale: bool,
}

/// Whether a released spare can be RECYCLED by an in-place provider restore
/// instead of destroyed: it must carry a provisioned-base checkpoint AND have
/// been built against the current `flake.lock` (a changed lockfile invalidates
/// the checkpoint's toolchain). Empty hashes never match — an unknown lock
/// state must fall back to the destroy+rebuild path, not fake freshness.
pub fn recyclable(
    checkpoint_id: Option<&str>,
    spare_lock_hash: &str,
    current_lock_hash: &str,
) -> bool {
    checkpoint_id.is_some_and(|c| !c.is_empty())
        && !spare_lock_hash.is_empty()
        && spare_lock_hash == current_lock_hash
}

/// Decide the warm-spare-pool actions for one `(repo, env)`. Pure + deterministic.
///
/// - `target`: desired ready spares (resolved size; `0` ⇒ disabled, tear all down).
/// - `provisioning`: spares currently being built (counted toward the target so we
///   don't over-provision while one is in flight).
/// - `ready`: each READY (unclaimed, provisioned) spare ([`ReadySpare`]).
/// - `policy`: [`PoolPolicy::ParkIdle`] (scale-to-zero: keep idle spares) vs
///   [`PoolPolicy::AgeOut`] (billed-when-stopped: age idle spares out).
/// - `max_idle_secs`: under `AgeOut`, release a spare idle longer than this
///   (`0` ⇒ never). Ignored under `ParkIdle`.
///
/// Keeps the `target` freshest ready spares; destroys the rest (over-target). A
/// within-target spare is released (tagged `Stale`, a recycle candidate) when its
/// base is `lock_stale` (flake.lock drift — always, both policies) or — only under
/// `AgeOut` — it is idle past `max_idle_secs`. `create` fills toward the target,
/// counting in-flight provisions; if a stale one is released the next tick refills.
/// A spare that is BOTH over target and stale is tagged `OverTarget` (surplus
/// wins: recycling it would still leave the pool over target).
pub fn decide_pool(
    target: usize,
    provisioning: usize,
    ready: &[ReadySpare],
    policy: PoolPolicy,
    max_idle_secs: u64,
) -> PoolAction {
    let create = target.saturating_sub(ready.len().saturating_add(provisioning));
    let mut by_fresh: Vec<&ReadySpare> = ready.iter().collect();
    by_fresh.sort_by_key(|s| s.idle_secs); // freshest (smallest idle) first
    let age_out = matches!(policy, PoolPolicy::AgeOut) && max_idle_secs > 0;
    let destroy = by_fresh
        .iter()
        .enumerate()
        .filter_map(|(i, s)| {
            if i >= target {
                Some((s.name.clone(), DestroyReason::OverTarget))
            } else if s.lock_stale || (age_out && s.idle_secs >= max_idle_secs) {
                Some((s.name.clone(), DestroyReason::Stale))
            } else {
                None
            }
        })
        .collect();
    PoolAction { create, destroy }
}

/// One CLAIMED worktree sandbox's inputs to the hibernation decision
/// (snapshot-then-destroy for compute that bills while it exists — see
/// `[lifecycle] hibernate_after_secs`). Distinct from [`WarmCandidate`]:
/// suspend is instant to undo, hibernate costs a full re-provision, so the
/// gates here are strict and the TTL is expected to be much longer.
#[derive(Debug, Clone)]
pub struct HibernateCandidate {
    pub worktree: String,
    /// The focused/active worktree is never hibernated.
    pub is_active: bool,
    /// A live interactive pane holds the sandbox (never destroy under it).
    pub has_pane: bool,
    /// Genuine in-sandbox activity (dev server / agent still working).
    pub busy: bool,
    /// Seconds since this worktree was last active.
    pub idle_secs: u64,
    /// Env-resolved eligibility: `hibernate` mode on + provider has the file
    /// API the capture needs + remote placement (the host resolves all that).
    pub hibernate_enabled: bool,
    /// Idle TTL for this candidate's env (per-env override or the global);
    /// `0` disables hibernation for it.
    pub after_secs: u64,
    /// A hibernation row already exists (mid-capture, destroyed, or
    /// restoring) — never start another cycle on top of it.
    pub already_hibernated: bool,
}

/// The worktrees whose sandboxes should hibernate NOW: eligible, not
/// active/pane-held/busy, not already in a hibernation cycle, and idle at
/// least their TTL. Pure — the host re-checks the volatile gates (pane,
/// busy) again under the sandbox lock before acting.
pub fn decide_hibernate(cands: &[HibernateCandidate]) -> Vec<String> {
    cands
        .iter()
        .filter(|c| {
            c.hibernate_enabled
                && !c.already_hibernated
                && !c.is_active
                && !c.has_pane
                && !c.busy
                && c.after_secs > 0
                && c.idle_secs >= c.after_secs
        })
        .map(|c| c.worktree.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(name: &str, warm: bool) -> WarmCandidate {
        WarmCandidate {
            worktree: name.into(),
            is_remote: true,
            has_pane: false,
            busy: false,
            idle_secs: 0,
            last_active_rank: 0,
            currently_warm: warm,
        }
    }

    fn budget() -> WarmBudget {
        WarmBudget {
            enabled: true,
            max_warm: 2,
            idle_ttl_secs: 300,
            keep_active_warm: true,
            keep_busy_warm: true,
        }
    }

    /// READY spares with fresh locks (lock_stale = false) — the common case.
    fn ready(specs: &[(&str, u64)]) -> Vec<ReadySpare> {
        specs
            .iter()
            .map(|(n, i)| ReadySpare {
                name: n.to_string(),
                idle_secs: *i,
                lock_stale: false,
            })
            .collect()
    }

    #[test]
    fn decide_pool_fills_toward_target() {
        // Empty pool, target 2 ⇒ create 2.
        assert_eq!(decide_pool(2, 0, &[], PoolPolicy::AgeOut, 600).create, 2);
        // One ready + one provisioning ⇒ at target, create 0.
        let d = decide_pool(2, 1, &ready(&[("a", 5)]), PoolPolicy::AgeOut, 600);
        assert_eq!(d.create, 0);
        assert!(d.destroy.is_empty());
        // Two provisioning toward target 2 ⇒ create 0 (don't over-provision).
        assert_eq!(decide_pool(2, 2, &[], PoolPolicy::AgeOut, 600).create, 0);
    }

    #[test]
    fn decide_pool_destroys_over_target_keeping_freshest() {
        // 3 ready, target 2 ⇒ destroy the most-idle (c, idle 30); keep a,b.
        // Over-target is billing-agnostic: holds under ParkIdle too.
        for policy in [PoolPolicy::AgeOut, PoolPolicy::ParkIdle] {
            let d = decide_pool(2, 0, &ready(&[("a", 5), ("b", 10), ("c", 30)]), policy, 600);
            assert_eq!(d.create, 0);
            assert_eq!(
                d.destroy,
                vec![("c".to_string(), DestroyReason::OverTarget)]
            );
        }
    }

    #[test]
    fn decide_pool_ageout_recycles_idle_expired() {
        // AgeOut (VPS): within target but past the idle TTL ⇒ release it (next tick
        // refills), tagged Stale so the host may recycle it by restore-in-place.
        let d = decide_pool(
            2,
            0,
            &ready(&[("a", 5), ("stale", 999)]),
            PoolPolicy::AgeOut,
            600,
        );
        assert!(
            d.destroy
                .contains(&("stale".to_string(), DestroyReason::Stale))
        );
        assert!(!d.destroy.iter().any(|(n, _)| n == "a"));
        // max_idle_secs = 0 disables idle recycling.
        assert!(
            decide_pool(2, 0, &ready(&[("a", 99999)]), PoolPolicy::AgeOut, 0)
                .destroy
                .is_empty()
        );
    }

    #[test]
    fn decide_pool_parkidle_keeps_idle_spares() {
        // ParkIdle (sprites): a within-target spare idle FAR past any TTL is KEPT —
        // it self-suspends for free, so aging it out would throw away provisioning
        // for $0. This is the core scale-to-zero correction.
        let d = decide_pool(
            2,
            0,
            &ready(&[("a", 5), ("old", 99999)]),
            PoolPolicy::ParkIdle,
            600,
        );
        assert_eq!(d.create, 0);
        assert!(
            d.destroy.is_empty(),
            "idle spares are parked, not aged out: {:?}",
            d.destroy
        );
    }

    #[test]
    fn decide_pool_lock_stale_rotates_under_both_policies() {
        // A within-target spare whose base drifted (flake.lock changed) must rotate
        // regardless of idle time OR policy — else a ParkIdle pool would serve a
        // stale toolchain forever. Tagged Stale; the host recycles-or-rebuilds it.
        let spares = vec![
            ReadySpare {
                name: "fresh".into(),
                idle_secs: 1,
                lock_stale: false,
            },
            ReadySpare {
                name: "drifted".into(),
                idle_secs: 1,
                lock_stale: true,
            },
        ];
        for policy in [PoolPolicy::AgeOut, PoolPolicy::ParkIdle] {
            let d = decide_pool(2, 0, &spares, policy, 600);
            assert!(
                d.destroy
                    .contains(&("drifted".to_string(), DestroyReason::Stale)),
                "{policy:?}"
            );
            assert!(!d.destroy.iter().any(|(n, _)| n == "fresh"), "{policy:?}");
        }
    }

    #[test]
    fn decide_pool_over_target_wins_over_stale() {
        // A spare BOTH surplus and past the TTL is tagged OverTarget (recycling
        // it would still leave the pool over target ⇒ it must be destroyed).
        let d = decide_pool(
            1,
            0,
            &ready(&[("a", 5), ("old", 999)]),
            PoolPolicy::AgeOut,
            600,
        );
        assert_eq!(
            d.destroy,
            vec![("old".to_string(), DestroyReason::OverTarget)]
        );
    }

    #[test]
    fn decide_pool_target_zero_tears_down() {
        // Target 0 tears everything down as surplus — under BOTH policies (a
        // disabled/over-cap pool must drain even scale-to-zero spares).
        for policy in [PoolPolicy::AgeOut, PoolPolicy::ParkIdle] {
            let d = decide_pool(0, 0, &ready(&[("a", 1), ("b", 2)]), policy, 600);
            assert_eq!(d.create, 0);
            assert_eq!(d.destroy.len(), 2);
            assert!(
                d.destroy
                    .iter()
                    .all(|(_, r)| *r == DestroyReason::OverTarget)
            );
        }
    }

    #[test]
    fn recyclable_needs_checkpoint_and_matching_lock() {
        // The happy path: id + matching non-empty hashes.
        assert!(recyclable(Some("cp-1"), "lock-a", "lock-a"));
        // No / empty checkpoint id ⇒ never.
        assert!(!recyclable(None, "lock-a", "lock-a"));
        assert!(!recyclable(Some(""), "lock-a", "lock-a"));
        // Lock hash drift (flake.lock changed) ⇒ the base is stale.
        assert!(!recyclable(Some("cp-1"), "lock-a", "lock-b"));
        // Empty hashes never match (unknown lock state ⇒ rebuild, not recycle).
        assert!(!recyclable(Some("cp-1"), "", ""));
        assert!(!recyclable(Some("cp-1"), "", "lock-a"));
    }

    #[test]
    fn should_live_scan_only_for_local_active_or_warm() {
        // Local always scans (cheap host subprocess).
        assert!(should_live_scan(false, false, false));
        // Remote + not warm + not active ⇒ serve cache (never wake).
        assert!(!should_live_scan(true, false, false));
        // Remote + warm ⇒ live.
        assert!(should_live_scan(true, true, false));
        // Remote + active ⇒ live.
        assert!(should_live_scan(true, false, true));
    }

    #[test]
    fn disabled_policy_suspends_nothing() {
        let input = WarmInputs {
            active_worktree: None,
            budget: WarmBudget {
                enabled: false,
                ..budget()
            },
            candidates: vec![cand("a", true), cand("b", true)],
        };
        let d = decide(&input);
        assert!(d.suspend.is_empty(), "inert policy never suspends");
        assert_eq!(d.keep_warm.len(), 2);
    }

    #[test]
    fn active_and_pane_and_busy_are_kept() {
        let active = cand("active", true);
        let mut pane = cand("pane", true);
        pane.has_pane = true;
        let mut busy = cand("busy", true);
        busy.busy = true;
        busy.idle_secs = 9999; // past TTL but busy ⇒ kept
        let input = WarmInputs {
            active_worktree: Some("active".into()),
            budget: budget(),
            candidates: vec![active.clone(), pane, busy],
        };
        let d = decide(&input);
        for w in ["active", "pane", "busy"] {
            assert!(d.keep_warm.contains(&w.to_string()), "{w} kept");
            assert!(!d.suspend.contains(&w.to_string()), "{w} not suspended");
        }
    }

    #[test]
    fn max_warm_caps_discretionary_keeps_but_never_suspends_within_ttl() {
        // 4 idle warm worktrees, max_warm=2, none active/pane/busy, all WITHIN the
        // idle TTL ⇒ the 2 most recently active are the discretionary keeps, but
        // NOTHING is suspended: a within-TTL (recently-used) worktree is never
        // suspended out from under a live pane (the thrash bug). They simply stay
        // warm, untouched.
        let mut cands = Vec::new();
        for (i, name) in ["w0", "w1", "w2", "w3"].iter().enumerate() {
            let mut c = cand(name, true);
            c.idle_secs = 10; // within TTL
            c.last_active_rank = i as u64; // w0 most recent
            cands.push(c);
        }
        let input = WarmInputs {
            active_worktree: None,
            budget: budget(),
            candidates: cands,
        };
        let d = decide(&input);
        assert!(d.keep_warm.contains(&"w0".to_string()) && d.keep_warm.contains(&"w1".to_string()));
        assert!(
            d.suspend.is_empty(),
            "within-TTL worktrees are never suspended (no thrash): {:?}",
            d.suspend
        );
    }

    #[test]
    fn fresh_worktrees_beyond_max_warm_are_not_suspended() {
        // The reported bug: 3 just-opened remote worktrees (idle ~0), max_warm=2,
        // none active yet (still materializing) ⇒ NONE suspended (all within TTL),
        // so their exec sessions don't thrash.
        let cands: Vec<_> = ["a", "b", "c"]
            .iter()
            .map(|n| {
                let mut c = cand(n, true);
                c.idle_secs = 0;
                c
            })
            .collect();
        let input = WarmInputs {
            active_worktree: None,
            budget: budget(),
            candidates: cands,
        };
        assert!(decide(&input).suspend.is_empty());
    }

    #[test]
    fn idle_past_ttl_is_suspended_even_within_budget() {
        // One warm worktree, idle past TTL, not active/pane/busy ⇒ suspend even
        // though max_warm=2 has a free slot (TTL gates discretionary keeps).
        let mut c = cand("stale", true);
        c.idle_secs = 600; // > idle_ttl_secs (300)
        let input = WarmInputs {
            active_worktree: None,
            budget: budget(),
            candidates: vec![c],
        };
        let d = decide(&input);
        assert_eq!(d.suspend, vec!["stale".to_string()]);
        assert!(d.keep_warm.is_empty());
    }

    #[test]
    fn mandatory_keeps_exceed_max_warm() {
        // 3 pane-held worktrees with max_warm=2 ⇒ all kept (never suspend a pane).
        let mut cands = Vec::new();
        for name in ["p0", "p1", "p2"] {
            let mut c = cand(name, true);
            c.has_pane = true;
            cands.push(c);
        }
        let input = WarmInputs {
            active_worktree: None,
            budget: budget(),
            candidates: cands,
        };
        let d = decide(&input);
        assert_eq!(d.keep_warm.len(), 3);
        assert!(d.suspend.is_empty());
    }

    #[test]
    fn local_worktrees_ignored() {
        let mut local = cand("local", true);
        local.is_remote = false;
        let input = WarmInputs {
            active_worktree: None,
            budget: budget(),
            candidates: vec![local],
        };
        let d = decide(&input);
        assert!(d.keep_warm.is_empty() && d.suspend.is_empty());
    }

    #[test]
    fn non_warm_idle_is_not_suspended_again() {
        // A non-warm idle worktree needs no action (already suspended).
        let mut c = cand("cold", false);
        c.idle_secs = 600;
        let input = WarmInputs {
            active_worktree: None,
            budget: budget(),
            candidates: vec![c],
        };
        let d = decide(&input);
        assert!(d.suspend.is_empty(), "no-op for already-suspended");
    }

    fn hib(name: &str) -> HibernateCandidate {
        // An eligible, long-idle candidate; each test flips one gate.
        HibernateCandidate {
            worktree: name.into(),
            is_active: false,
            has_pane: false,
            busy: false,
            idle_secs: 7200,
            hibernate_enabled: true,
            after_secs: 3600,
            already_hibernated: false,
        }
    }

    #[test]
    fn hibernate_picks_only_idle_eligible_candidates() {
        let cands = vec![hib("/a"), hib("/b")];
        assert_eq!(decide_hibernate(&cands), vec!["/a", "/b"]);
    }

    #[test]
    fn hibernate_every_gate_blocks_independently() {
        for flip in [
            &mut |c: &mut HibernateCandidate| c.is_active = true,
            &mut |c: &mut HibernateCandidate| c.has_pane = true,
            &mut |c: &mut HibernateCandidate| c.busy = true,
            &mut |c: &mut HibernateCandidate| c.hibernate_enabled = false,
            &mut |c: &mut HibernateCandidate| c.already_hibernated = true,
            &mut |c: &mut HibernateCandidate| c.after_secs = 0,
            &mut |c: &mut HibernateCandidate| c.idle_secs = 0,
        ] as [&mut dyn FnMut(&mut HibernateCandidate); 7]
        {
            let mut c = hib("/wt");
            flip(&mut c);
            assert!(
                decide_hibernate(&[c.clone()]).is_empty(),
                "gate failed to block: {c:?}"
            );
        }
    }

    #[test]
    fn hibernate_ttl_boundary_is_inclusive_and_per_candidate() {
        let mut at = hib("/at"); // idle == ttl ⇒ hibernate
        at.idle_secs = 3600;
        let mut under = hib("/under"); // one second short ⇒ keep
        under.idle_secs = 3599;
        let mut custom = hib("/custom"); // per-env shorter TTL wins
        custom.after_secs = 100;
        custom.idle_secs = 100;
        assert_eq!(
            decide_hibernate(&[at, under, custom]),
            vec!["/at", "/custom"]
        );
    }
}
