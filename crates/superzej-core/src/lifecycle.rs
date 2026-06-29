//! Pure, budget-governed warm/suspend policy for per-worktree sandboxes.
//!
//! A managed-provider sandbox (sprite/ssh/k8s/oci) stays *running* only while a
//! live exec session into it exists — a registered resident bridge or an
//! interactive pane. Dropping that session lets the platform suspend the idle
//! sandbox (free, filesystem preserved). superzej's own background polling
//! (sidebar git status, activity) runs *inside* the sandbox for a provider
//! worktree, so left unchecked it keeps every visible worktree warm — burning
//! compute purely to refresh the sidebar.
//!
//! This module is the substrate-agnostic decision core: given a snapshot of the
//! worktrees and a budget, it decides which sandboxes to keep warm and which to
//! let suspend, and whether a given worktree should be *live-scanned* (touch the
//! sandbox) versus served from cache. It is pure (no I/O) so it is exhaustively
//! unit-tested; the host applies the decision by holding/dropping bridges.

/// Whether superzej may run a *live*, in-sandbox query (git status, activity
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

    // Suspend anything currently warm that we are not keeping.
    let suspend: Vec<String> = remote
        .iter()
        .filter(|c| c.currently_warm && !keep.iter().any(|k| k == &c.worktree))
        .map(|c| c.worktree.clone())
        .collect();

    WarmDecision {
        keep_warm: keep,
        suspend,
    }
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
    fn budget_caps_discretionary_keeps_by_recency() {
        // 4 idle warm worktrees, max_warm=2, none active/pane/busy ⇒ keep the 2
        // most recently active, suspend the other 2.
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
        assert_eq!(d.suspend.len(), 2);
        assert!(d.suspend.contains(&"w2".to_string()) && d.suspend.contains(&"w3".to_string()));
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
}
