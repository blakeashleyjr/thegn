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
use superzej_core::db::PoolSpare;
use superzej_core::lifecycle::{
    DestroyReason, PoolPolicy, ReadySpare, WarmBudget, WarmCandidate, WarmInputs, decide,
    decide_pool, recyclable,
};
use superzej_core::remote::GitLoc;
use superzej_core::store::{PoolStore, WorkspaceStore};
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

/// Hard ceiling on the resolved pool target — a runaway guard so no config typo or
/// stuck hotkey can spin up an unbounded fleet. Idle scale-to-zero spares are
/// ~free, but PROVISIONING them is not (compute + the subscription's active-sprite
/// cap), so the pool is bounded no matter how it was configured.
pub const POOL_TARGET_CEILING: usize = 8;

/// The effective warm-pool target for `(repo, env)`, resolved in precedence order
/// and clamped to [`POOL_TARGET_CEILING`]:
///
/// 1. An explicit runtime override in the DB (the `+`/`-` hotkey, per repo+env) —
///    including a deliberate `0` to turn the pool off.
/// 2. An explicit `[lifecycle.pool] size`.
/// 3. **Auto (scale-to-zero):** an env whose provider hibernates for free
///    (`EnvProviderConfig::scale_to_zero`) AND that the user already opted to pay
///    into (`auto_provision`) parks ONE idle spare, so the first open is instant.
///    Gated on `auto_provision` so merely *configuring* a sprites env never spends;
///    never applied to a billed-while-stopped VPS.
/// 4. Otherwise off (`0`).
pub fn effective_pool_target(
    db: &superzej_core::db::Db,
    cfg: &Config,
    repo: &str,
    env: &str,
) -> usize {
    let target = if let Some(t) = db.pool_target(repo, env).ok().flatten() {
        t.max(0) as usize
    } else if cfg.lifecycle.pool.size > 0 {
        cfg.lifecycle.pool.size
    } else if cfg
        .env
        .get(env)
        .is_some_and(|e| e.provider.scale_to_zero() && e.provider.auto_provision)
    {
        1
    } else {
        0
    };
    target.min(POOL_TARGET_CEILING)
}

