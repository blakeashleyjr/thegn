//! Host side of the sandbox warm/lifecycle policy: reconcile the *warm set* of
//! managed-provider sandboxes against the budget, releasing idle ones so they
//! suspend (free, filesystem preserved) instead of being held warm forever by
//! superzej's own background polling.
//!
//! "Warm" = a resident bridge is registered for the worktree's loc (a live exec
//! session that keeps the provider sandbox running). Suspending a sandbox just
//! means dropping that bridge (`svc::bridge::drop_key`) — `BridgeClient`'s `Drop`
//! kills the exec child, and the platform suspends the now-idle VM. The pure
//! ranking lives in [`superzej_core::lifecycle::decide`]; this module only gathers
//! inputs and applies the result. It runs on the hydration cadence (~5s).

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use superzej_core::config::LifecycleConfig;
use superzej_core::lifecycle::{WarmBudget, WarmCandidate, WarmInputs, decide};
use superzej_core::remote::GitLoc;

/// Per-worktree "last seen active/busy" timestamps, so the reconcile can apply the
/// idle TTL (the activity FSM persists state strings but not a host-clock idle
/// duration). Process-global, mirroring `hydrate::glyph_cache`.
fn last_active() -> &'static Mutex<HashMap<String, Instant>> {
    static R: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Reconcile the warm set: drop resident bridges for idle, over-budget remote
/// worktrees so their sandboxes suspend and stop being live-scanned. No-op when
/// the policy is disabled. Best-effort + cheap (a few map lookups + maybe a
/// process kill); safe to call from the hydration thread.
pub fn reconcile(session: &crate::session::Session, cfg: &LifecycleConfig) {
    if !cfg.enabled {
        return;
    }
    let now = Instant::now();
    let states = superzej_core::activity::read_states();
    let active_path: Option<String> = session.active_group().map(|g| g.path.clone());

    let mut reg = last_active().lock().unwrap();
    let mut candidates: Vec<WarmCandidate> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for g in &session.worktrees {
        if g.path.is_empty() || !seen.insert(g.path.clone()) {
            continue;
        }
        let loc = GitLoc::for_worktree(Path::new(&g.path));
        if !loc.is_remote() {
            continue;
        }
        let currently_warm = superzej_svc::bridge::for_loc(&loc).is_some();
        let busy = states.get(&g.name).map(|s| s == "active").unwrap_or(false);
        let is_active = active_path.as_deref() == Some(g.path.as_str());
        // Refresh the activity clock while active/busy; otherwise let it age.
        if busy || is_active {
            reg.insert(g.path.clone(), now);
        }
        let last = *reg.entry(g.path.clone()).or_insert(now);
        let idle_secs = now.saturating_duration_since(last).as_secs();
        candidates.push(WarmCandidate {
            worktree: g.path.clone(),
            is_remote: true,
            // The active/background pane holds its OWN interactive exec session
            // independent of the resident bridge, so dropping the bridge never
            // kills a pane; `is_active`/`busy` already protect the focused/working
            // worktree. Treat panes as not-held here (bridge is what we manage).
            has_pane: false,
            busy,
            idle_secs,
            last_active_rank: idle_secs,
            currently_warm,
        });
    }
    // Prune vanished worktrees from the clock.
    reg.retain(|p, _| seen.contains(p));
    drop(reg);

    let decision = decide(&WarmInputs {
        active_worktree: active_path,
        budget: WarmBudget {
            enabled: true,
            max_warm: cfg.max_warm,
            idle_ttl_secs: cfg.idle_ttl_secs,
            keep_active_warm: cfg.keep_active_warm,
            keep_busy_warm: cfg.keep_busy_warm,
        },
        candidates,
    });

    for wt in &decision.suspend {
        let loc = GitLoc::for_worktree(Path::new(wt));
        if let Some(key) = superzej_svc::bridge::bridge_key(&loc) {
            superzej_svc::bridge::drop_key(&key);
            tracing::debug!(
                target: "szhost::lifecycle",
                worktree = %wt,
                "suspended idle sandbox (dropped resident bridge)"
            );
        }
    }
}
