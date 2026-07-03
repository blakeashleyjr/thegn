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

use superzej_core::config::{Config, LifecycleConfig};
use superzej_core::lifecycle::{WarmBudget, WarmCandidate, WarmInputs, decide, decide_pool};
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

/// Resolve the `(repo_root, repo, env_name)` triple the warm pool operates on
/// for the active worktree — using the worktree's EFFECTIVE env (its DB
/// selection, falling back through the normal repo/global layering via
/// `resolve_env`), never the bare ambient default: reconciling under the default
/// while the worktree runs a picked env warms — and claims! — spares for the
/// wrong env (the phantom-spare / bare-sprite-shell incident).
pub fn pool_context(
    db: &superzej_core::db::Db,
    cfg: &Config,
    wt: &str,
    loc: &GitLoc,
) -> (std::path::PathBuf, String, String) {
    let repo_root = db
        .repo_root_for(wt)
        .ok()
        .flatten()
        .filter(|s| !s.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| superzej_core::repo::main_worktree(Path::new(wt)))
        .unwrap_or_else(|| std::path::PathBuf::from(wt));
    let repo = repo_root.to_string_lossy().into_owned();
    let selected = db.effective_env(wt, &repo);
    let env_name = cfg
        .resolve_env(&repo_root, loc, Path::new(wt), selected.as_deref())
        .name;
    (repo_root, repo, env_name)
}

/// The effective warm-pool target for `(repo, env)`: the runtime +/- override in
/// the DB (set by the hotkey, per repo+env) if present, else the configured
/// `[lifecycle.pool] size`.
pub fn effective_pool_target(
    db: &superzej_core::db::Db,
    cfg: &Config,
    repo: &str,
    env: &str,
) -> usize {
    db.pool_target(repo, env)
        .ok()
        .flatten()
        .map(|t| t.max(0) as usize)
        .unwrap_or(cfg.lifecycle.pool.size)
}

/// Reconcile the warm-spare POOL for one `(repo, env)`: create spares toward the
/// target and destroy over-target/idle ones. Runs on a background thread (each
/// spare provision is minutes); destroys are quick + synchronous, creates are
/// spawned concurrently (each marks itself `provisioning` in the DB immediately,
/// so a re-entrant tick won't double-provision). `notify` pulses the UI when the
/// pool changes. No-op unless the pool is enabled for this (repo, env).
pub fn reconcile_pool<N>(cfg: &Config, repo_root: &Path, env_name: &str, notify: N)
where
    N: Fn() + Clone + Send + 'static,
{
    let Ok(db) = superzej_core::db::Db::open() else {
        return;
    };
    let repo = repo_root.to_string_lossy().into_owned();
    let target = effective_pool_target(&db, cfg, &repo, env_name);
    let max_idle = cfg.lifecycle.pool.max_idle_secs;

    let spares = db.pool_spares_for(&repo, env_name).unwrap_or_default();
    // Nothing configured and nothing to clean up ⇒ cheap no-op.
    if target == 0 && spares.is_empty() {
        return;
    }
    let now = superzej_core::util::now();
    // A spare stuck in `provisioning` past this is ORPHANED — its provision task
    // died with a previous szhost session (a restart drops the in-flight thread),
    // or hung. Without clearing it, `decide_pool` counts it as in-flight FOREVER
    // (`create = target - (ready + provisioning)` → 0) so the pool never refills
    // and no warm spare appears. Destroy stale ones so a replacement is created; a
    // genuinely in-flight provision (clone + nix + seeded devShell) finishes well
    // under this ceiling. (`provisioning` rows don't heartbeat, so age = created_at.)
    const PROVISION_STALE_SECS: i64 = 20 * 60;
    let mut provisioning = 0usize;
    for s in spares.iter().filter(|s| s.state == "provisioning") {
        if now - s.created_at >= PROVISION_STALE_SECS {
            superzej_core::msg::warn(&format!(
                "warm pool: clearing stale provisioning spare {} (orphaned by a prior \
                 session or hung) so the pool refills",
                s.sandbox_name
            ));
            let _ = crate::provision_gate::destroy_spare(cfg, env_name, &s.sandbox_name);
            notify();
        } else {
            provisioning += 1;
        }
    }
    let ready: Vec<(String, u64)> = spares
        .iter()
        .filter(|s| s.state == "ready")
        .map(|s| (s.sandbox_name.clone(), (now - s.updated_at).max(0) as u64))
        .collect();

    let mut action = decide_pool(target, provisioning, &ready, max_idle);
    // Only a CONFIGURED provider env can host spares — for anything else
    // (notably the implicit "default" env) provisioning "succeeds" instantly as
    // a no-op, minting a phantom `ready` spare whose claim then skips the real
    // worktree provision (the bare-sprite-shell bug). Creates are refused;
    // destroys + the stale sweep above still run so leftover rows drain.
    if !crate::provision_gate::poolable_env(cfg, env_name) {
        action.create = 0;
    }
    for name in &action.destroy {
        let _ = crate::provision_gate::destroy_spare(cfg, env_name, name);
        notify();
    }
    for _ in 0..action.create {
        let cfg = cfg.clone();
        let repo_root = repo_root.to_path_buf();
        let env_name = env_name.to_string();
        let notify = notify.clone();
        // Concurrent build: provision_spare inserts a `provisioning` row first
        // (so this tick + the next don't double-count), then builds + checkpoints.
        std::thread::spawn(move || {
            match crate::provision_gate::provision_spare(&cfg, &repo_root, &env_name, |_| {}) {
                Ok(name) => {
                    tracing::debug!(target: "szhost::lifecycle", %name, "warm spare ready");
                    notify();
                }
                Err(e) => {
                    superzej_core::msg::warn(&format!("warm pool: spare provision failed: {e}"))
                }
            }
        });
    }
    notify();
}