/// Immediately refill the pool after a claim consumed a spare: re-run the
/// reconcile for `worktree`'s `(repo, env)` right now instead of waiting for the
/// next ~8s maintainer tick, so a parked spare is ready again for the next open.
/// Runs on its own thread (DB + provider work) and is a no-op for a local
/// worktree. The UI pool chip refreshes on the next maintainer tick (the refill
/// itself is a minutes-long provision), so no waker is threaded here.
pub fn refill_pool_after_claim(cfg: &Config, worktree: &str) {
    let cfg = cfg.clone();
    let worktree = worktree.to_string();
    std::thread::spawn(move || {
        let loc = GitLoc::for_worktree(Path::new(&worktree));
        if !loc.is_remote() {
            return;
        }
        let Ok(db) = superzej_core::db::Db::open() else {
            return;
        };
        let (repo_root, _repo, env_name) = pool_context(&db, &cfg, &worktree, &loc);
        reconcile_pool(&cfg, &repo_root, &env_name, || {});
    });
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
    // Idle policy by provider billing model: a scale-to-zero provider (sprites)
    // parks idle spares for free — aging them out just discards provisioning for
    // $0 — while a billed-when-stopped provider (VPS) must age them out to reclaim
    // spend. Unknown/unconfigured ⇒ the safe AgeOut default (never leak).
    let policy = if cfg
        .env
        .get(env_name)
        .is_some_and(|e| e.provider.scale_to_zero())
    {
        PoolPolicy::ParkIdle
    } else {
        PoolPolicy::AgeOut
    };
    // The current lockfile: keys both the per-spare staleness flag below and the
    // recycle-vs-destroy decision in the destroy loop.
    let current_lock = crate::provision_gate::flake_lock_hash(repo_root);
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
    let ready: Vec<ReadySpare> = spares
        .iter()
        .filter(|s| s.state == "ready")
        .map(|s| ReadySpare {
            name: s.sandbox_name.clone(),
            idle_secs: (now - s.updated_at).max(0) as u64,
            // A ready spare whose base was built against a different flake.lock
            // than the repo's current one must rotate even while parked — else a
            // ParkIdle pool serves a stale toolchain forever. Only meaningful once
            // the repo HAS a lockfile (empty current ⇒ nothing to compare).
            lock_stale: !current_lock.is_empty()
                && s.lock_hash.as_deref().unwrap_or("") != current_lock,
        })
        .collect();

    let mut action = decide_pool(target, provisioning, &ready, policy, max_idle);
    // Only a CONFIGURED provider env can host spares — for anything else
    // (notably the implicit "default" env) provisioning "succeeds" instantly as
    // a no-op, minting a phantom `ready` spare whose claim then skips the real
    // worktree provision (the bare-sprite-shell bug). Creates are refused;
    // destroys + the stale sweep above still run so leftover rows drain.
    if !crate::provision_gate::poolable_env(cfg, env_name) {
        action.create = 0;
    }
    for (name, reason) in &action.destroy {
        // A STALE spare (aged past max_idle) whose provisioned-base checkpoint is
        // still fresh (same flake.lock) is RECYCLED: an in-place restore resets it
        // to the pristine provisioned state + resets its idle clock — no destroy,
        // no minutes-long rebuild. Over-TARGET spares are genuinely surplus
        // (recycling keeps them over target), so those always destroy; so does a
        // stale spare whose restore fails or that has no usable checkpoint.
        let recycled = *reason == DestroyReason::Stale
            && spares
                .iter()
                .find(|s| &s.sandbox_name == name)
                .is_some_and(|s| recycle_spare(cfg, env_name, s, &current_lock));
        if !recycled {
            let _ = crate::provision_gate::destroy_spare(cfg, env_name, name);
        }
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

/// Ceiling for one in-place restore (a Sprites checkpoint restore is ~seconds;
/// this only bounds a hung request so the reconcile/delete thread can't stall).
const RESTORE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// Recycle one spare by restoring it in place from its provisioned-base
/// checkpoint, then mark its row `ready` again (checkpoint kept, idle clock
/// reset). `false` when the spare isn't recyclable (no checkpoint / lock-hash
/// drift / provider can't) or the restore fails — the caller destroys instead.
fn recycle_spare(cfg: &Config, env_name: &str, spare: &PoolSpare, current_lock: &str) -> bool {
    // The kill-switch: `[lifecycle.pool] recycle = false` restores the
    // always-destroy behavior on every path (reconcile-stale AND
    // worktree-delete both route through here).
    if !cfg.lifecycle.pool.recycle {
        return false;
    }
    if !recyclable(
        spare.checkpoint_id.as_deref(),
        spare.lock_hash.as_deref().unwrap_or(""),
        current_lock,
    ) {
        return false;
    }
    let name = spare.sandbox_name.as_str();
    let cp = spare.checkpoint_id.as_deref().unwrap_or_default();
    let Some(env) = cfg.env.get(env_name) else {
        return false;
    };
    let Some(provider) = crate::agent::provider_for_named(&env.provider, name) else {
        return false;
    };
    if !provider.caps().checkpoints {
        return false;
    }
    // Verify the checkpoint still EXISTS before trusting a restore: the Sprites
    // restore endpoint returns success even for an unknown checkpoint id, so a
    // provider-GC'd checkpoint whose id lingers in our row would otherwise
    // "restore" into an unknown state. If listing fails (transient) we proceed —
    // the restore + post-claim exec are the backstop.
    let cp_present =
        crate::agent::block_on_provider(|| async { provider.list_checkpoints(name).await })
            .map(|cps| cps.iter().any(|c| c.id == cp))
            .unwrap_or(true);
    if !cp_present {
        superzej_core::msg::warn(&format!(
            "warm pool: checkpoint {cp} for {name} no longer exists; destroying instead of recycling"
        ));
        return false;
    }
    let restored = crate::agent::block_on_provider(|| async {
        tokio::time::timeout(RESTORE_TIMEOUT, provider.restore(name, cp))
            .await
            .unwrap_or_else(|_| {
                Err(anyhow::anyhow!(
                    "restore timed out after {}s",
                    RESTORE_TIMEOUT.as_secs()
                ))
            })
    });
    match restored {
        Ok(()) => {
            // Re-assert the provisioned marker: it is written AFTER the
            // checkpoint step (agent.rs), so the checkpoint a restore reverts
            // to does NOT contain it — without this, a claiming worktree would
            // see no marker and needlessly re-provision, negating the recycle.
            // Best-effort re-assert of the provisioned marker (it is written
            // post-checkpoint in agent.rs, so a restore reverts to a fs without
            // it). Not load-bearing for correctness: the CLAIM path skips
            // provisioning outright, so at worst the eager path re-provisions
            // once, idempotently. A cold just-restored sprite may reject this
            // write; that is fine.
            let workdir = env.provider.sync_workdir();
            let marker = superzej_core::envplan::EnvPlan::marker_path(&workdir);
            let _ = crate::agent::block_on_provider(|| async {
                provider.write(name, &marker, b"ok\n").await
            });
            if let Ok(db) = superzej_core::db::Db::open() {
                // best-effort: the DB is a cache; a miss just means the spare is
                // re-observed (and maybe destroyed) on a later reconcile tick.
                let _ = db.set_pool_spare_ready(name, Some(cp), current_lock);
            }
            tracing::debug!(
                target: "szhost::lifecycle",
                %name, checkpoint = %cp,
                "recycled spare via restore-in-place"
            );
            true
        }
        Err(e) => {
            superzej_core::msg::warn(&format!(
                "warm pool: restore-in-place of {name} failed ({e}); destroying it instead"
            ));
            false
        }
    }
}

/// Pure eligibility for the worktree-delete recycle: the sandbox must be a
/// CLAIMED pool spare of this env whose provisioned-base checkpoint is still
/// fresh against the current `flake.lock`. Returns the checkpoint to restore.
fn claimed_recycle_checkpoint(
    spare: &PoolSpare,
    env_name: &str,
    current_lock: &str,
) -> Option<String> {
    (spare.state == "claimed"
        && spare.env_name == env_name
        && recyclable(
            spare.checkpoint_id.as_deref(),
            spare.lock_hash.as_deref().unwrap_or(""),
            current_lock,
        ))
    .then(|| spare.checkpoint_id.clone().unwrap_or_default())
}

/// Worktree-delete path: when the deleted worktree's sandbox is a CLAIMED pool
/// spare with a fresh provisioned-base checkpoint, restore it in place and
/// return it to the pool (`ready`) instead of destroying it — the next worktree
/// claims it instantly. Returns whether it was recycled (the caller skips the
/// destroy). The deleted worktree's row/binding is dropped by the delete flow
/// itself (`forget_worktree_group`), so no explicit unbind is needed here. On a
/// non-recyclable row or a failed restore the phantom `claimed` row is dropped
/// so it can't linger after the caller destroys the sandbox.
pub fn recycle_claimed_on_delete(cfg: &Config, env_name: &str, name: &str) -> bool {
    let Ok(db) = superzej_core::db::Db::open() else {
        return false;
    };
    let Ok(Some(spare)) = db.pool_spare_by_name(name) else {
        return false; // not a pool spare: a derived per-worktree sandbox
    };
    // The row's own repo_path (recorded at mint time) keys the lock hash — the
    // worktree's rows/files are already being torn down concurrently.
    let current_lock = crate::provision_gate::flake_lock_hash(Path::new(&spare.repo_path));
    if claimed_recycle_checkpoint(&spare, env_name, &current_lock).is_some()
        && recycle_spare(cfg, env_name, &spare, &current_lock)
    {
        return true;
    }
    // best-effort: drop the stale claimed row so it never lingers as a phantom
    // spare; the caller proceeds to destroy the sandbox itself.
    let _ = db.delete_pool_spare(name);
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use superzej_core::db::Db;

    /// The DB row transitions around a recycle: ready → claimed → (restore
    /// succeeds) → ready again with the SAME checkpoint + a fresh clock, and the
    /// recycled spare is claimable again. The provider restore itself is a
    /// network seam (not unit-testable); this locks the bookkeeping around it.
    #[test]
    fn recycle_bookkeeping_returns_claimed_spare_to_ready() {
        let db = Db::open_memory().unwrap();
        db.insert_pool_spare("repo-pool-1", "/repo", "sprites")
            .unwrap();
        db.set_pool_spare_ready("repo-pool-1", Some("cp-1"), "lock-a")
            .unwrap();
        let (name, cp) = db
            .claim_pool_spare("/repo", "sprites", "/wt/x")
            .unwrap()
            .unwrap();
        assert_eq!(name, "repo-pool-1");
        assert_eq!(cp.as_deref(), Some("cp-1"));

        let spare = db.pool_spare_by_name(&name).unwrap().unwrap();
        assert_eq!(spare.state, "claimed");
        // Eligible: claimed row, matching env + lock hash.
        assert_eq!(
            claimed_recycle_checkpoint(&spare, "sprites", "lock-a").as_deref(),
            Some("cp-1")
        );
        // Not eligible: wrong env, drifted lock, or a ready (unclaimed) row.
        assert!(claimed_recycle_checkpoint(&spare, "other", "lock-a").is_none());
        assert!(claimed_recycle_checkpoint(&spare, "sprites", "lock-b").is_none());

        // What recycle_spare does after a successful restore:
        db.set_pool_spare_ready(&name, Some("cp-1"), "lock-a")
            .unwrap();
        let spare = db.pool_spare_by_name(&name).unwrap().unwrap();
        assert_eq!(spare.state, "ready");
        assert_eq!(spare.checkpoint_id.as_deref(), Some("cp-1"), "id kept");
        // …and it is claimable again.
        let again = db
            .claim_pool_spare("/repo", "sprites", "/wt/y")
            .unwrap()
            .unwrap();
        assert_eq!(again.0, "repo-pool-1");
    }

    /// The pre-warm defaults: a scale-to-zero env the user pays into (auto_provision)
    /// auto-parks one idle spare; a bare config, a non-auto env, or a VPS stays off;
    /// explicit config/hotkey wins; the ceiling clamps a runaway value.
    #[test]
    fn pool_target_auto_defaults_for_scale_to_zero_and_clamps() {
        use superzej_core::config::{EnvConfig, EnvProviderConfig, PlacementMode};
        let db = Db::open_memory().unwrap();
        let mut cfg = Config::default();
        let sprites = |auto: bool| EnvConfig {
            placement: PlacementMode::Provider,
            provider: EnvProviderConfig {
                provider: "sprites".into(),
                auto_provision: auto,
                ..Default::default()
            },
            ..Default::default()
        };

        // No env table at all ⇒ off.
        assert_eq!(effective_pool_target(&db, &cfg, "/repo", "sprites"), 0);
        // Scale-to-zero + auto_provision ⇒ auto-park one.
        cfg.env.insert("sprites".into(), sprites(true));
        assert_eq!(effective_pool_target(&db, &cfg, "/repo", "sprites"), 1);
        // …but without auto_provision ⇒ off (no surprise spend from mere config).
        cfg.env.insert("sprites".into(), sprites(false));
        assert_eq!(effective_pool_target(&db, &cfg, "/repo", "sprites"), 0);
        // VPS (not scale-to-zero) even with auto_provision ⇒ off (billed when stopped).
        cfg.env.insert(
            "vps".into(),
            EnvConfig {
                placement: PlacementMode::Provider,
                provider: EnvProviderConfig {
                    provider: "hetzner".into(),
                    auto_provision: true,
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        assert_eq!(effective_pool_target(&db, &cfg, "/repo", "vps"), 0);
        // Explicit config size wins over the auto default…
        cfg.env.insert("sprites".into(), sprites(true));
        cfg.lifecycle.pool.size = 3;
        assert_eq!(effective_pool_target(&db, &cfg, "/repo", "sprites"), 3);
        // …and the ceiling clamps a runaway value.
        cfg.lifecycle.pool.size = 999;
        assert_eq!(
            effective_pool_target(&db, &cfg, "/repo", "sprites"),
            POOL_TARGET_CEILING
        );
    }

    /// A spare that never captured a checkpoint (or predates S1) is not
    /// recyclable — the delete path must fall back to destroy + row cleanup.
    #[test]
    fn recycle_kill_switch_short_circuits_before_any_provider_work() {
        let mut cfg = Config::default();
        cfg.lifecycle.pool.recycle = false;
        let spare = superzej_core::db::PoolSpare {
            sandbox_name: "repo-pool-abc".into(),
            repo_path: "/repo".into(),
            env_name: "sprites".into(),
            state: "ready".into(),
            checkpoint_id: Some("cp-1".into()),
            lock_hash: Some("lock".into()),
            created_at: 0,
            updated_at: 0,
        };
        assert!(
            !recycle_spare(&cfg, "sprites", &spare, "lock"),
            "recycle=false must fall back to destroy even for a fresh checkpoint"
        );
    }

    /// The VPS pool policy: a VPS cannot checkpoint (no suspend — a powered-off
    /// instance still bills), so its spares record `checkpoint_id = None` and
    /// every recycle path must fall through to DESTROY. Locks both gates: the
    /// pure `recyclable()` input (no checkpoint) and `recycle_spare`'s
    /// caps().checkpoints refusal even if an id were somehow present.
    #[test]
    fn vps_spares_destroy_instead_of_recycling() {
        let mut cfg = Config::default();
        cfg.env.insert(
            "hetzner".into(),
            superzej_core::config::EnvConfig {
                placement: superzej_core::config::PlacementMode::Provider,
                provider: superzej_core::config::EnvProviderConfig {
                    provider: "hetzner".into(),
                    // Token env deliberately unset in tests: provider_for_named
                    // is None ⇒ recycle refuses before any network.
                    api_key_env: "SZ_TEST_NO_SUCH_HCLOUD_TOKEN".into(),
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        // The real shape: no checkpoint ⇒ not recyclable at the pure gate.
        let spare = superzej_core::db::PoolSpare {
            sandbox_name: "repo-pool-vps".into(),
            repo_path: "/repo".into(),
            env_name: "hetzner".into(),
            state: "ready".into(),
            checkpoint_id: None,
            lock_hash: Some("lock".into()),
            created_at: 0,
            updated_at: 0,
        };
        assert!(!recycle_spare(&cfg, "hetzner", &spare, "lock"));
        assert!(claimed_recycle_checkpoint(&spare, "hetzner", "lock").is_none());
        // Even a (hypothetical) checkpoint id refuses at the provider gate.
        let with_cp = superzej_core::db::PoolSpare {
            checkpoint_id: Some("bogus".into()),
            ..spare
        };
        assert!(!recycle_spare(&cfg, "hetzner", &with_cp, "lock"));
    }

    #[test]
    fn spare_without_checkpoint_is_not_recyclable() {
        let db = Db::open_memory().unwrap();
        db.insert_pool_spare("repo-pool-2", "/repo", "sprites")
            .unwrap();
        db.set_pool_spare_ready("repo-pool-2", None, "lock-a")
            .unwrap();
        db.claim_pool_spare("/repo", "sprites", "/wt/z").unwrap();
        let spare = db.pool_spare_by_name("repo-pool-2").unwrap().unwrap();
        assert!(claimed_recycle_checkpoint(&spare, "sprites", "lock-a").is_none());
    }
}

/// Live recycle verification against the REAL Sprites API. `#[ignore]` — needs
/// `SPRITES_TOKEN` + network and creates/destroys throwaway sprites (real
/// spend). Run serially via `just sprites-live-recycle`. Each test isolates
/// `XDG_STATE_HOME` (EnvVarGuard holds the crate env lock for its duration)
/// and audits the provider's sprite list before/after — zero leaks.
#[cfg(test)]
// Human-run live integration tests: eprintln progress + a git subprocess to
// build the scratch repo are fine here (the bans target the shipping binary).
#[allow(clippy::disallowed_macros, clippy::disallowed_methods)]
mod live_recycle {
    use super::*;

    fn token() -> Option<String> {
        std::env::var("SPRITES_TOKEN")
            .ok()
            .filter(|t| !t.is_empty())
    }

    /// A scratch repo whose origin the SPRITE can clone (public, tiny).
    fn scratch_repo(dir: &std::path::Path) -> std::path::PathBuf {
        let repo = dir.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        // A flake.lock gives the recycle staleness key a stable non-empty hash
        // (recycle engages for flake-lock repos; see recyclable()).
        std::fs::write(repo.join("flake.lock"), r#"{"nodes":{},"version":7}"#).unwrap();
        for args in [
            vec!["init", "-q"],
            vec![
                "remote",
                "add",
                "origin",
                "https://github.com/octocat/Hello-World",
            ],
        ] {
            let ok = superzej_core::util::git_cmd(&repo)
                .args(&args)
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            assert!(ok, "git {args:?}");
        }
        repo
    }

    fn live_cfg() -> Config {
        // Clean strategy + no devshell warm ⇒ the plan is workspace/git_auth/
        // clone/checkpoint — minutes, not tens of minutes.
        toml::from_str(
            r#"
            [env.szlive]
            placement = "provider"
            [env.szlive.provider]
            provider = "sprites"
            api_key_env = "SPRITES_TOKEN"
            auto_provision = true
            auto_checkpoint = true
            skip_devshell_warm = true
            [sandbox.home]
            strategy = "clean"
            "#,
        )
        .unwrap()
    }

    /// Destroy-on-drop so a failed assertion never leaks a paid sprite.
    struct SpareGuard {
        cfg: Config,
        name: String,
        done: bool,
    }
    impl Drop for SpareGuard {
        fn drop(&mut self) {
            if !self.done {
                let _ = crate::provision_gate::destroy_spare(&self.cfg, "szlive", &self.name);
            }
        }
    }

    fn sprite_names(token: &str) -> Vec<String> {
        let p = superzej_svc::provider::SpritesProvider::new("", token, "unused");
        use superzej_svc::provider::RemoteProvider;
        crate::agent::block_on_provider(|| async { p.list().await }).unwrap_or_default()
    }

    fn spare_row(name: &str) -> superzej_core::db::PoolSpare {
        superzej_core::db::Db::open()
            .unwrap()
            .pool_spare_by_name(name)
            .unwrap()
            .expect("spare row exists")
    }

    #[test]
    #[ignore = "live: needs SPRITES_TOKEN, network, creates a real sprite"]
    fn live_spare_checkpoint_capture_and_stale_recycle() {
        let Some(tok) = token() else {
            eprintln!("SPRITES_TOKEN unset — skipping");
            return;
        };
        let tmp = std::env::temp_dir().join(format!("sz-live-recycle-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let _env = crate::testenv::EnvVarGuard::set(&[(
            "XDG_STATE_HOME",
            tmp.join("state").to_str().unwrap(),
        )]);
        let before = sprite_names(&tok);
        let cfg = live_cfg();
        let repo = scratch_repo(&tmp);

        // S1 proof: provisioning a spare persists its checkpoint id.
        let t0 = std::time::Instant::now();
        let name = crate::provision_gate::provision_spare(&cfg, &repo, "szlive", |_| {})
            .expect("provision spare");
        let mut guard = SpareGuard {
            cfg: cfg.clone(),
            name: name.clone(),
            done: false,
        };
        eprintln!("spare {name} provisioned in {:?}", t0.elapsed());
        let row = spare_row(&name);
        let cp = row
            .checkpoint_id
            .clone()
            .expect("checkpoint id captured (S1)");
        assert_eq!(row.state, "ready");
        let db = superzej_core::db::Db::open().unwrap();
        let base = db
            .base_snapshot(&repo.to_string_lossy(), "szlive")
            .unwrap()
            .expect("env_base_snapshots row (S1)");
        assert_eq!(base.0, cp, "same checkpoint in both tables");

        // S2 proof: a stale spare recycles via restore-in-place, fast.
        let current_lock = crate::provision_gate::flake_lock_hash(&repo);
        let t1 = std::time::Instant::now();
        assert!(
            recycle_spare(&cfg, "szlive", &row, &current_lock),
            "recycle must restore in place"
        );
        let restore_took = t1.elapsed();
        eprintln!("recycled in {restore_took:?}");
        assert!(
            restore_took < std::time::Duration::from_secs(120),
            "restore should be seconds, took {restore_took:?}"
        );
        let row2 = spare_row(&name);
        assert_eq!(row2.state, "ready");
        assert_eq!(row2.checkpoint_id.as_deref(), Some(cp.as_str()));

        // Post-restore the sprite must exec (warm-retry past cold start) and
        // still carry the provisioned fs: the marker (re-asserted after
        // restore, since it is written post-checkpoint) + the clone.
        let p = superzej_svc::provider::SpritesProvider::new("", &tok, &name);
        let marker = superzej_core::envplan::EnvPlan::marker_path("/workspace");
        let argv: Vec<String> = vec![
            "/bin/sh".into(),
            "-lc".into(),
            format!(
                "test -f {marker} && echo MARKER; test -d /workspace/.git && echo GIT; echo DONE"
            ),
        ];
        let mut out = String::new();
        for _ in 0..30 {
            match crate::agent::block_on_provider(|| async {
                p.run_exec(&name, &argv, None, &[]).await
            }) {
                Ok((_, o)) if o.contains("DONE") => {
                    out = o;
                    break;
                }
                _ => std::thread::sleep(std::time::Duration::from_secs(2)),
            }
        }
        eprintln!("post-restore fs check:\n{out}");
        // GIT is the load-bearing property: the checkpoint restore preserves the
        // full provisioned filesystem. (The .superzej-provisioned marker is
        // written post-checkpoint, so it is NOT in the checkpoint; the claim path
        // skips provisioning regardless, so its absence only costs an idempotent
        // eager re-provision — a documented follow-up, not asserted here.)
        assert!(out.contains("GIT"), "clone survives restore: {out}");
        if !out.contains("MARKER") {
            eprintln!(
                "note: provisioned marker absent post-restore (written post-checkpoint; \
                 claim path skips provisioning so this is a known efficiency follow-up)"
            );
        }

        crate::provision_gate::destroy_spare(&cfg, "szlive", &name).expect("destroy");
        guard.done = true;
        assert_eq!(sprite_names(&tok), before, "zero leaked sprites");
    }

    #[test]
    #[ignore = "live: needs SPRITES_TOKEN, network, creates a real sprite"]
    fn live_claimed_delete_recycle_round_trip() {
        let Some(tok) = token() else {
            eprintln!("SPRITES_TOKEN unset — skipping");
            return;
        };
        let tmp = std::env::temp_dir().join(format!("sz-live-claim-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let _env = crate::testenv::EnvVarGuard::set(&[(
            "XDG_STATE_HOME",
            tmp.join("state").to_str().unwrap(),
        )]);
        let before = sprite_names(&tok);
        let cfg = live_cfg();
        let repo = scratch_repo(&tmp);
        let name = crate::provision_gate::provision_spare(&cfg, &repo, "szlive", |_| {})
            .expect("provision spare");
        let mut guard = SpareGuard {
            cfg: cfg.clone(),
            name: name.clone(),
            done: false,
        };

        // Claim it for a fake worktree (bind only; no branch settle needed).
        let db = superzej_core::db::Db::open().unwrap();
        let wt = tmp.join("wt").to_string_lossy().into_owned();
        db.put_worktree(
            "live-claim",
            &repo.to_string_lossy(),
            &wt,
            "sz/live",
            None,
            None,
        )
        .unwrap();
        let claimed = db
            .claim_pool_spare(&repo.to_string_lossy(), "szlive", &wt)
            .unwrap()
            .expect("claim");
        assert_eq!(claimed.0, name);
        assert_eq!(spare_row(&name).state, "claimed");

        // Delete-path recycle: back to ready, same sprite, re-claimable.
        assert!(
            recycle_claimed_on_delete(&cfg, "szlive", &name),
            "claimed spare with fresh checkpoint recycles on delete"
        );
        let row = spare_row(&name);
        assert_eq!(row.state, "ready");
        let again = db
            .claim_pool_spare(&repo.to_string_lossy(), "szlive", &wt)
            .unwrap()
            .expect("re-claim after recycle");
        assert_eq!(again.0, name, "restore→claim→restore round trip");

        crate::provision_gate::destroy_spare(&cfg, "szlive", &name).expect("destroy");
        guard.done = true;
        assert_eq!(sprite_names(&tok), before, "zero leaked sprites");
    }

    #[test]
    #[ignore = "live: needs SPRITES_TOKEN, network, creates a real sprite"]
    fn live_recycle_falls_back_to_destroy_on_bad_checkpoint() {
        let Some(tok) = token() else {
            eprintln!("SPRITES_TOKEN unset — skipping");
            return;
        };
        let tmp = std::env::temp_dir().join(format!("sz-live-fallback-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let _env = crate::testenv::EnvVarGuard::set(&[(
            "XDG_STATE_HOME",
            tmp.join("state").to_str().unwrap(),
        )]);
        let before = sprite_names(&tok);
        let cfg = live_cfg();
        let repo = scratch_repo(&tmp);
        let name = crate::provision_gate::provision_spare(&cfg, &repo, "szlive", |_| {})
            .expect("provision spare");
        let mut guard = SpareGuard {
            cfg: cfg.clone(),
            name: name.clone(),
            done: false,
        };

        // Corrupt the checkpoint id: restore must FAIL and recycle must report
        // false so the caller destroys (no wedged row, no leaked sprite).
        let db = superzej_core::db::Db::open().unwrap();
        let lock = crate::provision_gate::flake_lock_hash(&repo);
        db.set_pool_spare_ready(&name, Some("cp-bogus-does-not-exist"), &lock)
            .unwrap();
        let row = spare_row(&name);
        assert!(
            !recycle_spare(&cfg, "szlive", &row, &lock),
            "bad checkpoint must fall back, not fake success"
        );

        crate::provision_gate::destroy_spare(&cfg, "szlive", &name).expect("destroy");
        guard.done = true;
        assert_eq!(sprite_names(&tok), before, "zero leaked sprites");
    }
}
